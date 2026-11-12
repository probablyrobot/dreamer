//! `dreamerd` lifecycle against the real binary, with private runtime and
//! state directories and isolated tmux servers.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, ClientError, Launch, ensure_running};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

/// Private directories for one daemon; kills it and cleans up on drop.
struct Dirs {
    root: PathBuf,
}

static N: AtomicU32 = AtomicU32::new(0);

impl Dirs {
    fn new() -> Dirs {
        // Short: Unix socket paths are limited to about 100 bytes.
        let root = PathBuf::from("/tmp").join(format!(
            "drm-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        Dirs { root }
    }
    fn runtime(&self) -> PathBuf {
        self.root.join("rt")
    }
    fn state(&self) -> PathBuf {
        self.root.join("st")
    }
    fn socket(&self) -> PathBuf {
        self.runtime().join("dreamerd.sock")
    }
    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("DREAMER_RUNTIME_DIR", self.runtime().display().to_string()),
            ("DREAMER_STATE_DIR", self.state().display().to_string()),
            // Belt and braces: the daemon refuses the user's tmux server.
            ("DREAMER_FORBID_DEFAULT_TMUX", "1".into()),
        ]
    }
    /// A tmux socket name nothing listens on, so a daemon without an
    /// explicit endpoint can never reach the user's own server.
    fn no_tmux(&self) -> String {
        format!("drm-none-{}", std::process::id())
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(EXE);
        c.arg("--tmux-socket").arg(self.no_tmux());
        c.args(args)
            .arg("--tmux")
            .arg(tmux_program())
            .envs(self.env());
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }
    fn holder_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.runtime().join("dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
    fn launch(&self, extra: &[&str]) -> Launch {
        let mut args: Vec<String> = vec![
            "--tmux-socket".into(),
            self.no_tmux(),
            "--tmux".into(),
            tmux_program().display().to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        Launch {
            socket: self.socket(),
            daemon: PathBuf::from(EXE),
            args,
            env: self
                .env()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            wait: Duration::from_secs(10),
        }
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        if let Some(pid) = self.holder_pid().filter(|p| platform::process_alive(*p)) {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn connect_soon(socket: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect(socket, "test").await {
            Ok(c) => return c,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("daemon never answered: {e}"),
        }
    }
}

async fn status(c: &mut Client) -> api::DaemonStatus {
    match c
        .call(Request::DaemonStatus(api::Empty {}))
        .await
        .expect("status")
    {
        Response::DaemonStatus(s) => s,
        other => panic!("{other:?}"),
    }
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_starts_produce_one_daemon() {
    let d = Dirs::new();
    let mut children: Vec<Child> = (0..8)
        .map(|_| d.command(&["--background"]).spawn().expect("spawn"))
        .collect();
    let mut c = connect_soon(&d.socket()).await;
    let s = status(&mut c).await;
    // Every loser exits with "already running"; one daemon remains.
    let mut running = Vec::new();
    for child in &mut children {
        match wait_exit(child, Duration::from_secs(5)) {
            Some(code) => assert_eq!(code.code(), Some(3), "a losing start exits with 3"),
            None => running.push(child.id()),
        }
    }
    assert_eq!(
        running,
        vec![s.pid],
        "exactly one daemon, and it is the one answering"
    );
    // Concurrent ensure-running callers all reach that same daemon.
    let launch = d.launch(&[]);
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let l = launch.clone();
            tokio::spawn(async move { ensure_running(&l, "test").await.expect("ensure").0.welcome })
        })
        .collect();
    for t in tasks {
        let w = t.await.expect("join");
        assert_eq!(w.pid, s.pid);
        assert_eq!(w.instance, c.welcome.instance);
    }
    // Private files.
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&d.runtime()), 0o700);
    assert_eq!(mode(&d.state()), 0o700);
    assert_eq!(mode(&d.socket()), 0o600);
    assert_eq!(mode(&d.state().join("dreamerd.log")), 0o600);
    let log = std::fs::read_to_string(d.state().join("dreamerd.log")).expect("log");
    assert!(log.contains("already running"), "{log}");
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_running_starts_a_daemon_that_outlives_its_terminal() {
    let d = Dirs::new();
    // A terminal: a tmux pane whose shell launches the daemon the way a
    // user's shell would, then the whole terminal goes away.
    let term = TmuxServer::start(80, 24, "/bin/sh").expect("tmux");
    let env: String = d.env().iter().map(|(k, v)| format!("{k}='{v}' ")).collect();
    let line = format!(
        "{env}'{EXE}' --background --tmux-socket {} --tmux '{}' & echo launched-$!",
        d.no_tmux(),
        tmux_program().display()
    );
    term.run_checked(&["send-keys", "-t", "main", "-l", &line])
        .expect("type");
    term.run_checked(&["send-keys", "-t", "main", "Enter"])
        .expect("enter");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut c = loop {
        match Client::connect(&d.socket(), "test").await {
            Ok(c) => break c,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => panic!(
                "daemon never answered: {e}\npane:\n{}",
                term.run_checked(&["capture-pane", "-p", "-t", "main"])
                    .unwrap_or_default()
            ),
        }
    };
    let s = status(&mut c).await;
    let shell_pid: u32 = term
        .format("main", "#{pane_pid}")
        .expect("pane pid")
        .trim()
        .parse()
        .expect("pid");
    assert_ne!(
        platform::session_of(s.pid),
        platform::session_of(shell_pid),
        "the daemon left the terminal's session"
    );
    drop(c);
    // Closing the terminal hangs up everything in it.
    term.run_checked(&["kill-server"]).expect("kill");
    drop(term);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(platform::process_alive(s.pid));
    let mut again = Client::connect(&d.socket(), "test")
        .await
        .expect("still serving");
    assert_eq!(status(&mut again).await.pid, s.pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_but_frames_reaches_stdout_or_the_socket() {
    let d = Dirs::new();
    let child = d
        .command(&[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("foreground daemon");
    let mut c = connect_soon(&d.socket()).await;
    // Version, an unknown method, an unavailable endpoint, then shutdown:
    // every reply must decode as a frame, so stray bytes would fail here.
    match c
        .call(Request::DaemonVersion(api::Empty {}))
        .await
        .expect("version")
    {
        Response::DaemonVersion(v) => {
            assert_eq!(v.build_family, terminal_core::family::build_family());
            assert_eq!(v.protocol_major, u32::from(wire::PROTOCOL_MAJOR));
        }
        other => panic!("{other:?}"),
    }
    match c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, api::code::UNAVAILABLE),
        other => panic!("expected unavailable, got {other:?}"),
    }
    let r = c
        .call(Request::DaemonShutdown(api::Empty {}))
        .await
        .expect("shutdown");
    let Response::DaemonShutdown(effect) = r else {
        panic!("{r:?}")
    };
    assert!(effect.effect.contains("keep running"), "{}", effect.effect);
    let out = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .expect("join")
        .expect("output");
    assert!(out.status.success(), "{:?}", out.status);
    assert!(
        out.stdout.is_empty(),
        "stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("started dreamerd") && err.contains("shutdown requested"),
        "{err}"
    );
    assert!(!d.socket().exists(), "the socket is removed on shutdown");
}

#[test]
fn an_open_runtime_directory_is_refused() {
    let d = Dirs::new();
    std::fs::create_dir_all(d.runtime()).expect("dir");
    std::fs::set_permissions(d.runtime(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = d.command(&[]).stderr(Stdio::piped()).output().expect("run");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("must be 0700"), "{err}");
    assert!(!d.socket().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_the_daemon_and_leaves_tmux_running() {
    let d = Dirs::new();
    let tmux = TmuxServer::start(80, 24, "sleep 600").expect("tmux");
    let launch = d.launch(&["--tmux-socket", tmux.socket()]);
    let (mut c, started) = ensure_running(&launch, "test").await.expect("ensure");
    assert!(started);
    let pid = c.welcome.pid;
    match c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology")
    {
        Response::TmuxTopology(t) => {
            assert_eq!(t.sessions.len(), 1);
            assert_eq!(t.panes.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    let s = status(&mut c).await;
    assert_eq!(s.endpoints[0].state, api::LinkState::Live as i32);
    assert!(
        s.endpoints[0]
            .tmux_version
            .as_deref()
            .unwrap_or_default()
            .starts_with("tmux")
    );
    c.call(Request::DaemonShutdown(api::Empty {}))
        .await
        .expect("shutdown");
    let deadline = Instant::now() + Duration::from_secs(5);
    while platform::process_alive(pid) && Instant::now() < deadline {
        // A reaped or zombie daemon both count as stopped for the socket.
        if !d.socket().exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!d.socket().exists(), "daemon still serving after shutdown");
    tmux.run_checked(&["has-session", "-t", "main"])
        .expect("tmux session survives");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_crash_mid_action_is_outcome_unknown_after_restart() {
    let d = Dirs::new();
    let tmux = TmuxServer::start(100, 30, "sleep 600").expect("tmux");
    let pane: u32 = tmux
        .format("main", "#{pane_id}")
        .expect("pane")
        .trim()
        .trim_start_matches('%')
        .parse()
        .expect("id");
    let mut crashing = d
        .command(&["--background", "--tmux-socket", tmux.socket()])
        .env("DREAMER_TEST_ABORT_AFTER_DISPATCH", "1")
        .spawn()
        .expect("daemon");
    let mut c = connect_soon(&d.socket()).await;
    let split = api::SubmitAction {
        action: Some(api::Action {
            kind: Some(api::action::Kind::SplitPane(api::SplitPane {
                pane,
                direction: api::SplitDirection::Vertical as i32,
                argv: vec!["sleep".into(), "600".into()],
                ..api::SplitPane::default()
            })),
        }),
        ..api::SubmitAction::default()
    };
    let r = c.call(Request::ActionsSubmit(split)).await;
    assert!(r.is_err(), "the daemon died mid-action: {r:?}");
    let code = wait_exit(&mut crashing, Duration::from_secs(5)).expect("crashed");
    assert!(!code.success());

    // A new daemon settles the action and does not replay it.
    let (mut c, started) = ensure_running(&d.launch(&["--tmux-socket", tmux.socket()]), "test")
        .await
        .expect("restart");
    assert!(started);
    let list = match c
        .call(Request::ActionsList(api::ActionListParams::default()))
        .await
        .expect("list")
    {
        Response::ActionsList(l) => l.actions,
        other => panic!("{other:?}"),
    };
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].state,
        api::ActionState::OutcomeUnknown as i32,
        "{:?}",
        list[0]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let panes = tmux
        .run_checked(&["list-panes", "-a", "-F", "#{pane_id}"])
        .expect("panes");
    assert_eq!(
        panes.lines().count(),
        2,
        "the split ran exactly once: {panes}"
    );
    let log = std::fs::read_to_string(d.state().join("dreamerd.log")).expect("log");
    assert!(log.contains("settled as outcome_unknown"), "{log}");
    c.call(Request::DaemonShutdown(api::Empty {}))
        .await
        .expect("shutdown");
}
