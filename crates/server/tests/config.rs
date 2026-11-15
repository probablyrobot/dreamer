//! Live configuration through a real daemon: a valid change to
//! `dreamer.yaml` reaches every view of every pane at once, and a file
//! that does not read changes nothing.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    _tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new(settings: &str) -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::write(root.join("config/dreamer.yaml"), settings).expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
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
        Setup {
            root,
            _tmux: tmux,
            daemon,
        }
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

    fn write(&self, settings: &str) {
        std::fs::write(self.root.join("config/dreamer.yaml"), settings).expect("settings");
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("st/dreamerd.log")).unwrap_or_default()
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

fn hex(c: terminal_ghostty::core::Rgb) -> u32 {
    (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
}

/// The background a view of the pane shows, once it is `want`.
async fn wait_background(feed: &replica::feed::Feed, want: u32, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last = None;
    loop {
        if let Some(f) = feed.take() {
            last = Some(hex(f.colors.background));
            if last == Some(want) {
                return;
            }
        }
        if tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .is_err()
        {
            panic!(
                "{what}: the view never showed #{want:06x}; last #{:06x}",
                last.unwrap_or(0)
            );
        }
    }
}

#[tokio::test]
async fn a_theme_changed_in_the_file_reaches_every_view_and_a_broken_file_changes_nothing() {
    let s = Setup::new("terminal:\n  theme: dreamer-dark\n");
    let c = s.client().await;
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    let feed = replica::feed::Feed::start(s.root.join("rt/dreamerd.sock"), Some(pane), None)
        .expect("feed");
    wait_background(&feed, 0x14161a, "at start").await;

    // A valid change: every view takes the new colors, without a restart.
    s.write("terminal:\n  theme: nord\n");
    wait_background(&feed, 0x2e3440, "after the change").await;

    // A broken file: the settings in force stay, and the reason is logged.
    s.write("terminal:\n  theme: nord\nfont:\n  size: 999\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s.log().contains("configuration not applied") {
        assert!(Instant::now() < deadline, "never refused:\n{}", s.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(s.log().contains("font.size"), "{}", s.log());
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Some(f) = feed.take() {
        assert_eq!(
            hex(f.colors.background),
            0x2e3440,
            "kept the theme in force"
        );
    }
}
