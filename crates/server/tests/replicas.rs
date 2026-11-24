//! Client replicas following the real daemon: resume without
//! recapturing tmux, recovery outside retention, daemon restart, respawn.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use replica::{FollowState, Follower, RetryPolicy};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    env: Vec<(&'static str, String)>,
    daemons: Vec<Child>,
}

impl Setup {
    fn new(env: &[(&'static str, &str)]) -> Setup {
        // A counter, not the clock: parallel tests start in the same
        // microsecond and must never share a daemon.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-rep-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let mut s = Setup {
            root,
            tmux,
            env: env.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            daemons: Vec::new(),
        };
        s.start_daemon();
        s
    }

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    fn start_daemon(&mut self) {
        let child = Command::new(EXE)
            .args([
                "--background",
                "--tmux-socket",
                self.tmux.socket(),
                "--tmux",
            ])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        self.daemons.push(child);
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

    async fn follower(&self) -> Follower {
        // Connect once first so the daemon is up before following.
        drop(self.client().await);
        Follower::start(self.socket(), 0, RetryPolicy::default())
            .await
            .expect("follow")
    }

    fn type_line(&self, line: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }

    fn wait_tmux(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self
            .tmux
            .run_checked(&["capture-pane", "-p", "-t", "main"])
            .unwrap_or_default()
            .contains(text)
        {
            assert!(Instant::now() < deadline, "tmux never showed {text}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    async fn mirror(&self) -> (Vec<String>, api::MirrorStatus) {
        let c = self.client().await;
        let Response::TerminalRead(t) = c
            .call(Request::TerminalRead(api::ReadParams {
                pane: 0,
                history: Some(10_000),
                endpoint: None,
            }))
            .await
            .expect("read")
        else {
            panic!("read");
        };
        let Response::TerminalMirrors(m) = c
            .call(Request::TerminalMirrors(api::TopologyParams::default()))
            .await
            .expect("mirrors")
        else {
            panic!("mirrors");
        };
        let status = m.mirrors.into_iter().find(|s| s.pane == 0).expect("pane 0");
        (normalize(t.lines), status)
    }

    async fn shutdown(&mut self) {
        let c = self.client().await;
        let _ = c.call(Request::DaemonShutdown(api::Empty {})).await;
        drop(c);
        for mut d in self.daemons.drain(..) {
            let _ = d.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.socket().exists() {
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(20)).await;
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
        for d in &mut self.daemons {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
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

fn replica_lines(f: &Follower) -> Vec<String> {
    normalize(f.replica().text().split('\n').map(str::to_owned).collect())
}

/// Pump until the replica equals the daemon's mirror.
async fn converge(s: &Setup, f: &mut Follower) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        f.pump(Duration::from_millis(100)).await.expect("pump");
        let (mirror, _) = s.mirror().await;
        if replica_lines(f) == mirror {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "replica never matched the mirror ({:?}): {} vs {} lines",
            f.state,
            replica_lines(f).len(),
            mirror.len()
        );
    }
}

#[tokio::test]
async fn a_reconnecting_client_resumes_by_replay_without_recapturing_tmux() {
    let s = Setup::new(&[]);
    let mut f = s.follower().await;
    assert_eq!(f.last_mode, Some(api::SubscribeMode::Checkpoint));
    s.type_line("seq 1 2000; echo FIRST");
    s.wait_tmux("FIRST");
    converge(&s, &mut f).await;
    let (_, before) = s.mirror().await;

    // The viewer goes away while the pane keeps producing output.
    f.disconnect();
    s.type_line("seq 1 500; echo WHILE-AWAY");
    s.wait_tmux("WHILE-AWAY");
    f.reconnect().await.expect("reconnect");
    assert_eq!(
        f.last_mode,
        Some(api::SubscribeMode::Replay),
        "still retained: no checkpoint"
    );
    converge(&s, &mut f).await;
    assert_eq!(
        f.checkpoints, 1,
        "only the first subscription restored a checkpoint"
    );

    // The daemon kept the same mirror: same epoch and generation, and its
    // sequence moved on instead of starting over after a new capture.
    let (_, after) = s.mirror().await;
    assert_eq!(
        (after.epoch, after.generation),
        (before.epoch, before.generation)
    );
    assert!(
        after.sequence > before.sequence,
        "{} -> {}",
        before.sequence,
        after.sequence
    );
    assert_eq!(after.journal_first_sequence, before.journal_first_sequence);
}

#[tokio::test]
async fn a_client_outside_retention_recovers_through_a_new_checkpoint() {
    let s = Setup::new(&[("DREAMER_JOURNAL_PANE_BYTES", "16384")]);
    let mut f = s.follower().await;
    converge(&s, &mut f).await;
    let (_, before) = s.mirror().await;
    f.disconnect();
    // Far more than 16 KiB while away: the resume point is trimmed.
    s.type_line("seq 1 6000; echo FAR-BEHIND");
    s.wait_tmux("FAR-BEHIND");
    // The mirror ingests asynchronously: tmux showing the output does not
    // mean the daemon has journaled (and trimmed) it yet.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, during) = s.mirror().await;
        if during.journal_first_sequence > before.sequence + 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the resume point was never trimmed: before {before:?}, during {during:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.reconnect().await.expect("reconnect");
    assert_eq!(f.last_mode, Some(api::SubscribeMode::Checkpoint));
    converge(&s, &mut f).await;
    assert_eq!(f.checkpoints, 2);
}

#[tokio::test]
async fn a_daemon_restart_is_a_new_epoch_and_recovers_by_checkpoint() {
    let mut s = Setup::new(&[]);
    let mut f = s.follower().await;
    s.type_line("echo BEFORE-RESTART");
    s.wait_tmux("BEFORE-RESTART");
    converge(&s, &mut f).await;
    let (_, before) = s.mirror().await;

    s.shutdown().await;
    // The follower notices the viewer connection is gone.
    let r = f.pump(Duration::from_secs(5)).await;
    assert!(
        r.is_err() && f.state == FollowState::Disconnected,
        "{r:?} {:?}",
        f.state
    );
    s.start_daemon();
    drop(s.client().await);
    s.type_line("echo AFTER-RESTART");
    s.wait_tmux("AFTER-RESTART");
    f.reconnect().await.expect("reconnect");
    // The old resume point names another epoch, so it is not replayed.
    assert_eq!(f.last_mode, Some(api::SubscribeMode::Checkpoint));
    converge(&s, &mut f).await;
    let (_, after) = s.mirror().await;
    assert!(
        after.epoch > before.epoch,
        "epochs never repeat across daemon restarts"
    );
    assert!(replica_lines(&f).iter().any(|l| l == "AFTER-RESTART"));
}

#[tokio::test]
async fn a_respawned_pane_is_followed_into_its_new_generation() {
    let s = Setup::new(&[]);
    let mut f = s.follower().await;
    s.type_line("echo OLD-PROCESS");
    s.wait_tmux("OLD-PROCESS");
    converge(&s, &mut f).await;
    let (_, before) = s.mirror().await;
    s.tmux
        .run_checked(&["respawn-pane", "-k", "-t", "main", "/bin/sh"])
        .expect("respawn");
    s.type_line("echo NEW-PROCESS");
    s.wait_tmux("NEW-PROCESS");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        f.pump(Duration::from_millis(100)).await.expect("pump");
        let (mirror, status) = s.mirror().await;
        // The new process's first output can still arrive on the old
        // generation's stream, before the daemon sees the respawn; tmux
        // keeps the screen, so the text can match first. Wait for the new
        // generation's own checkpoint as well.
        if status.generation > before.generation
            && f.checkpoints >= 2
            && replica_lines(&f) == mirror
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never followed the respawn ({:?})",
            f.state
        );
    }
    // tmux keeps the pane's screen across `respawn-pane`, so old text may
    // legitimately remain; what matters is that the replica equals the new
    // generation's mirror, checked above.
    assert_eq!(
        f.checkpoints, 2,
        "the new generation started from its own checkpoint"
    );
}

/// Wait for the feed to publish a frame whose text satisfies `want`.
async fn feed_until(feed: &replica::feed::Feed, want: impl Fn(&str) -> bool) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .unwrap_or_else(|_| panic!("no matching frame; feed {:?}", feed.status()))
            .expect("feed alive");
        if let Some(f) = feed.take() {
            let text = f.text_dump();
            if want(&text) {
                return text;
            }
        }
    }
}

#[tokio::test]
async fn a_feed_publishes_on_change_coalesces_and_never_wakes_when_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("echo FEED-OK");
    feed_until(&feed, |t| t.contains("\nFEED-OK")).await;

    // A burst coalesces: the view is woken at most once per frame built.
    s.type_line("seq 1 5000; echo BURST-DONE");
    feed_until(&feed, |t| t.contains("BURST-DONE")).await;
    let frames = feed.stats.frames.load(Relaxed);
    let wakes = feed.stats.view_wakeups.load(Relaxed);
    assert!(wakes <= frames, "{wakes} view wakeups for {frames} frames");

    // Idle: nothing wakes, neither the owner thread nor the view.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while feed.wake.try_recv().is_ok() {}
    let before = (
        feed.stats.owner_wakeups.load(Relaxed),
        feed.stats.view_wakeups.load(Relaxed),
        feed.stats.frames.load(Relaxed),
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = (
        feed.stats.owner_wakeups.load(Relaxed),
        feed.stats.view_wakeups.load(Relaxed),
        feed.stats.frames.load(Relaxed),
    );
    assert_eq!(
        before, after,
        "an idle pane caused wakeups (owner, view, frames)"
    );
    assert!(feed.wake.try_recv().is_err());
}

#[tokio::test]
async fn a_streaming_pane_builds_frames_only_as_the_view_takes_them() {
    use std::sync::atomic::Ordering::Relaxed;
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("echo READY");
    feed_until(&feed, |t| t.contains("\nREADY")).await;

    // The view stops taking frames while the pane streams: the replica
    // keeps applying records but builds no frames nobody would show.
    let before = feed.stats.frames.load(Relaxed);
    s.type_line("seq 1 100000; echo STREAM-DONE");
    s.wait_tmux("STREAM-DONE");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !feed
        .text()
        .await
        .is_some_and(|t| t.contains("\nSTREAM-DONE"))
    {
        assert!(Instant::now() < deadline, "the replica never caught up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let built = feed.stats.frames.load(Relaxed) - before;
    assert!(built <= 2, "{built} frames built with none taken");

    // Taking the waiting frame lets the next one show everything, in order.
    let text = feed_until(&feed, |t| t.contains("\nSTREAM-DONE")).await;
    assert!(text.contains("99999\n100000\nSTREAM-DONE"), "{text}");
    assert!(feed.stats.frames.load(Relaxed) - before <= 4);
}

#[tokio::test]
async fn a_synchronized_output_hold_is_shown_after_the_limit() {
    use std::sync::atomic::Ordering::Relaxed;
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("echo READY");
    feed_until(&feed, |t| t.contains("\nREADY")).await;
    // Begin synchronized output and never end it.
    // Quotes split the marker so the shell's echo of the command does not
    // contain it; only the program's held output does.
    s.type_line("printf '\\033[?2026hHELD''-TEXT'");
    let started = Instant::now();
    feed_until(&feed, |t| t.contains("HELD-TEXT")).await;
    let waited = started.elapsed();
    assert!(
        waited >= replica::feed::HOLD_LIMIT / 2,
        "shown after {waited:?}: the hold was ignored"
    );
    assert!(feed.stats.holds_expired.load(Relaxed) >= 1);
    // Ending the hold presents normally again.
    s.type_line("printf '\\033[?2026l'; echo REL''EASED");
    feed_until(&feed, |t| t.contains("RELEASED")).await;
}

#[tokio::test]
async fn with_no_pane_given_the_feed_follows_the_active_pane() {
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), None, None).expect("feed");
    s.type_line("echo ACT''IVE-PANE");
    feed_until(&feed, |t| t.contains("\nACTIVE-PANE")).await;
    assert_eq!(feed.status(), replica::feed::FeedStatus::Following);
}

#[tokio::test]
async fn without_a_tmux_server_the_feed_says_so() {
    let s = Setup::new(&[]);
    drop(s.client().await);
    s.tmux.run_checked(&["kill-server"]).expect("kill");
    let feed = replica::feed::Feed::start(s.socket(), None, None).expect("feed");
    let text = feed_until(&feed, |t| t.contains("no tmux pane to show")).await;
    assert!(text.contains("start tmux"), "{text}");
    assert!(matches!(
        feed.status(),
        replica::feed::FeedStatus::Reconnecting(_)
    ));
}

#[tokio::test]
async fn input_sent_through_the_feed_reaches_the_pane() {
    use std::sync::atomic::Ordering::Relaxed;
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("echo FEED-READY");
    feed_until(&feed, |t| t.contains("\nFEED-READY")).await;
    // Text the way a window sends committed text, then Enter as a key.
    feed.send(api::terminal_input::Event::Text(api::TextInput {
        text: "echo VIA-FE''ED".into(),
    }));
    feed.send(api::terminal_input::Event::Key(api::KeyInput {
        key: "Enter".into(),
        ..api::KeyInput::default()
    }));
    feed_until(&feed, |t| t.contains("\nVIA-FEED")).await;
    assert_eq!(feed.inputs_sent.load(Relaxed), 2);
    assert_eq!(feed.inputs_dropped.load(Relaxed), 0);
}

async fn frame_until(
    feed: &replica::feed::Feed,
    want: impl Fn(&terminal_ghostty::core::FrameSnapshot) -> bool,
) -> std::sync::Arc<terminal_ghostty::core::FrameSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .unwrap_or_else(|_| panic!("no matching frame; feed {:?}", feed.status()))
            .expect("feed alive");
        if let Some(f) = feed.take()
            && want(&f)
        {
            return f;
        }
    }
}

#[tokio::test]
async fn scrolling_is_local_new_output_keeps_the_view_and_typing_returns() {
    use replica::feed::{Scroll, ViewCommand};
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("seq 1 300; echo SEQ''-DONE");
    feed_until(&feed, |t| t.contains("\nSEQ-DONE")).await;

    feed.view(ViewCommand::Scroll(Scroll::Delta(-100)));
    let back = frame_until(&feed, |f| f.view.rows_back() == 100).await;
    let top = back.rows[0].text();
    assert!(
        !back.text_dump().contains("SEQ-DONE"),
        "still at the bottom"
    );

    // New output arrives while scrolled back: the view keeps its rows.
    s.type_line("echo AFT''ER");
    let later = frame_until(&feed, |f| f.view.rows_back() > 100).await;
    assert_eq!(later.rows[0].text(), top, "new output moved the view");

    // Typing returns to the bottom.
    feed.send(api::terminal_input::Event::Text(api::TextInput {
        text: "echo TYP''ED\r".into(),
    }));
    frame_until(&feed, |f| f.view.rows_back() == 0).await;
    feed_until(&feed, |t| t.contains("\nTYPED")).await;
}

#[tokio::test]
async fn a_drag_held_past_the_top_scrolls_history_and_selects_into_it() {
    use replica::feed::{CellPos, SelectEvent, ViewCommand};
    let s = Setup::new(&[]);
    drop(s.client().await);
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    s.type_line("seq 1 300; echo SEQ''-DONE");
    let last = feed_until(&feed, |t| t.contains("\nSEQ-DONE")).await;
    let row = last
        .lines()
        .position(|l| l.trim_end() == "SEQ-DONE")
        .expect("row") as f32;

    feed.view(ViewCommand::Select(SelectEvent::Press {
        at: CellPos {
            x: 8.9,
            y: row + 0.5,
        },
        time_ns: 1,
    }));
    // Hold the drag above the grid: the owner scrolls a row per tick.
    feed.view(ViewCommand::Select(SelectEvent::Drag {
        at: CellPos { x: 0.1, y: -1.0 },
    }));
    frame_until(&feed, |f| f.view.rows_back() >= 10).await;
    feed.view(ViewCommand::Select(SelectEvent::Release {
        at: CellPos { x: 0.1, y: -1.0 },
    }));
    let text = feed.selection_text().await.expect("a selection");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.last(), Some(&"SEQ-DONE"), "{text}");
    assert!(
        lines.len() > usize::from(last.lines().count() as u16),
        "the selection did not reach into history: {} lines",
        lines.len()
    );
    // No more autoscroll after the release: the view goes quiet.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while feed.wake.try_recv().is_ok() {}
    let _ = feed.take();
    tokio::time::sleep(replica::feed::AUTOSCROLL_TICK * 6).await;
    assert!(
        feed.wake.try_recv().is_err(),
        "still scrolling after release"
    );
}

/// Step `f` until `until` holds for (bells so far, follower) or `wait`
/// passes; returns the bells counted.
async fn bells_until(
    f: &mut Follower,
    wait: Duration,
    until: impl Fn(u32, &Follower) -> bool,
) -> u32 {
    let deadline = tokio::time::Instant::now() + wait;
    let mut bells = 0;
    while !until(bells, f) {
        match tokio::time::timeout_at(deadline, f.step()).await {
            Ok(Ok(s)) => bells += s.bells,
            Ok(Err(e)) => panic!("follow: {e}"),
            Err(_) => break,
        }
    }
    bells
}

#[tokio::test]
async fn a_bell_reaches_each_live_subscriber_once_and_is_never_replayed() {
    let s = Setup::new(&[]);
    let mut a = s.follower().await;
    let mut b = s.follower().await;
    s.type_line("printf '\\033]2;TITLE-ONE\\007\\033]7;file://localhost/tmp/cwd-one\\007\\007'; echo BELL''-RUNG");
    let rung = |n: u32, f: &Follower| n >= 1 && f.replica().text().contains("\nBELL-RUNG");
    let ring_a = bells_until(&mut a, Duration::from_secs(20), rung).await;
    let ring_b = bells_until(&mut b, Duration::from_secs(20), rung).await;
    // Nothing more arrives for that one bell.
    let more_a = bells_until(&mut a, Duration::from_millis(500), |_, _| false).await;
    assert_eq!(
        (ring_a + more_a, ring_b),
        (1, 1),
        "one bell per live subscriber"
    );
    assert_eq!(a.meta.title.as_deref(), Some("TITLE-ONE"));
    assert!(
        a.meta.cwd.as_deref().is_some_and(|c| c.contains("cwd-one")),
        "{:?}",
        a.meta
    );

    // Reconnecting resumes by replay: the labels are current and the bell
    // is not rung again.
    a.disconnect();
    a.reconnect().await.expect("reconnect");
    assert_eq!(a.last_mode, Some(api::SubscribeMode::Replay));
    assert_eq!(a.meta.title.as_deref(), Some("TITLE-ONE"));
    assert_eq!(
        bells_until(&mut a, Duration::from_millis(500), |_, _| false).await,
        0
    );

    // A new follower starts from a checkpoint: labels, no old bells.
    let mut c = s.follower().await;
    assert_eq!(c.last_mode, Some(api::SubscribeMode::Checkpoint));
    assert_eq!(c.meta.title.as_deref(), Some("TITLE-ONE"));
    assert_eq!(
        bells_until(&mut c, Duration::from_millis(500), |_, _| false).await,
        0
    );

    // The daemon reports the labels as state too.
    let Response::TerminalMirrors(m) = s
        .client()
        .await
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
        .expect("mirrors")
    else {
        panic!("mirrors")
    };
    assert_eq!(m.mirrors[0].title.as_deref(), Some("TITLE-ONE"));
}

/// Step `f` for `wait` and return the clipboard writes addressed to it.
async fn clipboard_writes(f: &mut Follower, wait: Duration) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut got = Vec::new();
    while let Ok(r) = tokio::time::timeout_at(deadline, f.step()).await {
        r.expect("follow");
        got.extend(f.take_clipboard());
    }
    got
}

async fn focus(c: &Client, client: u64, gained: bool) {
    c.call(Request::TerminalInput(api::TerminalInput {
        pane: 0,
        client: Some(client),
        event: Some(api::terminal_input::Event::Focus(api::FocusInput {
            gained,
        })),
        ..api::TerminalInput::default()
    }))
    .await
    .expect("focus");
}

#[tokio::test]
async fn a_clipboard_write_reaches_only_the_focused_client_once() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    let start = |id: u64| Follower::start_as(s.socket(), 0, RetryPolicy::default(), Some(id));
    let mut a = start(1).await.expect("a");
    let mut b = start(2).await.expect("b");
    focus(&c, 1, true).await;
    // "CLIP-ONE" in base64; the marker is split so the echo never matches.
    s.type_line("printf '\\033]52;c;Q0xJUC1PTkU=\\007'; echo C''1");
    let to_a = clipboard_writes(&mut a, Duration::from_secs(3)).await;
    let to_b = clipboard_writes(&mut b, Duration::from_millis(500)).await;
    assert_eq!(
        to_a,
        vec![b"CLIP-ONE".to_vec()],
        "the focused client gets it once"
    );
    assert!(to_b.is_empty(), "an unfocused client never gets it");

    // Focus moves to b: the next write goes there only.
    focus(&c, 1, false).await;
    focus(&c, 2, true).await;
    s.type_line("printf '\\033]52;c;Q0xJUC1UV08=\\007'; echo C''2");
    assert_eq!(
        clipboard_writes(&mut b, Duration::from_secs(3)).await,
        vec![b"CLIP-TWO".to_vec()]
    );
    assert!(
        clipboard_writes(&mut a, Duration::from_millis(500))
            .await
            .is_empty()
    );

    // A reconnect replays records, never the write.
    b.disconnect();
    b.reconnect().await.expect("reconnect");
    assert!(
        clipboard_writes(&mut b, Duration::from_millis(500))
            .await
            .is_empty()
    );
    // With no focused client, nobody gets a write.
    focus(&c, 2, false).await;
    s.type_line("printf '\\033]52;c;Q0xJUC1UUkU=\\007'; echo C''3");
    assert!(
        clipboard_writes(&mut b, Duration::from_secs(2))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn focus_outlives_a_rebuilt_mirror_so_clipboard_writes_still_arrive() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    focus(&c, 4, true).await;
    let before = s.mirror().await.1.generation;
    // A new process in the pane gets a new mirror; the client that had
    // the pane's focus still has it.
    s.tmux
        .run_checked(&["respawn-pane", "-k", "-t", "main", "/bin/sh"])
        .expect("respawn");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let after = loop {
        let g = s.mirror().await.1.generation;
        if g != before {
            break g;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mirror was never rebuilt"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Follow the new process's mirror (an old subscription would end).
    let mut a = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never followed the new process"
        );
        if let Ok(mut f) = Follower::start_as(s.socket(), 0, RetryPolicy::default(), Some(4)).await
        {
            let settle = tokio::time::Instant::now() + Duration::from_secs(2);
            while f.replica().position().is_none() {
                if !matches!(tokio::time::timeout_at(settle, f.step()).await, Ok(Ok(_))) {
                    break;
                }
            }
            if f.replica().position().is_some_and(|(_, g, _)| g == after) {
                break f;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // "CLIP-NEW" in base64.
    s.type_line("printf '\\033]52;c;Q0xJUC1ORVc=\\007'; echo N''EW");
    assert_eq!(
        clipboard_writes(&mut a, Duration::from_secs(10)).await,
        vec![b"CLIP-NEW".to_vec()],
        "the write reaches the client that had focus before the mirror was rebuilt"
    );
}

#[tokio::test]
async fn a_feed_that_reports_focus_at_once_gets_a_programs_clipboard_write() {
    let s = Setup::new(&[]);
    // As a window does: start following, and say this pane has focus
    // straight away, before the feed has shown anything.
    let feed = replica::feed::Feed::start(s.socket(), Some(0), None).expect("feed");
    feed.send(api::terminal_input::Event::Focus(api::FocusInput {
        gained: true,
    }));
    s.type_line("echo READY");
    feed_until(&feed, |t| t.contains("\nREADY")).await;
    // The focus report travels apart from the pane's output, so on a slow
    // machine it can land after a write; before, it was lost for good. Write
    // again until one arrives. "CLIP-FEED" in base64.
    let deadline = Instant::now() + Duration::from_secs(15);
    let got = 'outer: loop {
        s.type_line("printf '\\033]52;c;Q0xJUC1GRUVE\\007'; echo F''DONE");
        let wait = Instant::now() + Duration::from_secs(1);
        while Instant::now() < wait {
            if let Some(t) = feed.take_clipboard() {
                break 'outer Some(t);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if Instant::now() > deadline {
            break None;
        }
    };
    assert_eq!(got.as_deref(), Some(&b"CLIP-FEED"[..]));
}

#[tokio::test]
async fn a_clipboard_read_gets_no_clipboard_contents() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    let mut a = Follower::start_as(s.socket(), 0, RetryPolicy::default(), Some(7))
        .await
        .expect("follow");
    focus(&c, 7, true).await;
    // Put something in reach first, so a leak would show.
    // The payload is split in the command line so only a reply matches.
    s.type_line("printf '\\033]52;c;Q0xJ''UC1PTkU=\\007'; echo W''ROTE");
    s.wait_tmux("\nWROTE");
    let _ = clipboard_writes(&mut a, Duration::from_millis(500)).await;
    // Ask for the clipboard and show whatever comes back within a second,
    // with ESC shown as E.
    s.type_line(
        "printf '\\033]52;c;?\\007'; stty raw -echo min 0 time 10; dd bs=1 count=256 2>/dev/null | tr '\\033\\007' EB; stty sane; echo; echo READ''-DONE",
    );
    s.wait_tmux("READ-DONE");
    let screen = s
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", "main"])
        .expect("screen");
    assert!(
        !screen.contains("Q0xJUC1PTkU") && !screen.contains("CLIP-ONE"),
        "clipboard contents reached the program:\n{screen}"
    );
    // Neither does the read become a write for any client.
    assert!(
        clipboard_writes(&mut a, Duration::from_millis(500))
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_color_scheme_reaches_a_view_through_the_checkpoint() {
    let config = PathBuf::from(format!("/tmp/drm-theme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("config");
    std::fs::write(
        config.join("dreamer.yaml"),
        "terminal:\n  theme: gruvbox-dark\n  cursor:\n    style: underline\n",
    )
    .expect("settings");
    let s = Setup::new(&[("DREAMER_CONFIG_DIR", config.to_str().expect("utf-8"))]);
    let mut f = s.follower().await;
    s.type_line("echo themed");
    converge(&s, &mut f).await;
    let scheme = terminal_ghostty::core::schemes::find("gruvbox-dark").expect("scheme");
    let frame = f.replica_mut().frame().expect("an engine").expect("frame");
    assert_eq!(frame.colors.background, scheme.background);
    assert_eq!(frame.colors.foreground, scheme.foreground);
    assert_eq!(frame.colors.palette[2], scheme.ansi[2]);
    assert_eq!(
        frame.cursor.shape,
        terminal_ghostty::core::CursorShape::Underline
    );
    let _ = std::fs::remove_dir_all(&config);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_too_large_for_a_frame_is_an_error_and_the_connection_lives_on() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    // Full-width lines of two-byte characters: the retained history alone
    // is more than one frame.
    s.type_line(
        "awk 'BEGIN{for(i=0;i<12000;i++){s=\"\";for(j=0;j<99;j++)s=s\"\\303\\251\";print s}; print \"DONE-BIG\"}'",
    );
    s.wait_tmux("DONE-BIG");
    // Wait until the mirror holds it too.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let screen = c
            .call(Request::TerminalRead(api::ReadParams {
                pane: 0,
                history: Some(0),
                endpoint: None,
            }))
            .await;
        if let Ok(Response::TerminalRead(t)) = screen
            && t.lines.iter().any(|l| l.trim_end() == "DONE-BIG")
        {
            break;
        }
        assert!(Instant::now() < deadline, "the mirror never caught up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let big = tokio::time::timeout(
        Duration::from_secs(20),
        c.call(Request::TerminalRead(api::ReadParams {
            pane: 0,
            history: Some(150_000),
            endpoint: None,
        })),
    )
    .await
    .expect("a reply, not a hang");
    let e = big.expect_err("too large for one frame");
    assert!(e.to_string().contains("frame limit"), "{e}");
    // The same connection still answers.
    let Response::DaemonVersion(_) = c
        .call(Request::DaemonVersion(api::Empty {}))
        .await
        .expect("still connected")
    else {
        panic!("version");
    };
}
