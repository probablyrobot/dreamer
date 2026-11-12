//! Client replicas following the real daemon (spec 18.2): resume without
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
    let (_, during) = s.mirror().await;
    assert!(
        during.journal_first_sequence > before.sequence + 1,
        "the resume point was trimmed: before {before:?}, during {during:?}"
    );
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
        if status.generation > before.generation && replica_lines(&f) == mirror {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never followed the respawn ({:?})",
            f.state
        );
    }
    assert!(!replica_lines(&f).iter().any(|l| l.contains("OLD-PROCESS")));
    assert_eq!(
        f.checkpoints, 2,
        "the new generation started from its own checkpoint"
    );
}
