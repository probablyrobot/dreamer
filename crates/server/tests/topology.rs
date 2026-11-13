//! Topology push (spec 15.1): after `tmux.watch`, a client follows every
//! change, including ones made by an ordinary tmux client, without polling.

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
            "/tmp/drm-tp-{}-{}",
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

    /// An ordinary tmux client command (not through the daemon).
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

/// The next `tmux.topology` event; `None` if none arrives within `wait`.
async fn next_update(rx: &mut Receiver<Pushed>, wait: Duration) -> Option<api::TopologyUpdate> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "tmux.topology" => {
                return api::TopologyUpdate::decode(payload.as_slice()).ok();
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Wait for an update satisfying `want`; returns it.
async fn until(
    rx: &mut Receiver<Pushed>,
    what: &str,
    want: impl Fn(&api::Topology) -> bool,
) -> api::TopologyUpdate {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let u = next_update(rx, left)
            .await
            .unwrap_or_else(|| panic!("no topology event for: {what}"));
        if u.topology.as_ref().is_some_and(&want) {
            return u;
        }
    }
}

#[tokio::test]
async fn watchers_follow_changes_made_by_another_tmux_client() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed events");
    let Response::TmuxWatch(first) = c
        .call(Request::TmuxWatch(api::TopologyParams::default()))
        .await
        .expect("watch")
    else {
        panic!("watch")
    };
    let topo = first.topology.expect("topology");
    assert_eq!(topo.panes.len(), 1);
    let mut revision = first.revision;

    let mut step = |u: api::TopologyUpdate| {
        assert!(u.revision > revision, "revisions must increase");
        revision = u.revision;
        u.topology.expect("topology")
    };

    s.tmux(&["split-window", "-t", "main", "/bin/sh"]);
    let t = step(until(&mut rx, "split", |t| t.panes.len() == 2).await);
    assert_eq!(t.windows[0].panes.len(), 2);

    s.tmux(&["rename-window", "-t", "main", "renamed"]);
    step(
        until(&mut rx, "rename", |t| {
            t.windows.iter().any(|w| w.name == "renamed")
        })
        .await,
    );

    s.tmux(&["resize-pane", "-Z", "-t", "main"]);
    step(until(&mut rx, "zoom", |t| t.windows.iter().any(|w| w.zoomed)).await);

    s.tmux(&["new-window", "-t", "main", "-n", "second", "/bin/sh"]);
    let t = step(until(&mut rx, "new window", |t| t.windows.len() == 2).await);
    let second = t
        .windows
        .iter()
        .find(|w| w.name == "second")
        .expect("second")
        .id;
    let session = &t.sessions[0];
    assert_eq!(session.active_window, Some(second), "new-window selects it");

    s.tmux(&["select-window", "-t", "main:0"]);
    step(
        until(&mut rx, "window switch", |t| {
            t.sessions[0].active_window != Some(second)
        })
        .await,
    );

    let pane = t
        .panes
        .iter()
        .find(|p| p.window == second)
        .expect("pane")
        .id;
    let generation = t
        .panes
        .iter()
        .find(|p| p.id == pane)
        .expect("pane")
        .generation;
    s.tmux(&["respawn-pane", "-k", "-t", &format!("%{pane}"), "/bin/sh"]);
    step(
        until(&mut rx, "respawn", |t| {
            t.panes
                .iter()
                .any(|p| p.id == pane && p.generation > generation)
        })
        .await,
    );

    s.tmux(&["kill-window", "-t", "main:second"]);
    let t = step(until(&mut rx, "kill", |t| t.windows.len() == 1).await);

    // What was pushed is what the daemon reports.
    let Response::TmuxTopology(now) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology")
    else {
        panic!("topology")
    };
    assert_eq!(now, t);
}

#[tokio::test]
async fn a_burst_of_changes_coalesces_to_the_final_state() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed events");
    let Response::TmuxWatch(_) = c
        .call(Request::TmuxWatch(api::TopologyParams::default()))
        .await
        .expect("watch")
    else {
        panic!("watch")
    };
    const N: usize = 30;
    for i in 0..N {
        s.tmux(&["rename-window", "-t", "main", &format!("w{i}")]);
    }
    let last = format!("w{}", N - 1);
    let mut events = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let u = next_update(&mut rx, deadline.saturating_duration_since(Instant::now()))
            .await
            .expect("the final name never arrived");
        events += 1;
        if u.topology.is_some_and(|t| t.windows[0].name == last) {
            break;
        }
    }
    // Nothing older arrives after the final state.
    assert!(
        next_update(&mut rx, Duration::from_millis(300))
            .await
            .is_none()
    );
    assert!(events < N, "{events} events for {N} renames: not coalesced");
}

async fn size(c: &Client, window: u32, client: u64, cols: u32, rows: u32) -> api::SizeResult {
    match c
        .call(Request::TmuxSizeWindow(api::SizeWindow {
            window,
            cols,
            rows,
            client,
            ..api::SizeWindow::default()
        }))
        .await
        .expect("size")
    {
        Response::TmuxSizeWindow(r) => r,
        other => panic!("{other:?}"),
    }
}

/// The window's size as tmux reports it.
fn window_size(s: &Setup) -> (u32, u32) {
    let out = s
        .tmux
        .run_checked(&[
            "display-message",
            "-p",
            "-t",
            "main:0",
            "#{window_width}x#{window_height}",
        ])
        .expect("size");
    let (w, h) = out.trim().split_once('x').expect("WxH");
    (w.parse().expect("w"), h.parse().expect("h"))
}

async fn settle_to(s: &Setup, want: (u32, u32)) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while window_size(s) != want {
        assert!(
            Instant::now() < deadline,
            "window never became {want:?}: {:?}",
            window_size(s)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn one_client_owns_a_window_s_size_and_ownership_passes_on_disconnect() {
    let s = Setup::new();
    // The default sizing rule of a normal tmux setup.
    s.tmux(&["set-option", "-g", "window-size", "latest"]);
    let mut a = s.client().await;
    let b = s.client().await;
    let Response::TmuxWatch(t) = a
        .call(Request::TmuxWatch(api::TopologyParams::default()))
        .await
        .expect("watch")
    else {
        panic!("watch")
    };
    let window = t.topology.expect("topology").windows[0].id;
    drop(a.pushed());

    let r = size(&a, window, 1, 110, 33).await;
    assert!(r.owner && r.applied, "{r:?}");
    settle_to(&s, (110, 33)).await;

    // Another GUI of a different size is a follower: nothing changes.
    let r = size(&b, window, 2, 80, 24).await;
    assert!(!r.owner && !r.applied && r.owner_client == Some(1), "{r:?}");

    // No oscillation: the size holds while nothing asks for a change.
    for _ in 0..20 {
        assert_eq!(window_size(&s), (110, 33));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Another tmux client of its own size attaches and is active. tmux's
    // `window-size latest` decides; Dreamer never answers by re-sizing, so
    // the window settles and then stays put.
    let mut other = std::process::Command::new(tmux_program())
        .args(["-L", s.tmux.socket(), "-C", "attach", "-t", "main"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("another tmux client");
    {
        use std::io::Write;
        let stdin = other.stdin.as_mut().expect("stdin");
        writeln!(stdin, "refresh-client -C 70x20").expect("size the other client");
        writeln!(stdin, "select-window -t main:0").expect("be active");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let settled = window_size(&s);
    for _ in 0..30 {
        assert_eq!(window_size(&s), settled, "the window size oscillates");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("sizing: with another tmux client at 70x20 the window settled at {settled:?}");
    let _ = other.kill();
    let _ = other.wait();

    // The owner goes away: the follower takes over with its last request.
    drop(a);
    settle_to(&s, (80, 24)).await;
    let r = size(&b, window, 2, 90, 28).await;
    assert!(r.owner && r.applied, "{r:?}");
    settle_to(&s, (90, 28)).await;
}
