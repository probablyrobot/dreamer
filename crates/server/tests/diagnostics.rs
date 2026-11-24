//! Performance diagnostics: off by default, collecting nothing;
//! once on, every daemon stage of a keystroke's way to a client is timed
//! and the traffic counted; off again, nothing more is added.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response, terminal_input::Event};
use client::Client;
use replica::{Follower, RetryPolicy};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    _tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(80, 24, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup {
            root,
            _tmux: tmux,
            daemon,
        }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.socket(), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
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

async fn diagnostics(c: &Client) -> api::Diagnostics {
    match c.call(Request::DaemonDiagnostics(api::Empty {})).await {
        Ok(Response::DaemonDiagnostics(d)) => d,
        other => panic!("diagnostics: {other:?}"),
    }
}

async fn set(c: &Client, enabled: bool, reset: bool) -> api::Diagnostics {
    match c
        .call(Request::DaemonSetDiagnostics(api::DiagnosticsSettings {
            enabled,
            reset,
        }))
        .await
    {
        Ok(Response::DaemonSetDiagnostics(d)) => d,
        other => panic!("set diagnostics: {other:?}"),
    }
}

/// Type a command that prints `M<n>K` into pane 0, and wait until the
/// daemon's mirror shows that line.
async fn print_marker(c: &Client, n: u32) {
    c.call(Request::TerminalInput(api::TerminalInput {
        pane: 0,
        event: Some(Event::Text(api::TextInput {
            text: format!("printf 'M%sK\\n' {n}\r"),
        })),
        ..api::TerminalInput::default()
    }))
    .await
    .expect("input");
    let want = format!("M{n}K");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Response::TerminalRead(t)) = c
            .call(Request::TerminalRead(api::ReadParams {
                pane: 0,
                history: Some(10),
                endpoint: None,
            }))
            .await
            && t.lines.iter().any(|l| l.trim_end() == want)
        {
            return;
        }
        assert!(Instant::now() < deadline, "the mirror never showed {want}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn count(d: &api::Diagnostics, stage: &str) -> u64 {
    d.stages
        .iter()
        .find(|s| s.stage == stage)
        .unwrap_or_else(|| panic!("no stage {stage}"))
        .count
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_are_off_until_turned_on_then_time_every_daemon_stage() {
    let s = Setup::new();
    let c = s.client().await;
    print_marker(&c, 1).await;

    // Off by default: the output above was neither timed nor counted.
    let d = diagnostics(&c).await;
    assert!(!d.enabled);
    assert!(d.stages.iter().all(|s| s.count == 0), "{:?}", d.stages);
    assert_eq!((d.tmux_bytes, d.parsed_bytes, d.records), (0, 0, 0));
    // Queue depths are read now, on or off.
    assert!(!d.worker_queues.is_empty());
    assert!(d.worker_queue_capacity > 0);

    assert!(set(&c, true, true).await.enabled);
    let mut f = Follower::start(s.socket(), 0, RetryPolicy::default())
        .await
        .expect("follow");
    print_marker(&c, 2).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.replica().text().lines().any(|l| l.trim_end() == "M2K") {
        assert!(Instant::now() < deadline, "the replica never showed M2K");
        f.pump(Duration::from_millis(50)).await.expect("pump");
    }

    // The client can see the output before the daemon has recorded how
    // long sending it took, so wait for every stage to have a sample.
    let deadline = Instant::now() + Duration::from_secs(10);
    let d = loop {
        let d = diagnostics(&c).await;
        if d.stages.iter().all(|s| s.count > 0) || Instant::now() >= deadline {
            break d;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(d.enabled);
    // Every daemon stage, in path order; the replica's are the client's.
    let names: Vec<&str> = d.stages.iter().map(|s| s.stage.as_str()).collect();
    assert_eq!(
        names,
        [
            "input", "tmux", "decode", "route", "parse", "publish", "transfer"
        ]
    );
    for st in &d.stages {
        assert!(st.count > 0, "stage {} has no samples", st.stage);
        assert!(
            st.p50_ns <= st.p95_ns && st.p95_ns <= st.p99_ns && st.p99_ns <= st.max_ns,
            "{st:?}"
        );
    }
    assert!(d.tmux_bytes > 0 && d.parsed_bytes > 0 && d.records > 0);
    assert!(d.parser_bytes_per_second > 0);
    assert!(d.transferred_bytes > 0 && d.transferred_batches > 0);
    assert!(
        d.checkpoints >= 1 && d.checkpoint_bytes > 0,
        "the follower's start"
    );
    assert_eq!(d.lagged_subscribers, 0);
    assert!(d.subscribers >= 1 && d.subscriber_queue_capacity > 0);

    // Off again: what was collected stays, and nothing more is added.
    set(&c, false, false).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = diagnostics(&c).await;
    print_marker(&c, 3).await;
    let after = diagnostics(&c).await;
    assert!(!after.enabled);
    assert_eq!(after.records, before.records);
    assert_eq!(count(&after, "parse"), count(&before, "parse"));
    assert_eq!(count(&after, "input"), count(&before, "input"));
    assert!(count(&after, "parse") > 0);

    // A reset zeroes them.
    let d = set(&c, false, true).await;
    assert!(d.stages.iter().all(|s| s.count == 0));
    assert_eq!(d.records, 0);
}
