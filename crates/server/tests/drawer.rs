//! The harness drawer through a real daemon and an isolated tmux: the
//! configured harness starts in a pane docked at the bottom, hiding moves
//! it to the holding session and showing brings the same pane back, each
//! session has its own, a restarted daemon finds it again, a small window
//! gets it zoomed, and the tier policy never puts it to sleep.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{DrawerWant, Request, Response};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

const SETTINGS: &str = "agents:
  default: pi
  pi:
    command: ['/bin/sh', '-c', 'echo HARNESS $PI_CODING_AGENT_DIR; exec sleep 600']
tiers:
  cold_after: never
  warm_limit: none
";

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new(settings: &str) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-drawer-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::write(root.join("config/dreamer.yaml"), settings).expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let daemon = Setup::daemon(&root, &tmux);
        Setup { root, tmux, daemon }
    }

    fn daemon(root: &std::path::Path, tmux: &TmuxServer) -> Child {
        Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_DATA_DIR", root.join("data"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd")
    }

    fn stop_daemon(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while platform::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }

    fn restart(&mut self) {
        self.stop_daemon();
        self.daemon = Setup::daemon(&self.root, &self.tmux);
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
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn topology(c: &Client) -> api::Topology {
    match c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    {
        Ok(Response::TmuxTopology(t)) => t,
        other => panic!("topology: {other:?}"),
    }
}

fn session_id(t: &api::Topology, name: &str) -> u32 {
    t.sessions
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no session {name}: {t:?}"))
        .id
}

fn window_of_session(t: &api::Topology, session: u32) -> &api::Window {
    let s = t
        .sessions
        .iter()
        .find(|s| s.id == session)
        .expect("session");
    t.windows
        .iter()
        .find(|w| Some(w.id) == s.active_window)
        .expect("window")
}

async fn drawer(c: &Client, session: u32, want: DrawerWant) -> api::DrawerStatus {
    let req = api::DrawerRequest {
        session: Some(session),
        want: want as i32,
        ..api::DrawerRequest::default()
    };
    let r = if want == DrawerWant::Unspecified {
        c.call(Request::DrawerStatus(req)).await
    } else {
        c.call(Request::DrawerToggle(req)).await
    };
    match r {
        Ok(Response::DrawerToggle(s) | Response::DrawerStatus(s)) => s,
        other => panic!("drawer: {other:?}"),
    }
}

async fn status(c: &Client, session: u32) -> api::DrawerStatus {
    drawer(c, session, DrawerWant::Unspecified).await
}

async fn screen(c: &Client, pane: u32) -> String {
    match c
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(100),
            endpoint: None,
        }))
        .await
    {
        Ok(Response::TerminalRead(t)) => t.lines.join("\n"),
        other => panic!("read: {other:?}"),
    }
}

async fn until_text(c: &Client, pane: u32, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = screen(c, pane).await;
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "%{pane} never showed {needle:?}: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_drawer_docks_hides_and_comes_back_as_the_same_running_pane() {
    let mut s = Setup::new(SETTINGS);
    // A window with a split, and something to keep in its panes.
    s.tmux
        .run_checked(&["split-window", "-h", "-t", "main:", "/bin/sh"])
        .expect("split");
    s.tmux
        .run_checked(&["send-keys", "-t", "main:.0", "echo LEFT''MARK", "Enter"])
        .expect("keys");
    s.tmux
        .run_checked(&["select-pane", "-t", "main:.0"])
        .expect("select");
    let c = s.client().await;
    let t = topology(&c).await;
    let main = session_id(&t, "main");
    let w = window_of_session(&t, main).clone();
    let left = w.panes[0];
    let before = w.active_pane;
    until_text(&c, left, "LEFTMARK").await;
    assert!(
        status(&c, main).await.pane.is_none(),
        "nothing until the first use"
    );

    // First toggle: the harness starts, docked at the bottom and focused.
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(d.created && d.shown && d.focused && !d.zoomed, "{d:?}");
    assert_eq!(d.harness, "pi");
    assert_eq!(d.window, Some(w.id));
    let pane = d.pane.expect("drawer pane");
    let text = until_text(&c, pane, "HARNESS").await;
    let profile = s.root.join("data/pi-profile");
    assert!(text.contains(&profile.display().to_string()), "{text}");
    assert!(profile.is_dir(), "the pi profile directory is created");
    let t = topology(&c).await;
    let p = t
        .panes
        .iter()
        .find(|p| p.id == pane)
        .expect("drawer in topology");
    assert_eq!(p.drawer_of, Some(main));
    assert_eq!(p.width, 100, "across the whole window");
    assert!((9..=11).contains(&p.height), "35% of 30 rows: {}", p.height);
    assert_eq!(p.top + p.height, 30, "at the bottom");
    let pid = p.pid;
    assert_eq!(window_of_session(&t, main).active_pane, Some(pane));

    // Focused: the same key hides it into the holding session.
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(!d.shown && !d.focused, "{d:?}");
    assert_eq!(d.pane, Some(pane));
    let t = topology(&c).await;
    let hold = t
        .sessions
        .iter()
        .find(|x| x.holding)
        .expect("holding session");
    assert_eq!(hold.name, "_dreamer_drawers");
    assert!(t.sessions.iter().filter(|x| x.holding).count() == 1);
    let ww = window_of_session(&t, main);
    assert_eq!(ww.panes.len(), 2, "the window has its own panes again");
    assert_eq!(ww.active_pane, before, "focus goes back where it was");
    assert!(
        hold.windows
            .contains(&t.panes.iter().find(|p| p.id == pane).expect("kept").window),
        "hidden in the holding session"
    );
    until_text(&c, left, "LEFTMARK").await;

    // Shown again: the same pane, still running, at the bottom.
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(d.shown && d.focused && !d.created, "{d:?}");
    assert_eq!(d.pane, Some(pane));
    let t = topology(&c).await;
    let p = t.panes.iter().find(|p| p.id == pane).expect("drawer");
    assert_eq!(p.pid, pid, "the harness kept running while hidden");
    assert_eq!(p.top + p.height, 30);
    until_text(&c, pane, "HARNESS").await;

    // Shown but not focused: the key focuses it.
    s.tmux
        .run_checked(&["select-pane", "-t", &format!("%{left}")])
        .expect("select");
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(d.shown && d.focused, "{d:?}");

    // A second session gets a drawer of its own.
    s.tmux
        .run_checked(&["new-session", "-d", "-s", "second", "-x", "100", "-y", "30"])
        .expect("second session");
    let t = topology(&c).await;
    let second = session_id(&t, "second");
    let d2 = drawer(&c, second, DrawerWant::Show).await;
    assert!(d2.created && d2.shown, "{d2:?}");
    assert_ne!(d2.pane, Some(pane));
    drawer(&c, second, DrawerWant::Hide).await;

    // A restarted daemon finds the drawer from tmux itself.
    drop(c);
    s.restart();
    let c = s.client().await;
    let d = status(&c, main).await;
    assert_eq!(d.pane, Some(pane), "{d:?}");
    assert!(d.shown);
    assert_eq!(d.harness, "pi");
    let d = drawer(&c, main, DrawerWant::Hide).await;
    assert!(!d.shown && d.pane == Some(pane), "{d:?}");
    let d = status(&c, second).await;
    assert!(!d.shown && d.pane == d2.pane, "{d:?}");

    // New: a fresh harness replaces the old pane.
    let d = drawer(&c, main, DrawerWant::New).await;
    assert!(d.created && d.shown, "{d:?}");
    assert_ne!(d.pane, Some(pane));
    let t = topology(&c).await;
    assert!(
        !t.panes.iter().any(|p| p.id == pane),
        "the old drawer is gone"
    );
}

/// The drawer knows the pane you came from: the one focused when it was
/// shown, or the one focused when the key moved focus to it, still known
/// once it is hidden.
#[tokio::test(flavor = "multi_thread")]
async fn the_drawer_records_the_pane_you_came_from() {
    let s = Setup::new(SETTINGS);
    s.tmux
        .run_checked(&["split-window", "-h", "-t", "main:", "/bin/sh"])
        .expect("split");
    s.tmux
        .run_checked(&["select-pane", "-t", "main:.0"])
        .expect("select");
    let c = s.client().await;
    let t = topology(&c).await;
    let main = session_id(&t, "main");
    let w = window_of_session(&t, main).clone();
    let (left, right) = (w.panes[0], w.panes[1]);
    assert_eq!(w.active_pane, Some(left));

    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(d.shown && d.focused, "{d:?}");
    assert_eq!(d.from_pane, Some(left), "{d:?}");
    assert_eq!(status(&c, main).await.from_pane, Some(left));

    // From the right pane, the key focuses the drawer: you came from there.
    s.tmux
        .run_checked(&["select-pane", "-t", &format!("%{right}")])
        .expect("select");
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(d.focused, "{d:?}");
    assert_eq!(d.from_pane, Some(right), "{d:?}");

    // Hidden: focus goes back there, and the drawer still names it.
    let d = drawer(&c, main, DrawerWant::Toggle).await;
    assert!(!d.shown, "{d:?}");
    assert_eq!(d.from_pane, Some(right), "{d:?}");
    let t = topology(&c).await;
    assert_eq!(window_of_session(&t, main).active_pane, Some(right));

    // A pane that is gone is not named.
    s.tmux
        .run_checked(&["kill-pane", "-t", &format!("%{right}")])
        .expect("kill");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let d = status(&c, main).await;
        if d.from_pane.is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "{d:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_window_too_small_to_dock_shows_the_drawer_zoomed() {
    let s = Setup::new(SETTINGS);
    s.tmux
        .run_checked(&["new-session", "-d", "-s", "tiny", "-x", "80", "-y", "12"])
        .expect("tiny");
    let c = s.client().await;
    let tiny = session_id(&topology(&c).await, "tiny");
    let d = drawer(&c, tiny, DrawerWant::Toggle).await;
    assert!(d.shown && d.zoomed && d.focused, "{d:?}");
    let t = topology(&c).await;
    assert!(window_of_session(&t, tiny).zoomed);
    let d = drawer(&c, tiny, DrawerWant::Toggle).await;
    assert!(!d.shown, "{d:?}");
    let t = topology(&c).await;
    let w = window_of_session(&t, tiny);
    assert!(!w.zoomed && w.panes.len() == 1, "{w:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_drawer_is_never_put_to_sleep() {
    let s = Setup::new(
        "agents:
  default: pi
  pi:
    command: ['/bin/sh', '-c', 'echo HARNESS; exec sleep 600']
tiers:
  cold_after: 1s
",
    );
    s.tmux
        .run_checked(&["new-window", "-d", "-t", "main:", "/bin/sh"])
        .expect("window");
    let c = s.client().await;
    let main = session_id(&topology(&c).await, "main");
    let d = drawer(&c, main, DrawerWant::Show).await;
    let pane = d.pane.expect("drawer");
    drawer(&c, main, DrawerWant::Hide).await;
    let t = topology(&c).await;
    let other = t
        .panes
        .iter()
        .find(|p| {
            p.id != pane && p.drawer_of.is_none() && p.window != window_of_session(&t, main).id
        })
        .expect("a pane in the background window")
        .id;
    // The quiet background pane goes Cold; the hidden drawer does not.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let Ok(Response::TerminalMirrors(l)) = c
            .call(Request::TerminalMirrors(api::TopologyParams::default()))
            .await
        else {
            panic!("mirrors")
        };
        let state = |p: u32| l.mirrors.iter().find(|m| m.pane == p).map(|m| m.state);
        if state(other) == Some(api::MirrorState::Cold as i32) {
            assert_eq!(state(pane), Some(api::MirrorState::Live as i32), "{l:?}");
            break;
        }
        assert!(Instant::now() < deadline, "nothing went Cold: {l:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
