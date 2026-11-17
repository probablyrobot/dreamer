//! A TypeScript app in a real pane, against a real daemon and an isolated
//! tmux: `dream app check` strips its types, the host runs it in QuickJS,
//! it lists the panes and acts on one under its grant, a call outside the
//! grant is refused, and an endless loop is stopped while the pane keeps
//! the last view, marked stale.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

const MANIFEST: &str = r#"
schema_version = 2
id = "test.script"
version = "0.1.0"
title = "Script"
capabilities = ["tmux.topology", "actions.submit"]
all_panes = true

[script]
source = "app.ts"
"#;

/// Lists the panes; Enter on a row types `echo ts-acted` into that pane.
const APP_TS: &str = r#"import { app, ui, dreamer } from "dreamer";

interface Pane {
  id: number;
  current_command: string;
}
type State = { said: string };

app({
  state: { said: "ready" } as State,
  async load(): Promise<{ panes: Pane[] }> {
    const t = await dreamer.tmux.topology();
    return { panes: t.panes as Pane[] };
  },
  view(s: State, d: { panes: Pane[] }) {
    return ui.screen(
      ui.column({
        key: "main",
        children: [
          ui.table({
            key: "panes",
            columns: ["id", "current_command"],
            rows: d.panes,
            onEnter: async (p: Pane): Promise<Partial<State>> => {
              await dreamer.actions.submit({
                action: { kind: { send_input: { pane: p.id, data: "ZWNobyB0cy1hY3RlZA0=" } } },
              });
              return { said: "sent to %" + p.id };
            },
          }),
          ui.button({
            key: "forbidden",
            label: "Shut the daemon down",
            onPress: async () => {
              try {
                await dreamer.call("daemon.shutdown");
                return { said: "it shut down" };
              } catch (e) {
                return { said: "refused: " + (e as Error).message };
              }
            },
          }),
          ui.button({ key: "spin", label: "Spin", onPress: (): void => { for (;;) {} } }),
          ui.text({ key: "said", text: s.said }),
        ],
      }),
      { status: s.said },
    );
  },
});
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

/// A program built beside `dreamer-termapp`.
fn beside(name: &str, package: &str) -> PathBuf {
    let p = PathBuf::from(APP).parent().expect("target dir").join(name);
    assert!(p.exists(), "{} is missing; build {package}", p.display());
    p
}

fn setup() -> Setup {
    let root = PathBuf::from(format!("/tmp/drm-script-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tmux = TmuxServer::start(120, 30, "/bin/sh").expect("tmux");
    let daemon = Command::new(beside("dreamerd", "dreamer-server"))
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

/// `dream app check DIR`, with a private configuration directory.
fn check(dir: &Path) -> std::process::Output {
    Command::new(beside("dream", "dreamer-cli"))
        .args(["app", "check"])
        .arg(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output()
        .expect("dream app check")
}

fn package(dir: &Path, ts: &str) {
    std::fs::create_dir_all(dir).expect("dir");
    std::fs::write(dir.join("app.toml"), MANIFEST).expect("manifest");
    std::fs::write(dir.join("app.ts"), ts).expect("app.ts");
}

async fn call(c: &client::Client, r: Request) -> Response {
    c.call(r).await.expect("call")
}

/// A desktop's copy of the pane's tree, read until `ok` holds for it.
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
        let answer = c
            .call(Request::AppsTree(api::AppTreeQuery {
                pane,
                after: *after,
                seq: mirror.seq,
                wait_ms: Some(500),
            }))
            .await;
        // The pane is known once its host registered.
        if let Ok(Response::AppsTree(t)) = answer {
            *after = t.revision;
            mirror.take(t.seq, &t.tree, &t.patches).expect("a tree");
            if let Some(f) = mirror.frame.as_ref().filter(|f| ok(f)) {
                return f.clone();
            }
        } else {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            Instant::now() < deadline,
            "the tree never {what}: {:?}",
            mirror.frame.as_ref().map(|f| (&f.status, f.outline()))
        );
    }
}

fn said(f: &view_tree::Frame) -> Option<&str> {
    f.root.find("said").and_then(|n| n.kind.text())
}

async fn input(c: &client::Client, i: api::AppInput) {
    call(c, Request::AppsInput(i)).await;
}

#[test]
fn a_typescript_app_works_under_its_grant_and_survives_a_runaway_handler() {
    let s = setup();
    let dir = s.root.join("pkg");
    package(&dir, APP_TS);
    let out = check(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.join("build/app.js").is_file(),
        "check builds the JavaScript"
    );
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
        let shell = t.panes[0].id;
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "test.script".into(),
                grant: Some(api::Grant {
                    methods: ["tmux.topology", "actions.submit"]
                        .map(String::from)
                        .to_vec(),
                    all_panes: true,
                    ..api::Grant::default()
                }),
                argv: vec![APP.into(), dir.join("app.toml").display().to_string()],
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

        // It lists the panes: the shell's and its own.
        let f = tree_until(&c, pane, &mut mirror, &mut after, "listed the panes", |f| {
            f.root.find("panes").is_some_and(|n| n.children.len() >= 2)
        })
        .await;
        let row = f
            .root
            .find("panes")
            .and_then(|n| n.children.iter().position(|r| r.key == shell.to_string()))
            .expect("the shell's row");

        // It acts on the shell's pane: a click selects the row, Enter
        // sends the input under the app's grant.
        input(
            &c,
            api::AppInput {
                pane,
                select: Some(u32::try_from(row).expect("row")),
                node: Some("panes".into()),
                ..api::AppInput::default()
            },
        )
        .await;
        input(
            &c,
            api::AppInput {
                pane,
                key: "Enter".into(),
                ..api::AppInput::default()
            },
        )
        .await;
        let sent = format!("sent to %{shell}");
        tree_until(&c, pane, &mut mirror, &mut after, "acted", |f| {
            said(f) == Some(sent.as_str())
        })
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &format!("%{shell}")])
                .unwrap_or_default();
            if screen.lines().any(|l| l.trim() == "ts-acted") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the input never arrived:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // A call outside its capabilities is refused, and it hears why.
        input(
            &c,
            api::AppInput {
                pane,
                select: Some(0),
                node: Some("forbidden".into()),
                ..api::AppInput::default()
            },
        )
        .await;
        let refused = tree_until(&c, pane, &mut mirror, &mut after, "was refused", |f| {
            said(f).is_some_and(|t| t.starts_with("refused: "))
        })
        .await;
        assert_eq!(
            said(&refused),
            Some("refused: `daemon.shutdown` is not in this app's capabilities")
        );
        assert!(
            matches!(
                call(&c, Request::DaemonStatus(api::Empty {})).await,
                Response::DaemonStatus(_)
            ),
            "the daemon is still up"
        );

        // An endless handler is stopped; the last view stays, marked stale.
        input(
            &c,
            api::AppInput {
                pane,
                select: Some(0),
                node: Some("spin".into()),
                ..api::AppInput::default()
            },
        )
        .await;
        let stale = tree_until(&c, pane, &mut mirror, &mut after, "went stale", |f| {
            f.status.starts_with("stale: ")
        })
        .await;
        assert!(
            stale.status.contains("a handler ran longer than 250 ms"),
            "{}",
            stale.status
        );
        assert_eq!(said(&stale), said(&refused), "the last view stays");
        // The pane's terminal is not held up: it draws the stale line.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
                .unwrap_or_default();
            if screen.contains("stale: ") && screen.contains("refused: ") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never went stale:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // `r` starts it again, with a fresh state.
        input(
            &c,
            api::AppInput {
                pane,
                key: "r".into(),
                ..api::AppInput::default()
            },
        )
        .await;
        tree_until(&c, pane, &mut mirror, &mut after, "restarted", |f| {
            !f.status.starts_with("stale: ") && said(f) == Some("ready")
        })
        .await;
        input(
            &c,
            api::AppInput {
                pane,
                key: "q".into(),
                ..api::AppInput::default()
            },
        )
        .await;
    });
}

#[test]
fn dream_app_check_reports_type_stripping_errors_with_positions() {
    let dir = PathBuf::from(format!("/tmp/drm-script-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    package(
        &dir,
        "import { app, ui } from \"dreamer\";\n\nenum Mode { A, B }\napp({ view: () => ui.text(\"x\") });\n",
    );
    let out = check(&dir);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("app.ts:3:1"), "{err}");
    assert!(!dir.join("build/app.js").exists());

    // A syntax error, with where it is.
    std::fs::write(dir.join("app.ts"), "app({\n  view( => 1,\n});\n").expect("app.ts");
    let out = check(&dir);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("app.ts:2:"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
