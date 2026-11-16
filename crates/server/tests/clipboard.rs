//! tmux's paste buffers and the clipboard: a copy made in tmux is reported
//! as a `clipboard.tmux_buffer` event (never with its text), its text is
//! read on request, and a buffer Dreamer writes reaches tmux without being
//! reported back.

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
        let root = PathBuf::from(format!("/tmp/drm-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        let tmux = TmuxServer::start(80, 20, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
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
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
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

/// The names of `clipboard.tmux_buffer` events seen within `wait`.
async fn buffer_events(rx: &mut Receiver<Pushed>, wait: Duration) -> Vec<(String, String)> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "events.batch" => {
                let b = api::EventBatch::decode(payload.as_slice()).expect("batch");
                for e in b.events {
                    if e.kind == "clipboard.tmux_buffer" {
                        out.push((
                            e.subject.clone().unwrap_or_default(),
                            e.state.clone().unwrap_or_default(),
                        ));
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return out,
        }
    }
}

async fn read(c: &Client, name: Option<String>) -> api::TmuxBuffer {
    match c
        .call(Request::ClipboardReadTmuxBuffer(api::TmuxBufferRead {
            endpoint: None,
            name,
        }))
        .await
        .expect("read")
    {
        Response::ClipboardReadTmuxBuffer(b) => b,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn tmux_buffers_and_the_clipboard_meet_in_both_directions() {
    let s = Setup::new();
    let mut c = s.client().await;
    // Connect the endpoint first, so tmux's notifications reach the daemon.
    let _ = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology");
    let Ok(Response::EventsSubscribe(_)) = c
        .call(Request::EventsSubscribe(api::EventSubscribe {
            kinds: vec!["clipboard.".into()],
            ..api::EventSubscribe::default()
        }))
        .await
    else {
        panic!("subscribe")
    };
    let mut rx = c.pushed().expect("pushed");

    // A copy made in tmux (here `set-buffer`, as copy mode's yank does):
    // reported by name only, and readable on request.
    s.tmux
        .run_checked(&["set-buffer", "-b", "from-tmux", "copied in tmux"])
        .expect("set-buffer");
    let seen = buffer_events(&mut rx, Duration::from_secs(10)).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "from-tmux");
    assert!(
        !seen[0].1.contains("copied in tmux"),
        "the event must never carry the text: {seen:?}"
    );
    let b = read(&c, Some("from-tmux".into())).await;
    assert!(b.found);
    assert_eq!(b.text, "copied in tmux");
    assert!(!read(&c, Some("no-such-buffer".into())).await.found);

    // A buffer Dreamer writes reaches tmux, and is not reported back.
    let w = c
        .call(Request::ClipboardWriteTmuxBuffer(api::TmuxBufferWrite {
            endpoint: None,
            text: "copied in Dreamer\nsecond line".into(),
        }))
        .await;
    assert!(
        matches!(w, Ok(Response::ClipboardWriteTmuxBuffer(_))),
        "write: {w:?}"
    );
    assert_eq!(
        s.tmux.run_checked(&["show-buffer"]).expect("show-buffer"),
        "copied in Dreamer\nsecond line"
    );
    let echo = buffer_events(&mut rx, Duration::from_secs(2)).await;
    assert!(echo.is_empty(), "Dreamer's own write came back: {echo:?}");
    assert_eq!(read(&c, None).await.text, "copied in Dreamer\nsecond line");

    // An empty write is refused.
    assert!(
        c.call(Request::ClipboardWriteTmuxBuffer(api::TmuxBufferWrite {
            endpoint: None,
            text: String::new(),
        }))
        .await
        .is_err()
    );
}
