//! `fetch` in a script app against a local HTTP server on 127.0.0.1: a
//! listed host answers, an unlisted one is refused before any connection
//! (a redirect to one too), and the size and time limits hold. Nothing
//! here reaches beyond the loopback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use app_model::{EngineOut, EngineStart, NetGrant};
use app_protocol::{AppInfo, AppMessage, Event, HostMessage, PROTOCOL};
use app_script::Limits;
use tokio::sync::mpsc;
use view_tree::Frame;

/// A server on 127.0.0.1 that answers by path; its port.
fn serve() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || answer(s, port));
        }
    });
    port
}

fn answer(s: TcpStream, port: u16) {
    let mut r = BufReader::new(s.try_clone().expect("clone"));
    let mut first = String::new();
    if r.read_line(&mut first).is_err() {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let (mut length, mut test) = (0usize, String::new());
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let (n, v) = line.split_once(':').unwrap_or(("", ""));
        match n.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = v.trim().parse().unwrap_or(0),
            "x-test" => test = v.trim().to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    let _ = r.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body);
    let mut w = s;
    let reply = |w: &mut TcpStream, status: &str, extra: &str, body: &str| {
        let _ = write!(
            w,
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
    };
    match path.as_str() {
        "/echo" => {
            let json = serde_json::json!({ "method": method, "body": body, "test": test });
            reply(&mut w, "200 OK", "x-served: yes\r\n", &json.to_string());
        }
        "/to-echo" => reply(&mut w, "302 Found", "location: /echo\r\n", ""),
        "/away" => reply(
            &mut w,
            "302 Found",
            &format!(
                "location: http://127.0.0.1:{}/echo\r\n",
                port.wrapping_add(1)
            ),
            "",
        ),
        "/big" => reply(&mut w, "200 OK", "", &"x".repeat(4096)),
        "/slow" => std::thread::sleep(Duration::from_secs(5)),
        _ => reply(&mut w, "404 Not Found", "", "{}"),
    }
}

/// The app: each key fetches something and shows what came of it.
fn app_js(port: u16) -> String {
    format!(
        r#"
const base = "http://127.0.0.1:{port}";
async function show(p) {{
  try {{
    const r = await p;
    const t = await r.text();
    return {{ out: r.status + (r.redirected ? " redirected" : "") + " " + t }};
  }} catch (e) {{
    return {{ out: e.name + ": " + e.message }};
  }}
}}
app({{
  state: {{ out: "" }},
  view(s) {{ return ui.text({{ key: "out", text: s.out }}); }},
  async onKey(k) {{
    switch (k) {{
      case "a": {{
        const r = await fetch(base + "/echo", {{
          method: "POST",
          headers: {{ "X-Test": "yes", "Content-Type": "text/plain" }},
          body: "ping",
        }});
        const j = await r.json();
        return {{ out: [r.status, r.ok, r.headers.get("x-served"), j.method, j.body, j.test].join(" ") }};
      }}
      case "b": return show(fetch(base + "/to-echo"));
      case "c": return show(fetch(base + "/away"));
      case "d": return show(fetch("http://unlisted.invalid/"));
      case "e": return show(fetch("http://127.0.0.1:{other}/echo"));
      case "f": return show(fetch(base + "/big"));
      case "g": return show(fetch(base + "/slow"));
      case "h": return show(fetch(base + "/to-echo", {{ redirect: "manual" }}));
    }}
  }},
}});
"#,
        other = port.wrapping_add(1),
    )
}

struct Host {
    to_app: mpsc::Sender<String>,
    from_app: mpsc::UnboundedReceiver<EngineOut>,
    view: Option<Frame>,
}

impl Host {
    /// The app, its manifest declaring `net.fetch` and `hosts`, with
    /// `net` as what its host learned of its grant.
    fn start(js: &str, hosts: &[String], net: NetGrant, limits: Limits) -> Host {
        let (to_app, input) = mpsc::channel(64);
        let (output, from_app) = mpsc::unbounded_channel();
        let app = AppInfo {
            id: "test.fetch".into(),
            version: "0.1.0".into(),
            title: "Fetch".into(),
        };
        let engine = app_script::engine_with(limits);
        engine(EngineStart {
            app: app.clone(),
            name: "app.ts".into(),
            js: js.into(),
            fixture: None,
            hosts: hosts.to_vec(),
            net,
            input,
            output,
        })
        .expect("start");
        let host = Host {
            to_app,
            from_app,
            view: None,
        };
        host.send(&HostMessage::Hello {
            protocol: PROTOCOL,
            app,
            methods: vec!["net.fetch".to_owned()],
            live: true,
        });
        host
    }

    fn send(&self, m: &HostMessage) {
        self.to_app
            .try_send(app_protocol::line(m))
            .expect("the app reads");
    }

    fn out(&self) -> String {
        self.view
            .as_ref()
            .and_then(|v| v.root.find("out"))
            .and_then(|n| n.kind.text())
            .unwrap_or("")
            .to_owned()
    }

    /// Press `key` and wait for what the app shows after it.
    async fn press(&mut self, key: &str) -> String {
        self.send(&HostMessage::Event {
            event: Event::Key { key: key.into() },
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let before = self.out();
        loop {
            let o = tokio::time::timeout_at(deadline, self.from_app.recv())
                .await
                .unwrap_or_else(|_| panic!("`{key}` never showed anything"));
            match o {
                Some(EngineOut::Line(l)) => match serde_json::from_str(&l).expect("a message") {
                    AppMessage::View { tree } => self.view = Some(tree),
                    AppMessage::Patch { ops } => {
                        let v = self.view.as_mut().expect("a view before a patch");
                        view_tree::apply(v, &ops).expect("the patch applies");
                    }
                    _ => {}
                },
                Some(EngineOut::Log(_)) => {}
                other => panic!("the app stopped: {other:?}"),
            }
            let now = self.out();
            if now != before && !now.is_empty() {
                return now;
            }
        }
    }
}

/// What the host learns when the daemon's grant holds `methods`.
fn granted(methods: &[&str]) -> NetGrant {
    let g = NetGrant::default();
    g.set(Some(&api::Grant {
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        ..api::Grant::default()
    }));
    g
}

fn limits() -> Limits {
    Limits {
        fetch_time: Duration::from_millis(500),
        fetch_size: 1024,
        ..Limits::default()
    }
}

#[tokio::test]
async fn a_listed_host_answers_and_nothing_else_is_reached() {
    let port = serve();
    let hosts = [format!("127.0.0.1:{port}")];
    let mut h = Host::start(&app_js(port), &hosts, granted(&["net.fetch"]), limits());
    // A POST with headers and a body; the JSON answer and its headers.
    assert_eq!(h.press("a").await, "200 true yes POST ping yes");
    // A redirect on the same host is followed.
    let r = h.press("b").await;
    assert!(r.starts_with("200 redirected {"), "{r}");
    // ... and with `redirect: "manual"` it is the answer.
    assert_eq!(h.press("h").await, "302 ");
    // A redirect to a port the manifest does not list is refused.
    let r = h.press("c").await;
    assert!(
        r.starts_with("TypeError: fetch: a redirect was refused")
            && r.contains("is not among the hosts this app's manifest lists"),
        "{r}"
    );
    // Unlisted hosts and ports are refused before any connection.
    let r = h.press("d").await;
    assert_eq!(
        r,
        "TypeError: fetch: unlisted.invalid:80 is not among the hosts this app's manifest lists ([net] hosts)"
    );
    let r = h.press("e").await;
    assert!(r.contains("is not among the hosts"), "{r}");
    // The limits: a body larger than the cap, a server that never answers.
    assert_eq!(
        h.press("f").await,
        "TypeError: fetch: the response is larger than 1024 bytes"
    );
    let started = std::time::Instant::now();
    assert_eq!(
        h.press("g").await,
        "TypeError: fetch: no complete answer within 500 ms"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    // The app is still running: the same request again.
    assert_eq!(h.press("a").await, "200 true yes POST ping yes");
}

#[tokio::test]
async fn a_launch_grant_without_net_fetch_refuses_it_whatever_the_manifest_declares() {
    let port = serve();
    let hosts = [format!("127.0.0.1:{port}")];
    // The manifest declares `net.fetch` and lists the host; the launch
    // granted less.
    let grant = granted(&["tmux.topology"]);
    let mut h = Host::start(&app_js(port), &hosts, grant.clone(), limits());
    assert_eq!(
        h.press("b").await,
        "TypeError: fetch: this app's grant does not include `net.fetch`"
    );
    // Before the host knows the grant, nothing is sent either.
    let unknown = NetGrant::default();
    let mut u = Host::start(&app_js(port), &hosts, unknown.clone(), limits());
    assert!(u.press("b").await.contains("grant is not known yet"));
    // The grant is read at each request: once the host learns a grant
    // with it, the same app fetches.
    unknown.set(Some(&api::Grant {
        methods: vec!["net.fetch".into()],
        ..api::Grant::default()
    }));
    assert_eq!(u.press("a").await, "200 true yes POST ping yes");
}
