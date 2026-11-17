//! The reference app's input components in a real pane: input from a
//! desktop (`apps.input`) types into the input, clicks press, pick, and
//! flip, and each event runs its command in the host, whose outcome the
//! desktop's copy of the tree and the plain tmux client both show.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use view_tree::Kind;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

fn reference_app() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/components/app.toml")
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
    let root = PathBuf::from(format!("/tmp/drm-comp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tmux = TmuxServer::start(120, 34, "/bin/sh").expect("tmux");
    let daemon = PathBuf::from(APP)
        .parent()
        .expect("target dir")
        .join("dreamerd");
    assert!(
        daemon.exists(),
        "{} is missing; build dreamer-server",
        daemon.display()
    );
    let daemon = Command::new(daemon)
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

async fn call(c: &client::Client, r: Request) -> Response {
    c.call(r).await.expect("call")
}

/// The pane's tree, read until `ok` holds for it.
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
                wait_ms: Some(500),
            }))
            .await
        {
            *after = t.revision;
            mirror.take(t.seq, &t.tree, &t.patches).expect("a tree");
            if let Some(f) = mirror.frame.as_ref().filter(|f| ok(f)) {
                return f.clone();
            }
        } else {
            // The pane is known once its host registered.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            Instant::now() < deadline,
            "the tree never {what}: {:?}",
            mirror.frame.as_ref().map(|f| f.status.clone())
        );
    }
}

fn input(pane: u32) -> api::AppInput {
    api::AppInput {
        pane,
        ..api::AppInput::default()
    }
}

#[test]
fn input_components_run_their_commands_in_a_pane() {
    let s = setup();
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
                app_id: "dreamer.components".into(),
                grant: Some(api::Grant::default()),
                argv: vec![APP.into(), reference_app().display().to_string()],
                session: t.sessions[0].id,
                ..api::LaunchApp::default()
            }),
        )
        .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;
        let mut mirror = view_tree::Mirror::default();
        let mut after = 0;
        let f = tree_until(&c, pane, &mut mirror, &mut after, "went live", |f| {
            f.status.contains("[live]")
        })
        .await;
        assert_eq!(f.focus.as_deref(), Some("name"));

        // Text from the desktop's text input, then Enter: the input's
        // submit runs `greet`, which reads the value.
        for i in [
            api::AppInput {
                text: Some("Ada".into()),
                ..input(pane)
            },
            api::AppInput {
                key: "Enter".into(),
                ..input(pane)
            },
        ] {
            call(&c, Request::AppsInput(i)).await;
        }
        tree_until(&c, pane, &mut mirror, &mut after, "greeted", |f| {
            f.status.ends_with("| greet: hello, Ada")
        })
        .await;

        // A click on an option picks it and runs `pick` with the event.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                select: Some(1),
                node: Some("skin".into()),
                ..input(pane)
            }),
        )
        .await;
        let f = tree_until(&c, pane, &mut mirror, &mut after, "picked", |f| {
            f.status.ends_with("| pick: skin: light")
        })
        .await;
        assert_eq!(f.focus.as_deref(), Some("skin"));
        assert!(matches!(
            f.root.find("skin").map(|n| &n.kind),
            Some(Kind::Select { selected: 1, .. })
        ));
        // The toggle flipped by a click; the button pressed with Space
        // after Tab moved the focus to it.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                select: Some(0),
                node: Some("notify".into()),
                ..input(pane)
            }),
        )
        .await;
        for key in ["Tab", " "] {
            call(
                &c,
                Request::AppsInput(api::AppInput {
                    key: key.into(),
                    ..input(pane)
                }),
            )
            .await;
        }
        let f = tree_until(&c, pane, &mut mirror, &mut after, "saved", |f| {
            f.status.ends_with("| save: saved light")
        })
        .await;
        assert!(matches!(
            f.root.find("notify").map(|n| &n.kind),
            Some(Kind::Toggle { on: false, .. })
        ));

        // The plain tmux client sees the same components and outcome.
        let target = format!("%{pane}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &target])
                .unwrap_or_default();
            if screen.contains("save: saved light")
                && screen.contains("Name Ada")
                && screen.contains("● light")
                && screen.contains("[ ] Notify when done")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never showed the components:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        call(
            &c,
            Request::AppsInput(api::AppInput {
                key: "Ctrl-c".into(),
                ..input(pane)
            }),
        )
        .await;
    });
}
