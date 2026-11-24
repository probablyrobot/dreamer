//! Scripts through a real daemon: a loaded script reacts to
//! events by calling the API under its grant, a runaway is terminated,
//! reported, and restarted, and a bad reload keeps the last-good version.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-sc-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_TEST_SCRIPT_BACKOFF_MS", "100")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn tmux(&self, args: &[&str]) {
        self.tmux.run_checked(args).expect("tmux command");
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(c: &Client, r: Request) -> Result<Response, client::ClientError> {
    c.call(r).await
}

fn load(name: &str, source: &str, methods: &[&str], events: &[&str]) -> Request {
    Request::ScriptsLoad(api::ScriptLoad {
        name: name.into(),
        source: source.into(),
        grant: Some(api::ScriptGrant {
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            panes: Vec::new(),
            all_panes: true,
        }),
        events: events.iter().map(|e| (*e).to_owned()).collect(),
        limits: Some(api::ScriptLimits {
            call_ms: Some(30),
            ..api::ScriptLimits::default()
        }),
    })
}

async fn status(c: &Client, name: &str) -> api::ScriptStatus {
    let Ok(Response::ScriptsList(l)) = call(c, Request::ScriptsList(api::Empty {})).await else {
        panic!("list")
    };
    l.scripts
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no script {name}"))
}

/// The next message on a subscribed topic: (payload, sender instance).
async fn message(rx: &mut Receiver<Pushed>) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "messages.deliver" => {
                let e = api::MessageDelivery::decode(payload.as_slice())
                    .expect("delivery")
                    .envelope
                    .expect("envelope");
                return (
                    String::from_utf8_lossy(&e.payload).into_owned(),
                    e.sender.map(|s| s.instance).unwrap_or_default(),
                );
            }
            Ok(Some(_)) => {}
            other => panic!("no message: {other:?}"),
        }
    }
}

const WATCHER: &str = r#"
function on_event(e)
  dreamer.messages.publish({ topic = "script.seen", payload = base64.encode(e.kind) })
end
"#;

#[tokio::test]
async fn a_script_reacts_to_events_under_its_grant_and_reloads_atomically() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    call(&c, Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology");
    call(
        &c,
        Request::MessagesSubscribe(api::MessageSubscribe {
            topics: vec!["script.*".into()],
        }),
    )
    .await
    .expect("subscribe");
    let Response::ScriptsLoad(st) = call(
        &c,
        load(
            "watcher",
            WATCHER,
            &["events.subscribe", "messages.publish"],
            &["tmux"],
        ),
    )
    .await
    .expect("load") else {
        panic!("load")
    };
    assert_eq!(st.state, api::ScriptState::Running as i32);
    s.tmux(&["new-window", "-d", "-t", "main", "/bin/sh"]);
    let (payload, sender) = message(&mut rx).await;
    assert_eq!(payload, "tmux.topology");
    assert_eq!(sender, "script:watcher");

    // Outside its grant: a script that tries to stop the daemon does not
    // load, and the daemon keeps running.
    let e = call(
        &c,
        load(
            "nosy",
            "dreamer.daemon.shutdown()",
            &["messages.publish"],
            &[],
        ),
    )
    .await
    .expect_err("refused");
    assert!(e.to_string().contains("-32005"), "{e}");
    call(&c, Request::DaemonVersion(api::Empty {}))
        .await
        .expect("still running");
    // Receiving events needs events.subscribe in the grant.
    assert!(
        call(&c, load("deaf", WATCHER, &["messages.publish"], &["tmux"]))
            .await
            .is_err()
    );

    // A reload that does not compile or does not load leaves revision 1.
    let bad_syntax = Request::ScriptsReload(api::ScriptSource {
        name: "watcher".into(),
        source: "local = ".into(),
    });
    assert!(call(&c, bad_syntax).await.is_err());
    let failing = Request::ScriptsReload(api::ScriptSource {
        name: "watcher".into(),
        source: "error('boom')".into(),
    });
    let e = call(&c, failing).await.expect_err("does not load");
    assert!(e.to_string().contains("last-good"), "{e}");
    let st = status(&c, "watcher").await;
    assert_eq!(
        (st.revision, st.state),
        (1, api::ScriptState::Running as i32)
    );
    s.tmux(&["new-window", "-d", "-t", "main", "/bin/sh"]);
    assert_eq!(
        message(&mut rx).await.0,
        "tmux.topology",
        "the old version still runs"
    );
    // A good reload replaces it.
    let good = Request::ScriptsReload(api::ScriptSource {
        name: "watcher".into(),
        source: WATCHER.replace("e.kind", "'v2:' .. e.kind"),
    });
    let Response::ScriptsReload(st) = call(&c, good).await.expect("reload") else {
        panic!("reload")
    };
    assert_eq!(st.revision, 2);
    s.tmux(&["new-window", "-d", "-t", "main", "/bin/sh"]);
    assert_eq!(message(&mut rx).await.0, "v2:tmux.topology");

    let Response::ScriptsStop(st) = call(
        &c,
        Request::ScriptsStop(api::ScriptRef {
            name: "watcher".into(),
        }),
    )
    .await
    .expect("stop") else {
        panic!("stop")
    };
    assert_eq!(st.state, api::ScriptState::Stopped as i32);
    assert!(status_missing(&c, "watcher").await);
}

async fn status_missing(c: &Client, name: &str) -> bool {
    let Ok(Response::ScriptsList(l)) = call(c, Request::ScriptsList(api::Empty {})).await else {
        panic!("list")
    };
    !l.scripts.iter().any(|s| s.name == name)
}

#[tokio::test]
async fn a_runaway_script_is_terminated_reported_and_restarted() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    call(&c, Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology");
    call(
        &c,
        Request::EventsSubscribe(api::EventSubscribe {
            kinds: vec!["scripts".into()],
            ..api::EventSubscribe::default()
        }),
    )
    .await
    .expect("events");
    call(
        &c,
        load(
            "spin",
            "function on_event(e) while true do end end",
            &["events.subscribe"],
            &["tmux"],
        ),
    )
    .await
    .expect("load");
    s.tmux(&["new-window", "-d", "-t", "main", "/bin/sh"]);
    // The failure is an event in the log.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let failed = loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "events.batch" => {
                let b = api::EventBatch::decode(payload.as_slice()).expect("batch");
                if let Some(e) = b.events.into_iter().find(|e| e.kind == "scripts.failed") {
                    break e;
                }
            }
            Ok(Some(_)) => {}
            other => panic!("no scripts.failed event: {other:?}"),
        }
    };
    assert_eq!(failed.subject.as_deref(), Some("spin"));
    assert!(
        failed
            .state
            .as_deref()
            .unwrap_or_default()
            .contains("time budget")
    );
    // Started again from its source, and terminal work went on.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = status(&c, "spin").await;
        if st.restarts >= 1 && st.state == api::ScriptState::Running as i32 {
            assert!(st.last_error.unwrap_or_default().contains("time budget"));
            break;
        }
        assert!(Instant::now() < deadline, "never restarted: {st:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    call(&c, Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("the daemon answers");
}
