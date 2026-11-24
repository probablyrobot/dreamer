//! A package running in a real tmux pane: it shows live data
//! from the daemon, runs a keybound command, shows its data as stale while
//! the daemon is gone and live again after it is back, and all of it is
//! what a plain tmux client sees.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

fn target_dir() -> PathBuf {
    PathBuf::from(APP)
        .parent()
        .expect("target dir")
        .to_path_buf()
}

const MANIFEST: &str = r#"
schema_version = 1
id = "test.panes"
version = "0.1.0"
title = "Panes"
capabilities = ["tmux.topology", "actions.submit"]

[data.panes]
method = "tmux.topology"
path = "panes"
events = ["tmux.topology"]

[view]
type = "table"
source = "panes"
row_key = "id"
columns = ["id", "window", "current_command"]

[commands.rename]
key = "r"
label = "rename"
method = "actions.submit"
params = { action = { kind = { rename_window = { window = "${selected.window}", name = "renamed-${selected.id}" } } } }
"#;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: std::cell::RefCell<Option<std::process::Child>>,
}

impl Setup {
    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "DREAMER_RUNTIME_DIR",
                self.root.join("rt").display().to_string(),
            ),
            (
                "DREAMER_STATE_DIR",
                self.root.join("st").display().to_string(),
            ),
            ("DREAMER_FORBID_DEFAULT_TMUX", "1".into()),
        ]
    }

    fn start_daemon(&self) {
        let daemon = target_dir().join("dreamerd");
        assert!(
            daemon.exists(),
            "{} is missing; build dreamer-server",
            daemon.display()
        );
        let child = Command::new(daemon)
            .args([
                "--background",
                "--tmux-socket",
                self.tmux.socket(),
                "--tmux",
            ])
            .arg(tmux_program())
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        *self.daemon.borrow_mut() = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.root.join("rt/dreamerd.sock").exists() {
            assert!(Instant::now() < deadline, "daemon never started");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the daemon (SIGTERM) and reap it; `false` if it would not stop.
    fn stop_daemon(&self) -> bool {
        let Some(mut child) = self.daemon.borrow_mut().take() else {
            return true;
        };
        let _ = Command::new("kill").arg(child.id().to_string()).status();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                return true;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn screen(&self, pane: &str) -> String {
        self.tmux
            .run_checked(&["capture-pane", "-p", "-t", pane])
            .unwrap_or_default()
    }

    fn wait(&self, pane: &str, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = self.screen(pane);
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "never {what}:\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_package_runs_in_a_real_pane_and_survives_a_daemon_restart() {
    let root = PathBuf::from(format!("/tmp/drm-tapp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let manifest = root.join("app.toml");
    std::fs::write(&manifest, MANIFEST).expect("manifest");
    let s = Setup {
        root,
        tmux: TmuxServer::start(120, 30, "/bin/sh").expect("tmux"),
        daemon: std::cell::RefCell::new(None),
    };
    let check = Command::new(APP)
        .arg("--check")
        .arg(&manifest)
        .output()
        .expect("check");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    s.start_daemon();
    // The app is the pane's program, started like any other.
    let env: Vec<String> = s
        .env()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut args = vec!["new-window", "-d", "-P", "-F", "#{pane_id}", "env"];
    args.extend(env.iter().map(String::as_str));
    let manifest_s = manifest.display().to_string();
    args.extend([APP, manifest_s.as_str()]);
    let pane = s
        .tmux
        .run_checked(&args)
        .expect("new-window")
        .trim()
        .to_owned();

    // Live data: both panes, with the app's own command shown.
    let shown = s.wait(&pane, "showed live data", |t| {
        t.contains("[live]") && t.contains("dreamer-termapp")
    });
    assert!(shown.contains(" Panes "), "{shown}");
    let rows = shown
        .lines()
        .filter(|l| l.contains("sh") || l.contains("dreamer-termapp"))
        .count();
    assert!(rows >= 2, "{shown}");

    // A keybound command acts on the selected row (the first pane, in
    // window 0 here).
    s.tmux
        .run_checked(&["send-keys", "-t", &pane, "r"])
        .expect("key");
    s.wait(&pane, "ran the command", |t| t.contains("rename: done"));
    let windows = s
        .tmux
        .run_checked(&["list-windows", "-a", "-F", "#{window_name}"])
        .expect("windows");
    assert!(
        windows.lines().any(|w| w.starts_with("renamed-")),
        "{windows}"
    );

    // The daemon goes away: last data stays, marked stale.
    assert!(s.stop_daemon(), "the daemon stopped");
    let stale = s.wait(&pane, "went stale", |t| t.contains("[stale"));
    assert!(
        stale.contains("dreamer-termapp"),
        "the last data is still shown:\n{stale}"
    );
    // A command now is not sent.
    s.tmux
        .run_checked(&["send-keys", "-t", &pane, "r"])
        .expect("key");
    s.wait(&pane, "refused to send", |t| {
        t.contains("not sent (disconnected)")
    });
    // Back: live again, with fresh data.
    s.start_daemon();
    s.wait(&pane, "came back live", |t| t.contains("[live]"));

    // q quits and gives the terminal back.
    s.tmux
        .run_checked(&["send-keys", "-t", &pane, "q"])
        .expect("key");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let cmd = s
            .tmux
            .run_checked(&["display", "-p", "-t", &pane, "#{pane_dead}"])
            .unwrap_or_default();
        if cmd.trim() == "1" || cmd.trim().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the app did not quit");
        std::thread::sleep(Duration::from_millis(100));
    }
}
