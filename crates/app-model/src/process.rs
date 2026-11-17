//! Hosting a process app: a package whose `[process]` program draws its
//! own view tree and speaks the app process protocol on its standard
//! input and output (`app-protocol`, `docs/app-protocol.md`).
//!
//! The host starts the program and supervises it:
//! - its view (a whole tree, then keyed patches) is drawn and published
//!   like any app pane's;
//! - input from the pane and from desktops goes to it as events;
//! - its API calls go to the daemon on the host's connection, so under
//!   the app's grant, and only for methods in its manifest's
//!   capabilities;
//! - its standard error is kept, the last lines of it;
//! - when it exits with a failure, the last view stays, marked stale,
//!   with why; `r` starts it again and `q` quits.
//!
//! All of the program's I/O runs on tasks of its own, so a program that
//! stops reading or writing never holds up drawing or input.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_protocol::{AppInfo, AppMessage, CallError, Event, HostMessage, MAX_LINE, PROTOCOL};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use view_tree::{Frame, Kind, Node};

use crate::manifest::{Manifest, ProcessSpec};
use crate::model::{App, Link};
use crate::run::{Conn, Endpoint, Input, connect, from_remote, host_trees, host_views, now_ms};

/// Messages held for a program that is not reading its input. Past this,
/// new ones are not queued and the status line says so.
const QUEUED: usize = 1024;
/// Lines of the program's standard error kept.
const TAIL: usize = 20;
/// How long a call may take (the API's own waits are at most 60 s).
const CALL_TIMEOUT: Duration = Duration::from_secs(70);
/// How long a program has to exit after it is asked to.
const GRACE: Duration = Duration::from_secs(1);

/// Methods only the host itself calls on its connection.
const HOST_ONLY: [&str; 4] = [
    "apps.register",
    "apps.publish",
    "apps.publish_tree",
    "grants.narrow",
];

/// Whether the app may call `method`: it is among `capabilities` (a
/// method, or `service.*`), and not one of the host's own.
pub fn allowed(capabilities: &[String], method: &str) -> Result<(), CallError> {
    if HOST_ONLY.contains(&method) {
        return Err(CallError {
            code: api::code::FORBIDDEN,
            message: format!("`{method}` is the app host's own"),
        });
    }
    let listed = capabilities.iter().any(|c| {
        c == method
            || c.strip_suffix(".*")
                .is_some_and(|s| method.strip_prefix(s).is_some_and(|r| r.starts_with('.')))
    });
    if listed {
        Ok(())
    } else {
        Err(CallError {
            code: api::code::FORBIDDEN,
            message: format!("`{method}` is not in this app's capabilities"),
        })
    }
}

/// What the program's output reader saw.
enum Out {
    Line(String),
    TooLong,
    End,
}

/// A running program: where its input goes, what it wrote, its standard
/// error, and itself.
struct Running {
    child: tokio::process::Child,
    to_app: Option<mpsc::Sender<String>>,
    out: mpsc::Receiver<Out>,
    tail: Arc<Mutex<VecDeque<String>>>,
    /// Reading its standard error, until it ends.
    said: tokio::task::JoinHandle<()>,
    hello: bool,
}

impl Running {
    /// Queue `m` for the program. False when it is not reading.
    fn send(&self, m: &HostMessage) -> bool {
        self.to_app
            .as_ref()
            .is_some_and(|t| t.try_send(app_protocol::line(m)).is_ok())
    }

    /// The last line of its standard error.
    fn last_said(&self) -> Option<String> {
        self.tail.lock().ok().and_then(|t| t.back().cloned())
    }

    /// Ask it to stop, close its input, and wait a moment for it; kill it
    /// after that.
    async fn stop(mut self) {
        self.send(&HostMessage::Quit);
        self.to_app = None;
        if tokio::time::timeout(GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// The program to run for `spec`, from the package directory `dir`.
fn program(dir: &Path, spec: &ProcessSpec) -> PathBuf {
    let prog = spec.command.first().map_or("", String::as_str);
    if prog.contains('/') && !Path::new(prog).is_absolute() {
        dir.join(prog)
    } else {
        PathBuf::from(prog)
    }
}

/// Start the program and its I/O tasks, and say hello.
fn start(dir: &Path, m: &Manifest, live: bool) -> Result<Running, String> {
    let spec = m.process.as_ref().ok_or("not a process app")?;
    let prog = program(dir, spec);
    let mut child = tokio::process::Command::new(&prog)
        .args(spec.command.iter().skip(1))
        .current_dir(dir)
        // The launch ticket is the host's; the program never sees it.
        .env_remove("DREAMER_LAUNCH_ID")
        .env_remove("DREAMER_LAUNCH_TICKET")
        .env("DREAMER_APP_PROTOCOL", PROTOCOL.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", prog.display()))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("the program's standard streams are missing".into());
    };
    let (to_app, mut queued) = mpsc::channel::<String>(QUEUED);
    tokio::spawn(async move {
        while let Some(line) = queued.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                return;
            }
        }
    });
    let (out_tx, out) = mpsc::channel::<Out>(64);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut buf = Vec::new();
            let read = (&mut reader)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)
                .await;
            let msg = match read {
                Ok(0) | Err(_) => Out::End,
                Ok(_) if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE => Out::TooLong,
                Ok(_) => Out::Line(String::from_utf8_lossy(&buf).trim_end().to_owned()),
            };
            let last = !matches!(msg, Out::Line(_));
            if out_tx.send(msg).await.is_err() || last {
                return;
            }
        }
    });
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let kept = tail.clone();
    let said = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line: String = line.chars().take(500).collect();
            if let Ok(mut t) = kept.lock() {
                if t.len() == TAIL {
                    t.pop_front();
                }
                t.push_back(line);
            }
        }
    });
    let running = Running {
        child,
        to_app: Some(to_app),
        out,
        tail,
        said,
        hello: false,
    };
    running.send(&HostMessage::Hello {
        protocol: PROTOCOL,
        app: AppInfo {
            id: m.id.clone(),
            version: m.version.clone(),
            title: m.title.clone(),
        },
        methods: m.methods(),
        live,
    });
    Ok(running)
}

/// What one line from the program does to the view.
enum Said {
    Nothing,
    /// The view changed.
    Drawn,
    /// Answer this call.
    Call(u64, String, Value),
    /// A broken exchange: stop the program, for this reason.
    Broken(String),
}

/// Take one line from the program: its hello, a view, a patch, or a call.
fn take_line(line: &str, running: &mut Running, view: &mut Option<Frame>) -> Said {
    if line.is_empty() {
        return Said::Nothing;
    }
    let msg: AppMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => return Said::Broken(format!("an unreadable message: {e}")),
    };
    if !running.hello {
        return match msg {
            AppMessage::Hello { protocol, .. } if protocol == PROTOCOL => {
                running.hello = true;
                Said::Nothing
            }
            AppMessage::Hello { protocol, .. } => Said::Broken(format!(
                "the app speaks protocol {protocol}; this host speaks {PROTOCOL}"
            )),
            _ => Said::Broken("the app did not answer the hello first".into()),
        };
    }
    match msg {
        AppMessage::Hello { .. } => Said::Broken("a second hello".into()),
        AppMessage::View { tree } => match tree.check() {
            Ok(()) => {
                *view = Some(tree);
                Said::Drawn
            }
            Err(e) => Said::Broken(format!("a bad tree: {e}")),
        },
        AppMessage::Patch { ops } => {
            let Some(mut next) = view.clone() else {
                running.send(&HostMessage::Resync {
                    reason: "no tree yet".into(),
                });
                return Said::Nothing;
            };
            match view_tree::apply(&mut next, &ops).and_then(|()| next.check()) {
                Ok(()) => {
                    *view = Some(next);
                    Said::Drawn
                }
                Err(reason) => {
                    running.send(&HostMessage::Resync { reason });
                    Said::Nothing
                }
            }
        }
        AppMessage::Call { id, method, params } => Said::Call(id, method, params),
    }
}

/// Call `method` on the daemon; the result as JSON, or the error.
async fn call(c: &client::Client, method: &str, params: Value) -> Result<Value, CallError> {
    let req = api::Request::from_json(method, params).map_err(|e| CallError {
        code: match e {
            api::CallError::UnknownMethod(_) => api::code::METHOD_NOT_FOUND,
            _ => api::code::INVALID_PARAMS,
        },
        message: e.to_string(),
    })?;
    match tokio::time::timeout(CALL_TIMEOUT, c.call(req)).await {
        Err(_) => Err(CallError {
            code: api::code::UNAVAILABLE,
            message: "no answer in time".into(),
        }),
        Ok(Ok(r)) => Ok(r.to_json()),
        Ok(Err(client::ClientError::Rpc(e))) => Err(CallError {
            code: e.code,
            message: e.message,
        }),
        Ok(Err(e)) => Err(CallError {
            code: api::code::UNAVAILABLE,
            message: e.to_string(),
        }),
    }
}

/// The input as an event for the program.
fn event(i: Input) -> Option<Event> {
    Some(match i {
        Input::Key(key) => Event::Key { key },
        Input::Text(text) => Event::Text { text },
        Input::Select { node, index } => Event::Select {
            node,
            index: index as u64,
        },
        Input::Scroll { node, steps } => Event::Scroll { node, steps },
        Input::Redraw | Input::Quit => return None,
    })
}

/// What is drawn before the program's first view.
fn waiting(m: &Manifest, what: &str) -> Frame {
    Frame {
        title: m.title.clone(),
        status: String::new(),
        focus: None,
        root: Node::new(
            "host",
            Kind::Text {
                text: what.to_owned(),
            },
        ),
    }
}

/// Run the process app at `path` against the daemon at `endpoint`. Input
/// from `inputs` and from desktops goes to the program; `publish` gets
/// every view to draw. Returns when the user quits (Ctrl-c, or `q` once
/// the program has stopped) or the program exits successfully.
pub async fn run_process(
    path: PathBuf,
    endpoint: Endpoint,
    inputs: &mut mpsc::UnboundedReceiver<Input>,
    publish: &mut dyn FnMut(&Frame) -> Result<(), String>,
) -> Result<(), String> {
    let (manifest, source) = crate::run::load_with_text(&path.display().to_string())?;
    let dir = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let methods = manifest.methods();
    // Not launched with a ticket: the connection would be the user's
    // own, so it gives that up down to the package's capabilities.
    let endpoint = Endpoint {
        narrow: endpoint.narrow || (endpoint.ticket && crate::run::ticket().is_none()),
        ..endpoint
    };
    let mut app = App::new(manifest.clone());
    let mut view: Option<Frame> = None;
    let mut stopped: Option<String> = None;
    let mut status: Option<String> = None;
    let mut running = match start(&dir, &manifest, false) {
        Ok(r) => Some(r),
        Err(e) => {
            stopped = Some(e);
            None
        }
    };
    let mut conn: Option<Conn> = None;
    let mut registered = false;
    let mut gave_up = false;
    let mut retry = tokio::time::Instant::now();
    let mut backoff = Duration::from_millis(500);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut hosting: Option<tokio::sync::watch::Sender<String>> = None;
    let mut hosting_tree: Option<tokio::sync::watch::Sender<Option<Frame>>> = None;
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(u64, Result<Value, CallError>)>();
    let mut dirty = true;
    loop {
        if dirty {
            let mut shown = view
                .clone()
                .unwrap_or_else(|| waiting(&manifest, "starting…"));
            if let Some(why) = &stopped {
                shown.status = format!("stale: {why}  | r restart  q quit");
            } else if let Some(s) = &status {
                shown.status = format!("{}  | {s}", shown.status);
            }
            publish(&shown)?;
            if let Some(h) = &hosting {
                h.send_replace(app.to_published(&source));
            }
            if let Some(h) = &hosting_tree {
                h.send_replace(Some(shown));
            }
            dirty = false;
        }
        let out = async {
            match running.as_mut() {
                Some(r) => r.out.recv().await,
                None => std::future::pending().await,
            }
        };
        let pushed = async {
            match conn.as_mut() {
                Some(c) => c.pushed.recv().await,
                None => std::future::pending().await,
            }
        };
        let mut lost: Option<String> = None;
        let mut input: Option<Input> = None;
        tokio::select! {
            i = inputs.recv() => match i {
                None | Some(Input::Quit) => break,
                Some(i) => input = Some(i),
            },
            o = out => match o {
                Some(Out::Line(line)) => {
                    let Some(r) = running.as_mut() else { continue };
                    match take_line(&line, r, &mut view) {
                        Said::Nothing => {}
                        Said::Drawn => dirty = true,
                        Said::Call(id, method, params) => {
                            let answer = allowed(&methods, &method).and_then(|()| {
                                conn.as_ref().map(|c| c.client.clone()).ok_or(CallError {
                                    code: api::code::UNAVAILABLE,
                                    message: "not connected to the daemon".into(),
                                })
                            });
                            match answer {
                                Err(error) => {
                                    r.send(&HostMessage::Result { id, result: None, error: Some(error) });
                                }
                                Ok(client) => {
                                    let tx = answers_tx.clone();
                                    tokio::spawn(async move {
                                        let _ = tx.send((id, call(&client, &method, params).await));
                                    });
                                }
                            }
                        }
                        Said::Broken(why) => {
                            if let Some(r) = running.take() {
                                r.stop().await;
                            }
                            stopped = Some(why);
                            dirty = true;
                        }
                    }
                }
                Some(Out::TooLong) => {
                    if let Some(r) = running.take() {
                        r.stop().await;
                    }
                    stopped = Some(format!("a message over {MAX_LINE} bytes"));
                    dirty = true;
                }
                Some(Out::End) | None => {
                    // Its output ended: it exits now, or is stopped.
                    let Some(mut r) = running.take() else { continue };
                    r.to_app = None;
                    let status = match tokio::time::timeout(GRACE, r.child.wait()).await {
                        Ok(Ok(s)) => Some(s),
                        _ => {
                            let _ = r.child.kill().await;
                            None
                        }
                    };
                    if status.is_some_and(|s| s.success()) {
                        return Ok(());
                    }
                    // What it said last, once its standard error is read.
                    let _ = tokio::time::timeout(GRACE, &mut r.said).await;
                    let what = match status.and_then(|s| s.code()) {
                        Some(code) => format!("the app exited with status {code}"),
                        None if status.is_some() => "the app was killed by a signal".to_owned(),
                        None => "the app closed its output".to_owned(),
                    };
                    stopped = Some(match r.last_said() {
                        Some(said) => format!("{what}: {said}"),
                        None => what,
                    });
                    app.link = Link::Stale(now_ms(), stopped.clone().unwrap_or_default());
                    dirty = true;
                }
            },
            a = answers.recv() => {
                if let (Some((id, answer)), Some(r)) = (a, running.as_ref()) {
                    let (result, error) = match answer {
                        Ok(v) => (Some(v), None),
                        Err(e) => (None, Some(e)),
                    };
                    r.send(&HostMessage::Result { id, result, error });
                }
            }
            p = pushed => match p {
                Some(client::Pushed::Event { topic, payload, .. }) => {
                    match api::Notification::decode(&topic, &payload) {
                        Ok(api::Notification::AppsInput(i)) => input = from_remote(&i),
                        Ok(n) => {
                            if let Some(r) = running.as_ref() {
                                r.send(&HostMessage::Notify { topic, payload: n.to_json() });
                            }
                        }
                        Err(_) => {}
                    }
                }
                Some(client::Pushed::Closed { error }) => {
                    lost = Some(error.unwrap_or_else(|| "the daemon closed the connection".into()));
                }
                Some(_) => {}
                None => lost = Some("the daemon closed the connection".into()),
            },
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                if conn.is_none() && !gave_up && now >= retry {
                    match connect(&endpoint, &mut registered, &[], &methods).await {
                        Ok(c) => {
                            if registered && hosting.is_none() {
                                hosting = Some(host_views(c.client.clone()));
                                hosting_tree = Some(host_trees(c.client.clone()));
                            }
                            conn = Some(c);
                            app.link = Link::Live;
                            backoff = Duration::from_millis(500);
                            if let Some(r) = running.as_ref() {
                                r.send(&HostMessage::Link { live: true, reason: None });
                            }
                            dirty = true;
                        }
                        Err(e) => {
                            gave_up = registered;
                            retry = now + backoff;
                            backoff = (backoff * 2).min(Duration::from_secs(5));
                            if !matches!(app.link, Link::Stale(..)) {
                                app.link = Link::Stale(now_ms(), e);
                                dirty = true;
                            }
                        }
                    }
                }
            }
        }
        if let Some(i) = input {
            dirty = true;
            if let Input::Key(k) = &i {
                // Ctrl-c is the host's: it always quits.
                if k == "Ctrl-c" {
                    break;
                }
                if running.is_none() {
                    match k.as_str() {
                        "q" | "Esc" => break,
                        "r" => match start(&dir, &manifest, conn.is_some()) {
                            Ok(r) => {
                                running = Some(r);
                                stopped = None;
                                status = None;
                                app.link = if conn.is_some() {
                                    Link::Live
                                } else {
                                    Link::Connecting
                                };
                            }
                            Err(e) => stopped = Some(e),
                        },
                        _ => {}
                    }
                    continue;
                }
            }
            if let (Some(e), Some(r)) = (event(i), running.as_ref()) {
                status = (!r.send(&HostMessage::Event { event: e }))
                    .then(|| "the app is not reading its input; input was not sent".to_owned());
            }
        }
        if let Some(why) = lost {
            conn = None;
            if let Some(r) = running.as_ref() {
                r.send(&HostMessage::Link {
                    live: false,
                    reason: Some(why.clone()),
                });
            }
            app.link = Link::Stale(now_ms(), why);
            retry = tokio::time::Instant::now() + backoff;
            dirty = true;
        }
    }
    if let Some(r) = running.take() {
        r.stop().await;
    }
    Ok(())
}

/// The process app at `path` as it first looks, with no daemon: its
/// program's first view, within five seconds.
pub async fn preview_process(path: &Path) -> Result<Frame, String> {
    let (manifest, _) = crate::run::load_with_text(&path.display().to_string())?;
    let dir = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut r = start(&dir, &manifest, false)?;
    let mut view = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        match tokio::time::timeout_at(deadline, r.out.recv()).await {
            Err(_) => break Err("the app drew no view within 5 s".to_owned()),
            Ok(Some(Out::Line(line))) => match take_line(&line, &mut r, &mut view) {
                Said::Drawn => break Ok(view.take().unwrap_or_else(|| waiting(&manifest, ""))),
                Said::Broken(why) => break Err(why),
                Said::Call(id, ..) => {
                    r.send(&HostMessage::Result {
                        id,
                        result: None,
                        error: Some(CallError {
                            code: api::code::UNAVAILABLE,
                            message: "a preview has no daemon".into(),
                        }),
                    });
                }
                Said::Nothing => {}
            },
            Ok(_) => {
                let _ = tokio::time::timeout(GRACE, &mut r.said).await;
                break Err(match r.last_said() {
                    Some(s) => format!("the app ended before drawing a view: {s}"),
                    None => "the app ended before drawing a view".to_owned(),
                });
            }
        }
    };
    r.stop().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_outside_the_capabilities_are_refused() {
        let caps = vec!["tmux.topology".to_owned(), "jobs.*".to_owned()];
        assert!(allowed(&caps, "tmux.topology").is_ok());
        assert!(allowed(&caps, "jobs.start").is_ok());
        let e = allowed(&caps, "actions.submit").expect_err("refused");
        assert_eq!(e.code, api::code::FORBIDDEN);
        assert!(allowed(&caps, "jobsx.start").is_err());
        // The host's own methods, even when listed.
        let all = vec!["apps.*".to_owned()];
        assert!(allowed(&all, "apps.publish_tree").is_err());
        assert!(allowed(&all, "apps.view").is_ok());
    }

    /// A package of a shell script that speaks the protocol, in a fresh
    /// directory.
    fn package(name: &str, script: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("drm-proc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("app.sh"), script).expect("script");
        std::fs::write(
            dir.join("app.toml"),
            r#"
schema_version = 2
id = "test.crash"
version = "0.1.0"
title = "Crash"
capabilities = ["process"]

[process]
command = ["/bin/sh", "app.sh"]
"#,
        )
        .expect("manifest");
        dir.join("app.toml")
    }

    /// No daemon: the host runs the program anyway, unconnected.
    fn nowhere() -> Endpoint {
        Endpoint {
            socket: std::env::temp_dir().join("drm-proc-no-daemon.sock"),
            role: "test",
            ticket: false,
            narrow: false,
        }
    }

    #[tokio::test]
    async fn a_failed_app_leaves_its_last_view_marked_stale() {
        let path = package(
            "crash",
            r#"read hello
echo '{"type":"hello","protocol":1}'
echo '{"type":"view","tree":{"title":"Crash","status":"ok","root":{"key":"m","type":"text","text":"before"}}}'
read event
echo "boom after $event" >&2
exit 3
"#,
        );
        let (tx, mut inputs) = mpsc::unbounded_channel();
        let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Frame>::new()));
        let seen = frames.clone();
        let mut publish = move |f: &Frame| {
            seen.borrow_mut().push(f.clone());
            Ok(())
        };
        async fn until(
            frames: &std::cell::RefCell<Vec<Frame>>,
            what: &str,
            ok: impl Fn(&Frame) -> bool,
        ) -> Frame {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(f) = frames.borrow().last().filter(|f| ok(f)) {
                    return f.clone();
                }
                assert!(std::time::Instant::now() < deadline, "never {what}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let drive = async {
            until(&frames, "drew", |f| {
                f.root.kind
                    == Kind::Text {
                        text: "before".into(),
                    }
            })
            .await;
            tx.send(Input::Key("a".into())).expect("send");
            let f = until(&frames, "went stale", |f| f.status.starts_with("stale: ")).await;
            // The last view, marked stale with the exit and what it said.
            assert_eq!(
                f.root.kind,
                Kind::Text {
                    text: "before".into()
                }
            );
            assert!(
                f.status
                    .contains("the app exited with status 3: boom after")
                    && f.status
                        .contains(r#"{"type":"event","event":{"kind":"key","key":"a"}}"#),
                "{}",
                f.status
            );
            tx.send(Input::Key("q".into())).expect("send");
        };
        let (ran, ()) = tokio::join!(
            run_process(path.clone(), nowhere(), &mut inputs, &mut publish),
            drive
        );
        ran.expect("quit");
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }
}
