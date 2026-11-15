//! The shared app model (`app-model`) against a real daemon: what both
//! presentations, the pane app and the desktop's native panel, rely on.
//! Rows are published once connected, a key moves the selection, a
//! command's key runs its typed action, and Quit ends the run. A narrowed
//! connection can do only what the package declares.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use app_model::{App, Endpoint, Input, Link};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

const MANIFEST: &str = r#"schema_version = 1
id = "test.windows"
version = "0.1.0"
title = "Windows"
capabilities = ["tmux.topology", "actions.submit"]

[data.windows]
method = "tmux.topology"
path = "windows"
events = ["tmux.topology"]

[view]
type = "table"
source = "windows"
row_key = "id"
columns = ["id", "name"]

[commands.rename]
key = "r"
method = "actions.submit"
params = { action = { kind = { rename_window = { window = "${selected.id}", name = "from-the-model" } } } }
"#;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-appm-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["new-window", "-d", "-n", "second", "/bin/sh"])
            .expect("second window");
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("rt/dreamerd.sock").exists() {
            assert!(Instant::now() < deadline, "daemon never started");
            std::thread::sleep(Duration::from_millis(50));
        }
        Setup { root, tmux, daemon }
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

/// Wait for a published model that satisfies `want`.
fn until(models: &mpsc::Receiver<App>, what: &str, want: impl Fn(&App) -> bool) -> App {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match models.recv_timeout(left) {
            Ok(app) if want(&app) => return app,
            Ok(_) => {}
            Err(e) => panic!("never published {what}: {e}"),
        }
    }
}

fn names(app: &App) -> Vec<String> {
    app.rows
        .iter()
        .map(|r| app_model::text(&r["name"]))
        .collect()
}

#[test]
fn the_model_runs_a_package_for_any_presentation() {
    let s = Setup::new();
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, MANIFEST).expect("manifest");
    let (inputs, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (models_tx, models) = mpsc::channel();
    let endpoint = Endpoint {
        socket: s.root.join("rt/dreamerd.sock"),
        role: "test",
        ticket: false,
        narrow: true,
    };
    let runner = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut publish = |app: &App| {
            let _ = models_tx.send(app.clone());
            Ok(())
        };
        rt.block_on(app_model::run(manifest, endpoint, &mut rx, &mut publish))
    });

    let first = until(&models, "two windows, live", |a| {
        a.link == Link::Live && a.rows.len() == 2
    });
    assert_eq!(first.selected, 0);
    assert!(names(&first).contains(&"second".to_owned()), "{first:?}");

    // A key moves the selection; the selected window's id goes into the
    // command's typed action.
    inputs.send(Input::Key("Down".into())).expect("send");
    let moved = until(&models, "the selection on the second row", |a| {
        a.selected == 1
    });
    let id = app_model::text(&moved.rows[1]["id"]);
    inputs.send(Input::Key("r".into())).expect("send");
    let renamed = until(&models, "the renamed window", |a| {
        names(a).contains(&"from-the-model".to_owned())
    });
    assert_eq!(renamed.status.as_deref(), Some("rename: done"));
    let shown = s
        .tmux
        .run_checked(&["list-windows", "-F", "#{window_id} #{window_name}"])
        .expect("list");
    assert!(
        shown.lines().any(|l| l == format!("@{id} from-the-model")),
        "{shown}"
    );

    inputs.send(Input::Quit).expect("send");
    assert_eq!(runner.join().expect("runner"), Ok(()));
}

#[test]
fn a_narrowed_package_can_do_only_what_it_declares() {
    let s = Setup::new();
    let manifest = s.root.join("app.toml");
    // The same package without `actions.submit` among its capabilities.
    std::fs::write(
        &manifest,
        MANIFEST.replace(
            r#"["tmux.topology", "actions.submit"]"#,
            r#"["tmux.topology"]"#,
        ),
    )
    .expect("manifest");
    let (inputs, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (models_tx, models) = mpsc::channel();
    let endpoint = Endpoint {
        socket: s.root.join("rt/dreamerd.sock"),
        role: "test",
        ticket: false,
        narrow: true,
    };
    let runner = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut publish = |app: &App| {
            let _ = models_tx.send(app.clone());
            Ok(())
        };
        rt.block_on(app_model::run(manifest, endpoint, &mut rx, &mut publish))
    });
    until(&models, "two windows, live", |a| {
        a.link == Link::Live && a.rows.len() == 2
    });
    inputs.send(Input::Key("r".into())).expect("send");
    let refused = until(&models, "the command's outcome", |a| {
        a.status
            .as_deref()
            .is_some_and(|s| s.starts_with("rename: "))
    });
    let said = refused.status.unwrap_or_default();
    assert!(said.contains("actions.submit"), "{said}");
    let shown = s
        .tmux
        .run_checked(&["list-windows", "-F", "#{window_name}"])
        .expect("list");
    assert!(!shown.contains("from-the-model"), "{shown}");
    inputs.send(Input::Quit).expect("send");
    assert_eq!(runner.join().expect("runner"), Ok(()));
}
