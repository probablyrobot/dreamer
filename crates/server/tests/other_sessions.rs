//! Panes outside the session the daemon's main control client attached
//! to: a user's `tmux new -s work` on the same server must stream like any
//! other pane. tmux sends `%output` to a control client only for panes
//! whose window is in that client's session, so the daemon keeps a client
//! per session and takes each pane's output from exactly one of them.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response, terminal_input::Event};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    /// A server with `main` plus `sessions`, created before the daemon starts.
    fn new(sessions: &[&str]) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-oses-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        for name in sessions {
            tmux.run_checked(&["new-session", "-d", "-s", name, "/bin/sh"])
                .expect("session");
        }
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

    fn pane_of(&self, target: &str) -> u32 {
        self.tmux
            .format(target, "#{pane_id}")
            .expect("pane id")
            .trim_start_matches('%')
            .parse()
            .expect("pane number")
    }

    fn type_line(&self, pane: u32, line: &str) {
        let t = format!("%{pane}");
        self.tmux
            .run_checked(&["send-keys", "-t", &t, "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", &t, "Enter"])
            .expect("enter");
    }

    /// Wait until tmux itself shows `line` in the pane: the program ran.
    fn wait_tmux(&self, pane: u32, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
                .unwrap_or_default();
            if s.lines().any(|l| l.trim_end() == line) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tmux never showed {line:?}:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(50));
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

async fn mirror(c: &Client, pane: u32) -> Option<api::MirrorStatus> {
    match c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    {
        Ok(Response::TerminalMirrors(l)) => l.mirrors.into_iter().find(|m| m.pane == pane),
        other => panic!("mirrors: {other:?}"),
    }
}

async fn wait_live(c: &Client, pane: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let m = mirror(c, pane).await;
        if m.as_ref().map(|m| m.state) == Some(api::MirrorState::Live as i32) {
            return;
        }
        assert!(Instant::now() < deadline, "%{pane} never went Live: {m:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn mirror_has(c: &Client, pane: u32, line: &str) -> bool {
    matches!(
        c.call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(200),
            endpoint: None,
        }))
        .await,
        Ok(Response::TerminalRead(t)) if t.lines.iter().any(|l| l.trim_end() == line)
    )
}

async fn wait_mirror(c: &Client, pane: u32, line: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !mirror_has(c, pane, line).await {
        assert!(
            Instant::now() < deadline,
            "the mirror of %{pane} never showed {line:?}; tmux did; status {:?}",
            mirror(c, pane).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn type_through_daemon(c: &Client, pane: u32, text: &str) {
    for event in [
        Event::Text(api::TextInput { text: text.into() }),
        Event::Key(api::KeyInput {
            key: "Enter".into(),
            ..api::KeyInput::default()
        }),
    ] {
        let r = c
            .call(Request::TerminalInput(api::TerminalInput {
                pane,
                generation: None,
                endpoint: None,
                client: None,
                event: Some(event),
                lease: None,
            }))
            .await;
        assert!(matches!(r, Ok(Response::TerminalInput(_))), "{r:?}");
    }
}

/// Output, and input echo, in a pane of `target` reach the daemon's mirror.
async fn streams(s: &Setup, c: &Client, pane: u32, tag: &str) {
    wait_live(c, pane).await;
    // Markers are split with quotes so the shell's echo of the command
    // never matches the line it prints.
    let (a, b) = tag.split_at(2);
    s.type_line(pane, &format!("echo {a}''{b}-OUT"));
    s.wait_tmux(pane, &format!("{tag}-OUT"));
    wait_mirror(c, pane, &format!("{tag}-OUT")).await;
    type_through_daemon(c, pane, &format!("echo {a}''{b}-IN")).await;
    s.wait_tmux(pane, &format!("{tag}-IN"));
    wait_mirror(c, pane, &format!("{tag}-IN")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn panes_in_a_session_the_daemon_did_not_attach_to_stream_live() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    // Attach the daemon to `main` first, as a running Dreamer would be.
    wait_live(&c, s.pane_of("main:")).await;

    // Control: a new window in the attached session.
    s.tmux
        .run_checked(&["new-window", "-d", "-t", "main:", "/bin/sh"])
        .expect("window");
    let same = s.pane_of("main:1");
    streams(&s, &c, same, "SAME").await;

    // A user's `tmux new -s work` on the same server.
    s.tmux
        .run_checked(&["new-session", "-d", "-s", "work", "/bin/sh"])
        .expect("session");
    let other = s.pane_of("work:");
    streams(&s, &c, other, "WORK").await;
}

/// After the harness drawer was used once, its holding session sorts before
/// `main`; a restarted daemon must still stream the user's own panes.
#[tokio::test(flavor = "multi_thread")]
async fn the_drawer_holding_session_does_not_take_the_user_s_panes_offline() {
    let s = Setup::new(&[tmux_backend::ops::DRAWER_HOLDING]);
    let c = s.client().await;
    let main = s.pane_of("main:");
    streams(&s, &c, main, "MAIN").await;
}

/// The mirror's lines that are `<tag>-<n>`, as numbers, in screen order.
async fn numbered(c: &Client, pane: u32, tag: &str) -> Vec<u32> {
    match c
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(500),
            endpoint: None,
        }))
        .await
    {
        Ok(Response::TerminalRead(t)) => t
            .lines
            .iter()
            .filter_map(|l| {
                l.trim_end()
                    .strip_prefix(tag)?
                    .strip_prefix('-')?
                    .parse()
                    .ok()
            })
            .collect(),
        other => panic!("read: {other:?}"),
    }
}

/// Print `<tag>-1` to `<tag>-<n>` in the pane, then wait until its mirror
/// shows exactly those lines, once each and in order.
async fn count_once(s: &Setup, c: &Client, pane: u32, tag: &str, n: u32) {
    // `$i` keeps the command line itself from matching.
    s.type_line(
        pane,
        &format!("i=1; while [ $i -le {n} ]; do echo {tag}-$i; i=$((i+1)); done"),
    );
    s.wait_tmux(pane, &format!("{tag}-{n}"));
    let want: Vec<u32> = (1..=n).collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = numbered(c, pane, tag).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the mirror of %{pane} shows {tag} lines {got:?}, not 1..={n} once each; status {:?}",
            mirror(c, pane).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `main`, with the daemon attached to it, then `work` and `more`.
async fn two_more_sessions() -> Setup {
    let s = Setup::new(&[]);
    let c = s.client().await;
    wait_live(&c, s.pane_of("main:")).await;
    for name in ["work", "more"] {
        s.tmux
            .run_checked(&["new-session", "-d", "-s", name, "/bin/sh"])
            .expect("session");
    }
    s
}

/// A window linked into several sessions is in several clients' sessions,
/// and tmux sends its output to each of them: the mirror takes it from one.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_linked_into_two_sessions_streams_once_and_in_order() {
    let s = two_more_sessions().await;
    let c = s.client().await;
    let pane = s.pane_of("work:");
    s.tmux
        .run_checked(&["link-window", "-d", "-s", "work:0", "-t", "more:9"])
        .expect("link");
    wait_live(&c, pane).await;
    count_once(&s, &c, pane, "TWO", 40).await;

    // Linked into the main session as well: the pane moves to that
    // session's client, and still nothing arrives twice.
    s.tmux
        .run_checked(&["link-window", "-d", "-s", "work:0", "-t", "main:9"])
        .expect("link");
    count_once(&s, &c, pane, "THREE", 40).await;
    streams(&s, &c, pane, "LINK").await;
}

/// The session a pane streams from is killed while its window stays linked
/// into another session: the pane moves to that session's client, and its
/// mirror is rebuilt from a capture, so output written meanwhile is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_session_hands_its_linked_panes_to_another_client() {
    let s = two_more_sessions().await;
    let c = s.client().await;
    let pane = s.pane_of("work:");
    s.tmux
        .run_checked(&["link-window", "-d", "-s", "work:0", "-t", "more:9"])
        .expect("link");
    streams(&s, &c, pane, "BEFORE").await;
    s.tmux
        .run_checked(&["kill-session", "-t", "work"])
        .expect("kill");
    // Straight away, before the daemon can have noticed.
    s.type_line(pane, "echo DUR''ING");
    s.wait_tmux(pane, "DURING");
    wait_mirror(&c, pane, "DURING").await;
    wait_mirror(&c, pane, "BEFORE-OUT").await;
    streams(&s, &c, pane, "AFTER").await;
}

fn daemon_clients(s: &Setup) -> usize {
    s.tmux
        .run_checked(&["list-clients", "-F", "#{client_name}"])
        .unwrap_or_default()
        .lines()
        .count()
}

fn sessions(s: &Setup) -> usize {
    s.tmux
        .run_checked(&["list-sessions", "-F", "#{session_id}"])
        .unwrap_or_default()
        .lines()
        .count()
}

/// Sessions made and killed leave no control client behind: the daemon
/// has one per session, no more.
#[tokio::test(flavor = "multi_thread")]
async fn session_churn_leaves_one_client_per_session() {
    let s = Setup::new(&[]);
    let c = s.client().await;
    wait_live(&c, s.pane_of("main:")).await;
    for round in 0..4 {
        let names: Vec<String> = (0..3).map(|i| format!("churn{round}x{i}")).collect();
        for n in &names {
            s.tmux
                .run_checked(&["new-session", "-d", "-s", n, "/bin/sh"])
                .expect("session");
        }
        // Each new session streams.
        let last = s.pane_of(&format!("{}:", names[2]));
        wait_live(&c, last).await;
        for n in &names[..2] {
            s.tmux
                .run_checked(&["kill-session", "-t", n])
                .expect("kill");
        }
    }
    // Left: `main` and the last session of each round.
    assert_eq!(sessions(&s), 5);
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon_clients(&s) != sessions(&s) {
        assert!(
            Instant::now() < deadline,
            "{} clients for {} sessions:\n{}",
            daemon_clients(&s),
            sessions(&s),
            s.tmux
                .run_checked(&["list-clients", "-F", "#{client_name} #{session_name}"])
                .unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let survivor = s.pane_of("churn3x2:");
    streams(&s, &c, survivor, "LAST").await;
}
