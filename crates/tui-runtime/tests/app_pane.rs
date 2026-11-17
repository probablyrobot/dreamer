//! An app pane: a package launched as a registered app runs in a real
//! tmux pane, and the daemon marks that pane in the topology. A desktop
//! reads the host's own model through the daemon (`apps.view`), and its
//! keys reach the host (`apps.input`) and act there, while a plain tmux
//! client sees the same app as a TUI. When the host stops, its last view
//! stays, marked not live.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

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
    let root = PathBuf::from(format!("/tmp/drm-apane-{}", std::process::id()));
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

async fn topology_pane(c: &client::Client, pane: u32) -> Option<api::Pane> {
    let Response::TmuxTopology(t) = call(c, Request::TmuxTopology(Default::default())).await else {
        panic!("not a topology");
    };
    t.panes.into_iter().find(|p| p.id == pane)
}

/// The pane's view, once `ok` holds for it.
async fn view_until(
    c: &client::Client,
    pane: u32,
    what: &str,
    ok: impl Fn(&api::AppView, &app_model::App) -> bool,
) -> app_model::App {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut after = 0;
    let mut cache = None;
    loop {
        let Response::AppsView(v) = call(
            c,
            Request::AppsView(api::AppViewQuery {
                pane,
                after,
                wait_ms: Some(500),
            }),
        )
        .await
        else {
            panic!("not a view");
        };
        after = v.revision;
        if !v.view.is_empty() {
            let app = app_model::App::from_published(&v.view, &mut cache).expect("a model");
            if ok(&v, &app) {
                return app;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the view never {what}: {}",
            v.view
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
    }
}

#[test]
fn an_app_pane_is_marked_drawn_from_its_host_and_typed_into() {
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
        let session = t.sessions[0].id;
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "test.panes".into(),
                grant: Some(api::Grant {
                    methods: [
                        "tmux.topology",
                        "actions.submit",
                        "events.subscribe",
                        "events.unsubscribe",
                    ]
                    .map(String::from)
                    .to_vec(),
                    all_panes: true,
                    ..api::Grant::default()
                }),
                argv: vec![APP.into(), manifest.display().to_string()],
                session,
                ..api::LaunchApp::default()
            }),
        )
        .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;
        // Keep the pane after the host ends, to see its last view.
        s.tmux
            .run_checked(&[
                "set-option",
                "-p",
                "-t",
                &format!("%{pane}"),
                "remain-on-exit",
                "on",
            ])
            .expect("remain-on-exit");

        // The topology marks the pane with its app once the host registered.
        let deadline = Instant::now() + Duration::from_secs(15);
        let app = loop {
            if let Some(a) = topology_pane(&c, pane).await.and_then(|p| p.app) {
                break a;
            }
            assert!(
                Instant::now() < deadline,
                "the pane was never marked: {:?}\n{}",
                topology_pane(&c, pane).await,
                s.tmux
                    .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
                    .unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(app.app_id, "test.panes");
        assert!(app.live);
        // Other panes are not.
        assert!(
            topology_pane(&c, t.panes[0].id)
                .await
                .and_then(|p| p.app)
                .is_none()
        );

        // The desktop's view is the host's own model, live with data.
        let shown = view_until(&c, pane, "showed live rows", |v, a| {
            v.live && a.link == app_model::Link::Live && a.rows.len() >= 2
        })
        .await;
        assert_eq!(shown.manifest.title, "Panes");

        // A key from the desktop acts in the host: its command runs there,
        // and the plain tmux client sees the same app say so.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "r".into(),
            }),
        )
        .await;
        view_until(&c, pane, "ran the command", |_, a| {
            a.status.as_deref() == Some("rename: done")
        })
        .await;
        let target = format!("%{pane}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &target])
                .unwrap_or_default();
            if screen.contains("rename: done") && screen.contains(" Panes ") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never showed it:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // The host quits (q, from the desktop): its last view stays, not
        // live, and the topology says so.
        call(
            &c,
            Request::AppsInput(api::AppInput {
                pane,
                key: "q".into(),
            }),
        )
        .await;
        let last = view_until(&c, pane, "went stale", |v, _| !v.live).await;
        assert_eq!(last.status.as_deref(), Some("rename: done"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while topology_pane(&c, pane)
            .await
            .and_then(|p| p.app)
            .is_none_or(|a| a.live)
        {
            assert!(
                Instant::now() < deadline,
                "the topology never marked it stopped"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let refused = c
            .call(Request::AppsInput(api::AppInput {
                pane,
                key: "r".into(),
            }))
            .await;
        assert!(refused.is_err(), "a stopped host takes no keys");

        // The pane closes: the app is forgotten.
        s.tmux
            .run_checked(&["kill-pane", "-t", &target])
            .expect("kill-pane");
        let deadline = Instant::now() + Duration::from_secs(10);
        while c
            .call(Request::AppsView(api::AppViewQuery {
                pane,
                after: 0,
                wait_ms: Some(0),
            }))
            .await
            .is_ok()
        {
            assert!(Instant::now() < deadline, "the closed pane's app stayed");
            // Anything that reads the topology prunes it.
            let _ = topology_pane(&c, pane).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}
