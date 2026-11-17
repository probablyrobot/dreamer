//! The example process app (`examples/pane-app.rs`) in a real pane: the
//! app host runs it, a desktop follows its tree through the daemon
//! (`apps.tree`) and sends it input (`apps.input`), the plain tmux client
//! sees the same layout as a TUI, a call outside its capabilities is
//! refused, and a manifest without the `process` capability is refused.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

/// `target/<profile>`, from this test executable in `target/<profile>/deps`.
fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("exe");
    exe.parent()
        .and_then(|deps| deps.parent())
        .expect("target dir")
        .to_path_buf()
}

fn built(name: &str) -> PathBuf {
    let p = target_dir().join(name);
    assert!(
        p.exists(),
        "{} is missing; build dreamer-server, dreamer-termapp, dreamer-cli, and this crate's examples first",
        p.display()
    );
    p
}

fn manifest(example: &Path, with_capability: bool) -> String {
    let caps = if with_capability {
        r#"["process", "tmux.topology"]"#
    } else {
        r#"["tmux.topology"]"#
    };
    format!(
        r#"schema_version = 2
id = "test.pane-app"
version = "0.1.0"
title = "Pane app"
capabilities = {caps}

[process]
command = ["{}"]
"#,
        example.display()
    )
}

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg(self.daemon.id().to_string())
            .status();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn setup() -> Setup {
    let root = PathBuf::from(format!("/tmp/drm-papp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pkg")).expect("root");
    let tmux = TmuxServer::start(120, 32, "/bin/sh").expect("tmux");
    let daemon = Command::new(built("dreamerd"))
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("rt/dreamerd.sock").exists() {
        assert!(Instant::now() < deadline, "daemon never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    Setup { root, tmux, daemon }
}

async fn call(c: &client::Client, r: Request) -> Response {
    c.call(r).await.expect("call")
}

/// The desktop's copy of the pane's tree, read until `ok` holds for it.
async fn tree_until(
    c: &client::Client,
    pane: u32,
    mirror: &mut view_tree::Mirror,
    after: &mut u64,
    what: &str,
    ok: impl Fn(&view_tree::Frame) -> bool,
) -> view_tree::Frame {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Response::AppsTree(t)) = c
            .call(Request::AppsTree(api::AppTreeQuery {
                pane,
                after: *after,
                seq: mirror.seq,
                wait_ms: Some(300),
            }))
            .await
        {
            *after = t.revision;
            if mirror.take(t.seq, &t.tree, &t.patches).is_err() {
                *mirror = view_tree::Mirror::default();
                *after = 0;
            }
        }
        if let Some(f) = mirror.frame.as_ref().filter(|f| ok(f)) {
            return f.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the tree never {what}: {:?}",
            mirror.frame
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn screen_until(s: &Setup, pane: u32, what: &str, ok: impl Fn(&str) -> bool) -> String {
    let target = format!("%{pane}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = s
            .tmux
            .run_checked(&["capture-pane", "-p", "-t", &target])
            .unwrap_or_default();
        if ok(&screen) {
            return screen;
        }
        assert!(Instant::now() < deadline, "the TUI never {what}:\n{screen}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn gauge_label(f: &view_tree::Frame) -> Option<String> {
    match &f.root.find("level")?.kind {
        view_tree::Kind::Gauge { label, .. } => Some(label.clone()),
        _ => None,
    }
}

#[test]
fn the_example_runs_as_a_pane_app_and_as_a_tui() {
    let example = built("examples/pane-app");
    let termapp = built("dreamer-termapp");
    let s = setup();
    let path = s.root.join("pkg/app.toml");
    std::fs::write(&path, manifest(&example, true)).expect("manifest");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let c = client::Client::connect(&s.root.join("rt/dreamerd.sock"), "test")
            .await
            .expect("connect");
        let Response::TmuxTopology(t) = call(&c, Request::TmuxTopology(Default::default())).await
        else {
            panic!("not a topology");
        };
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "test.pane-app".into(),
                grant: Some(api::Grant {
                    methods: vec!["tmux.topology".into()],
                    all_panes: true,
                    ..api::Grant::default()
                }),
                argv: vec![termapp.display().to_string(), path.display().to_string()],
                session: t.sessions[0].id,
                ..api::LaunchApp::default()
            }),
        )
        .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;

        // The desktop's view: the app's tree, with the panes it read
        // through the host under its grant.
        let mut mirror = view_tree::Mirror::default();
        let mut after = 0;
        let frame = tree_until(&c, pane, &mut mirror, &mut after, "showed panes", |f| {
            f.root.find("panes").is_some_and(|n| !n.children.is_empty())
        })
        .await;
        let outline: Vec<String> = frame
            .outline()
            .lines()
            .map(|l| l.split(" (").next().unwrap_or(l).to_owned())
            .collect();
        assert_eq!(
            outline,
            [
                "column main",
                "  split top",
                "    table panes",
                "    column side",
                "      gauge level",
                "      chart history",
                "  log events",
            ]
        );
        assert_eq!(frame.title, "Pane app");
        assert_eq!(frame.focus.as_deref(), Some("panes"));
        assert!(frame.status.contains("[live]"), "{}", frame.status);
        // The topology marks the pane as this app's.
        let Response::TmuxTopology(t) = call(&c, Request::TmuxTopology(Default::default())).await
        else {
            panic!("not a topology");
        };
        let app = t
            .panes
            .iter()
            .find(|p| p.id == pane)
            .and_then(|p| p.app.clone())
            .expect("an app pane");
        assert_eq!(app.app_id, "test.pane-app");

        // The terminal draws the same layout: the table left of the
        // counter, the events below both.
        let screen = screen_until(&s, pane, "drew the layout", |screen| {
            screen.contains(" Panes ") && screen.contains(" Count ") && screen.contains(" Events ")
        });
        let place = |what: &str| {
            screen
                .lines()
                .enumerate()
                .find_map(|(y, l)| l.find(what).map(|b| (l[..b].chars().count(), y)))
                .expect("shown")
        };
        let (px, py) = place(" Panes ");
        let (cx, cy) = place(" Count ");
        let (_, ey) = place(" Events ");
        assert!(px < cx && py == cy && ey > py, "{screen}");
        assert!(screen.contains(" Pane app "), "{screen}");

        // Keys from the desktop change the app's state: the desktop gets
        // the change as a patch, and the terminal shows it.
        for _ in 0..3 {
            call(
                &c,
                Request::AppsInput(api::AppInput {
                    pane,
                    key: "+".into(),
                    ..api::AppInput::default()
                }),
            )
            .await;
        }
        tree_until(&c, pane, &mut mirror, &mut after, "counted", |f| {
            gauge_label(f).as_deref() == Some("count 3")
        })
        .await;
        screen_until(&s, pane, "counted", |screen| screen.contains("count 3"));
        // Keys typed in the pane itself reach the app the same way.
        s.tmux
            .run_checked(&["send-keys", "-t", &format!("%{pane}"), "-"])
            .expect("send-keys");
        tree_until(
            &c,
            pane,
            &mut mirror,
            &mut after,
            "took the typed key",
            |f| gauge_label(f).as_deref() == Some("count 2"),
        )
        .await;

        // A call outside the manifest's capabilities is refused by the
        // host; the app shows the refusal.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "x".into(),
                ..api::AppInput::default()
            }),
        )
        .await;
        let f = tree_until(&c, pane, &mut mirror, &mut after, "was refused", |f| {
            f.status.contains("forbidden: refused")
        })
        .await;
        assert!(
            f.status
                .contains("`daemon.status` is not in this app's capabilities"),
            "{}",
            f.status
        );

        // `q` reaches the app, which quits; the host ends with it.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "q".into(),
                ..api::AppInput::default()
            }),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let Response::TmuxTopology(t) =
                call(&c, Request::TmuxTopology(Default::default())).await
            else {
                panic!("not a topology");
            };
            if !t.panes.iter().any(|p| p.id == pane) {
                break;
            }
            assert!(Instant::now() < deadline, "the pane never closed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

#[test]
fn without_the_process_capability_the_app_is_refused() {
    let example = built("examples/pane-app");
    let termapp = built("dreamer-termapp");
    let dream = built("dream");
    let root = PathBuf::from(format!("/tmp/drm-papp-refused-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let path = root.join("app.toml");
    std::fs::write(&path, manifest(&example, false)).expect("manifest");
    let why = "running a program needs the `process` capability";

    // At check, and so at install: `dream app check`.
    let out = Command::new(&dream)
        .args(["app", "check"])
        .arg(&root)
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .output()
        .expect("dream");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(why),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // At run: the host refuses to start it, and starts nothing.
    let out = Command::new(&termapp)
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .expect("termapp");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(why),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With the capability, the same package checks.
    std::fs::write(&path, manifest(&example, true)).expect("manifest");
    let out = Command::new(&termapp)
        .args(["--check"])
        .arg(&path)
        .output()
        .expect("termapp");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
