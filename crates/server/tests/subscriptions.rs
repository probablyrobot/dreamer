//! Terminal subscriptions through the real daemon: checkpoint at a sequence
//! boundary plus ordered records, journal replay on resume, and an explicit
//! end for a subscriber that falls behind.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message as _;
use terminal_ghostty::core::GridSize;
use terminal_ghostty::{Engine, EngineConfig};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Daemon {
    root: PathBuf,
    tmux: TmuxServer,
    launcher: std::process::Child,
}

impl Daemon {
    fn start() -> Daemon {
        Daemon::start_with(&[])
    }

    fn start_with(env: &[(&str, &str)]) -> Daemon {
        let root = PathBuf::from(format!(
            "/tmp/drm-sub-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let launcher = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Daemon {
            root,
            tmux,
            launcher,
        }
    }

    async fn client(&self) -> Client {
        self.client_with(1024).await
    }

    async fn client_with(&self, pushed: usize) -> Client {
        let socket = self.root.join("rt/dreamerd.sock");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_with(&socket, "test", pushed).await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn type_line(&self, line: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.launcher.kill();
        let _ = self.launcher.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A counter, not the clock: parallel tests start in the same microsecond
/// and must never share a daemon.
fn rand_suffix() -> u32 {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The minimum a client needs to follow a pane: an engine restored from the
/// checkpoint, then records applied strictly in order.
struct Follower {
    engine: Option<Engine>,
    scrollback: usize,
    /// Last applied sequence.
    applied: u64,
    /// Records that arrived before the checkpoint they follow.
    early: Vec<api::TerminalRecord>,
}

impl Follower {
    fn new(s: &api::Subscription) -> Follower {
        Follower {
            engine: None,
            scrollback: s.scrollback_lines as usize,
            applied: s.sequence,
            early: Vec::new(),
        }
    }

    fn config(&self) -> EngineConfig {
        let mut c = EngineConfig::new(GridSize { cols: 80, rows: 24 });
        c.scrollback_lines = Some(self.scrollback);
        c.scrollback_bytes = Some(usize::MAX);
        c.continuation_max_bytes = Some(tmux_backend::bootstrap::CONTINUATION_MAX_BYTES);
        c
    }

    fn checkpoint(&mut self, data: &api::CheckpointData) {
        self.engine = Some(Engine::from_snapshot(&data.snapshot, &self.config()).expect("restore"));
        self.applied = data.sequence;
        for r in std::mem::take(&mut self.early) {
            self.apply(r);
        }
    }

    fn apply(&mut self, r: api::TerminalRecord) {
        let Some(engine) = self.engine.as_mut() else {
            self.early.push(r);
            return;
        };
        if r.sequence <= self.applied {
            return; // already in the checkpoint
        }
        assert_eq!(r.sequence, self.applied + 1, "a gap in the record stream");
        match (r.cols, r.rows) {
            (Some(c), Some(rows)) => engine
                .resize(
                    GridSize {
                        cols: c as u16,
                        rows: rows as u16,
                    },
                    mirror::worker::CELL_PX,
                )
                .expect("resize"),
            _ => drop(engine.feed(&r.data).expect("feed")),
        }
        self.applied = r.sequence;
    }

    fn lines(&self) -> Vec<String> {
        let text = self
            .engine
            .as_ref()
            .expect("restored")
            .screen_text()
            .expect("text");
        normalize(text.split('\n').map(str::to_owned).collect())
    }
}

fn normalize(mut lines: Vec<String>) -> Vec<String> {
    for l in &mut lines {
        let t = l.trim_end().len();
        l.truncate(t);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

async fn subscribe(c: &Client, req: api::TerminalSubscribe) -> api::Subscription {
    match c
        .call(Request::TerminalSubscribe(req))
        .await
        .expect("subscribe")
    {
        Response::TerminalSubscribe(s) => s,
        other => panic!("{other:?}"),
    }
}

async fn mirror_lines(c: &Client) -> Vec<String> {
    match c
        .call(Request::TerminalRead(api::ReadParams {
            pane: 0,
            history: Some(10_000),
            endpoint: None,
        }))
        .await
        .expect("read")
    {
        Response::TerminalRead(t) => normalize(t.lines),
        other => panic!("{other:?}"),
    }
}

/// Apply whatever is pushed within `wait`.
async fn pump(f: &mut Follower, pushed: &mut Receiver<Pushed>, sub: u64, wait: Duration) {
    while let Ok(Some(p)) = tokio::time::timeout(wait, pushed.recv()).await {
        apply_pushed(f, p, sub);
    }
}

fn apply_pushed(f: &mut Follower, p: Pushed, sub: u64) {
    match p {
        Pushed::Bulk { stream, data, .. } if stream == sub => {
            f.checkpoint(&api::CheckpointData::decode(data.as_slice()).expect("checkpoint"));
        }
        Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
            let b = api::RecordBatch::decode(payload.as_slice()).expect("batch");
            assert_eq!(b.subscription, sub);
            for r in b.records {
                f.apply(r);
            }
        }
        Pushed::Event { topic, payload, .. } if topic == "terminal.end" => {
            panic!(
                "subscription ended: {:?}",
                api::SubscriptionEnd::decode(payload.as_slice())
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// Apply pushed data until `done` holds for the follower.
async fn follow_until(
    f: &mut Follower,
    pushed: &mut Receiver<Pushed>,
    sub: u64,
    timeout: Duration,
    done: impl Fn(&Follower) -> bool,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !(f.engine.is_some() && done(f)) {
        let p = tokio::time::timeout_at(deadline, pushed.recv())
            .await
            .expect("timed out following the pane")
            .expect("connection open");
        apply_pushed(f, p, sub);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkpoint_plus_records_matches_the_mirror_and_resume_replays() {
    let d = Daemon::start();
    let mut c = d.client().await;
    let mut pushed = c.pushed().expect("pushed");
    let s = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s.mode, api::SubscribeMode::Checkpoint as i32);
    assert!(s.checkpoint_bytes > 0 && s.scrollback_lines > 0, "{s:?}");
    let mut f = Follower::new(&s);

    d.type_line("seq 1 8000; echo FOLLOWED");
    follow_until(&mut f, &mut pushed, s.id, Duration::from_secs(60), |f| {
        f.lines().iter().any(|l| l == "FOLLOWED")
    })
    .await;
    // Converged: the follower equals the daemon's canonical mirror.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ours = f.lines();
        let theirs = mirror_lines(&c).await;
        if ours == theirs {
            break;
        }
        if Instant::now() >= deadline {
            let i = ours
                .iter()
                .zip(&theirs)
                .position(|(a, b)| a != b)
                .unwrap_or(ours.len().min(theirs.len()));
            panic!(
                "follower ({} lines) never matched the mirror ({} lines); first difference at {i}:\nours  {:?}\ntheirs {:?}",
                ours.len(),
                theirs.len(),
                &ours[i.saturating_sub(2)..(i + 3).min(ours.len())],
                &theirs[i.saturating_sub(2)..(i + 3).min(theirs.len())]
            );
        }
        pump(&mut f, &mut pushed, s.id, Duration::from_millis(100)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (epoch, generation, last) = (s.epoch, s.generation, f.applied);

    // Disconnect, let the pane move on, and come back.
    drop(pushed);
    drop(c);
    d.type_line("seq 1 300; echo RESUMED");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !d
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", "main"])
        .unwrap_or_default()
        .contains("RESUMED")
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = d.client().await;
    let mut pushed = c.pushed().expect("pushed");
    let s2 = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            resume_epoch: Some(epoch),
            resume_generation: Some(generation),
            resume_sequence: Some(last),
            ..Default::default()
        },
    )
    .await;
    // Still retained: replay, no checkpoint, same mirror (epoch and
    // generation unchanged, so tmux was not captured again).
    assert_eq!(s2.mode, api::SubscribeMode::Replay as i32, "{s2:?}");
    assert_eq!(
        (s2.epoch, s2.generation, s2.sequence),
        (epoch, generation, last)
    );
    assert_eq!(s2.checkpoint_bytes, 0);
    follow_until(&mut f, &mut pushed, s2.id, Duration::from_secs(30), |f| {
        f.lines().iter().any(|l| l == "RESUMED")
    })
    .await;
    assert!(f.applied > last);
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.lines() != mirror_lines(&c).await {
        assert!(
            Instant::now() < deadline,
            "resumed follower never matched the mirror"
        );
        pump(&mut f, &mut pushed, s2.id, Duration::from_millis(100)).await;
    }

    // A resume point from another epoch cannot be replayed.
    let s3 = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            resume_epoch: Some(epoch + 100),
            resume_generation: Some(generation),
            resume_sequence: Some(last),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s3.mode, api::SubscribeMode::Checkpoint as i32);
    c.call(Request::TerminalUnsubscribe(api::Unsubscribe {
        subscription: s3.id,
    }))
    .await
    .expect("unsubscribe");
    c.call(Request::DaemonShutdown(api::Empty {}))
        .await
        .expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscriber_that_stops_reading_is_ended_as_lagged() {
    // Small queues on both sides, so the subscriber falls behind on every
    // platform regardless of how much the OS buffers in the socket.
    let d = Daemon::start_with(&[("DREAMER_TEST_SMALL_QUEUES", "1")]);
    let mut c = d.client_with(2).await;
    let mut pushed = c.pushed().expect("pushed");
    let s = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            ..Default::default()
        },
    )
    .await;
    // Heavy output while this client reads nothing.
    d.type_line("seq 1 400000; echo FLOOD-DONE");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", "main"])
        .unwrap_or_default()
        .contains("FLOOD-DONE")
    {
        assert!(Instant::now() < deadline, "flood never finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Meanwhile the daemon kept serving requests on other connections.
    let other = d.client().await;
    let _ = mirror_lines(&other).await;
    // Now read: the stream ends with an explicit reason, never a gap.
    let mut last = 0u64;
    let reason = loop {
        let p = tokio::time::timeout(Duration::from_secs(30), pushed.recv())
            .await
            .expect("an end arrives")
            .expect("open");
        match p {
            Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
                for r in api::RecordBatch::decode(payload.as_slice())
                    .expect("batch")
                    .records
                {
                    assert!(
                        last == 0 || r.sequence == last + 1,
                        "records stay contiguous until the end"
                    );
                    last = r.sequence;
                }
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.end" => {
                let e = api::SubscriptionEnd::decode(payload.as_slice()).expect("end");
                assert_eq!(e.subscription, s.id);
                break e.reason;
            }
            Pushed::Bulk { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(reason, api::EndReason::Lagged as i32);
    other
        .call(Request::DaemonShutdown(api::Empty {}))
        .await
        .expect("shutdown");
}
