//! A tree app in a real pane: the host publishes its view tree through
//! the daemon (`apps.publish_tree`), a desktop follows it (`apps.tree`) as
//! the whole tree and then patches, and the plain tmux client sees the
//! same nested layout drawn as a TUI.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

const MANIFEST: &str = r#"
schema_version = 2
id = "test.tree"
version = "0.1.0"
title = "Tree"
capabilities = ["tmux.topology"]
root = "main"

[data.panes]
method = "tmux.topology"
path = "panes"
events = ["tmux.topology"]

[data.windows]
method = "tmux.topology"
path = "windows"

[nodes.main]
type = "split"
sizes = [60, 40]
children = ["panes", "side"]

[nodes.panes]
type = "table"
title = "Panes"
source = "panes"
row_key = "id"
columns = ["id", "current_command"]

[nodes.side]
type = "column"
children = ["windows"]

[nodes.windows]
type = "list"
title = "Windows"
source = "windows"
row_key = "id"
field = "name"
"#;

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
    let root = PathBuf::from(format!("/tmp/drm-atree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tmux = TmuxServer::start(120, 30, "/bin/sh").expect("tmux");
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

/// A desktop's copy of the pane's tree, read until `ok` holds for it.
/// Returns whether any answer brought the whole tree.
async fn tree_until(
    c: &client::Client,
    pane: u32,
    mirror: &mut view_tree::Mirror,
    after: &mut u64,
    what: &str,
    ok: impl Fn(&view_tree::Frame) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut whole = false;
    loop {
        let Response::AppsTree(t) = call(
            c,
            Request::AppsTree(api::AppTreeQuery {
                pane,
                after: *after,
                seq: mirror.seq,
                wait_ms: Some(500),
            }),
        )
        .await
        else {
            panic!("not a tree");
        };
        *after = t.revision;
        whole |= !t.tree.is_empty();
        mirror.take(t.seq, &t.tree, &t.patches).expect("a tree");
        if mirror.frame.as_ref().is_some_and(&ok) {
            return whole;
        }
        assert!(
            Instant::now() < deadline,
            "the tree never {what}: {:?}",
            mirror.frame.as_ref().map(view_tree::Frame::outline)
        );
    }
}

#[test]
fn a_tree_app_reaches_the_desktop_as_patches_and_the_terminal_as_a_tui() {
    let s = setup();
    let manifest = s.root.join("app.toml");
    std::fs::write(&manifest, MANIFEST).expect("manifest");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let socket = s.root.join("rt/dreamerd.sock");
        let c = client::Client::connect(&socket, "test")
            .await
            .expect("connect");
        let Response::TmuxTopology(t) = call(&c, Request::TmuxTopology(Default::default())).await
        else {
            panic!("not a topology");
        };
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "test.tree".into(),
                grant: Some(api::Grant {
                    methods: ["tmux.topology", "events.subscribe", "events.unsubscribe"]
                        .map(String::from)
                        .to_vec(),
                    all_panes: true,
                    ..api::Grant::default()
                }),
                argv: vec![APP.into(), manifest.display().to_string()],
                session: t.sessions[0].id,
                ..api::LaunchApp::default()
            }),
        )
        .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;

        // The desktop's copy: the whole tree first, with live rows.
        let mut mirror = view_tree::Mirror::default();
        let mut after = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        let whole = loop {
            // The pane is known once its host registered.
            match c
                .call(Request::AppsTree(api::AppTreeQuery {
                    pane,
                    after: 0,
                    seq: 0,
                    wait_ms: Some(0),
                }))
                .await
            {
                Ok(_) => {
                    break tree_until(&c, pane, &mut mirror, &mut after, "showed rows", |f| {
                        f.root.find("panes").is_some_and(|n| !n.children.is_empty())
                            && f.status.contains("[live]")
                    })
                    .await;
                }
                Err(_) => {
                    assert!(Instant::now() < deadline, "the host never registered");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        assert!(whole, "a new reader gets the whole tree");
        let frame = mirror.frame.clone().expect("a tree");
        let outline: Vec<String> = frame
            .outline()
            .lines()
            .map(|l| l.split(" (").next().unwrap_or(l).to_owned())
            .collect();
        assert_eq!(
            outline,
            [
                "split main",
                "  table panes",
                "  column side",
                "    list windows"
            ]
        );
        assert_eq!(frame.focus.as_deref(), Some("panes"));

        // The terminal draws the same structure: the table on the left,
        // the list on the right.
        let target = format!("%{pane}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &target])
                .unwrap_or_default();
            let place = |what: &str| {
                screen
                    .lines()
                    .enumerate()
                    .find_map(|(y, l)| l.find(what).map(|b| (l[..b].chars().count(), y)))
            };
            if let (Some((px, py)), Some((wx, wy))) = (place(" Panes "), place(" Windows ")) {
                assert!(px < wx && py == wy, "{screen}");
                assert!(screen.contains(" Tree "), "{screen}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never showed the tree:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Tab, from the desktop, moves the focus in the host; the desktop
        // gets that as a patch, not the whole tree.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "Tab".into(),
                ..api::AppInput::default()
            }),
        )
        .await;
        let whole = tree_until(&c, pane, &mut mirror, &mut after, "moved the focus", |f| {
            f.focus.as_deref() == Some("windows")
        })
        .await;
        assert!(!whole, "a reader that keeps up gets patches");
        // The tree's structure is as it was.
        let now = mirror.frame.clone().expect("a tree");
        assert_eq!(
            now.outline().lines().count(),
            frame.outline().lines().count()
        );

        // A click on the table's last row, from the desktop, names the
        // node: the table takes the focus and that row is selected.
        let last = now.root.find("panes").map_or(0, |n| n.children.len() - 1);
        let last_key = now
            .root
            .find("panes")
            .and_then(|n| n.children.last())
            .map(|r| r.key.clone());
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                select: Some(u32::try_from(last).expect("index")),
                node: Some("panes".into()),
                ..api::AppInput::default()
            }),
        )
        .await;
        tree_until(&c, pane, &mut mirror, &mut after, "took the click", |f| {
            f.focus.as_deref() == Some("panes")
                && f.root.find("panes").is_some_and(|n| {
                    matches!(&n.kind, view_tree::Kind::Table { selected, .. } if *selected == last_key)
                })
        })
        .await;

        // The model is still published beside the tree.
        let Response::AppsView(v) = call(
            &c,
            Request::AppsView(api::AppViewQuery {
                pane,
                after: 0,
                wait_ms: Some(0),
            }),
        )
        .await
        else {
            panic!("not a view");
        };
        assert!(v.view.contains("test.tree"), "{}", v.view);
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "q".into(),
                ..api::AppInput::default()
            }),
        )
        .await;
    });
}
