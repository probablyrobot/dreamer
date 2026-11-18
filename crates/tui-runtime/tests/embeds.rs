//! `terminal` and `image` nodes in a real pane, against a real daemon and
//! an isolated tmux: a TypeScript app shows two shells' live screens side
//! by side above a table, a third pane outside its grant is refused (in
//! the node and on the status line), an image from its package shows as a
//! placeholder with its size, and one that leaves the package is refused.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use view_tree::{Frame, Kind};

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

const MANIFEST: &str = r#"
schema_version = 2
id = "test.embeds"
version = "0.1.0"
title = "Embeds"
capabilities = ["terminal.read"]

[script]
source = "app.ts"
"#;

/// Two panes side by side and a table, a pane outside the grant, and two
/// images. `{A}`, `{B}`, and `{C}` are pane numbers.
const APP_TS: &str = r#"import { app, ui } from "dreamer";

const panes = [
  { id: {A}, role: "left" },
  { id: {B}, role: "right" },
];

app({
  state: {},
  view() {
    return ui.column({ key: "main", children: [
      ui.split({ key: "top", children: [
        ui.terminal({ key: "a", pane: {A}, title: "left" }),
        ui.terminal({ key: "b", pane: "%{B}", title: "right" }),
      ]}),
      ui.table({ key: "t", columns: ["id", "role"], rows: panes }),
      ui.terminal({ key: "c", pane: {C}, title: "other" }),
      ui.image({ key: "logo", src: "img/logo.png" }),
      ui.image({ key: "out", src: "../outside.png" }),
    ]});
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
    let root = PathBuf::from(format!("/tmp/drm-embeds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tmux = TmuxServer::start(160, 40, "/bin/sh").expect("tmux");
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

/// A PNG's signature and header, 2 by 1 pixels: the host reads no more.
fn png() -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend(2u32.to_be_bytes());
    png.extend(1u32.to_be_bytes());
    png
}

fn package(dir: &Path, ts: &str) {
    std::fs::create_dir_all(dir.join("img")).expect("dir");
    std::fs::write(dir.join("app.toml"), MANIFEST).expect("manifest");
    std::fs::write(dir.join("app.ts"), ts).expect("app.ts");
    std::fs::write(dir.join("img/logo.png"), png()).expect("png");
    std::fs::write(dir.join("../outside.png"), png()).expect("png");
    let out = Command::new(beside("dream", "dreamer-cli"))
        .args(["app", "check"])
        .arg(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output()
        .expect("dream app check");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    ok: impl Fn(&Frame) -> bool,
) -> Frame {
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

/// The pane's screen, captured until it holds every one of `want`.
async fn screen_until(s: &Setup, pane: u32, want: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let screen = s
            .tmux
            .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
            .unwrap_or_default();
        if want.iter().all(|w| screen.contains(w)) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "%{pane} never showed {want:?}:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn refused(f: &Frame, key: &str) -> String {
    match f.root.find(key).map(|n| &n.kind) {
        Some(Kind::Terminal { refused, .. } | Kind::Image { refused, .. }) => refused.clone(),
        other => panic!("`{key}` is {other:?}"),
    }
}

#[test]
fn an_app_shows_two_panes_live_and_is_refused_a_third() {
    let s = setup();
    // Three shells: two the app may read, one it may not.
    for _ in 0..2 {
        s.tmux
            .run_checked(&["split-window", "-h", "-t", "=main:0", "/bin/sh"])
            .or_else(|_| s.tmux.run_checked(&["split-window", "-h", "/bin/sh"]))
            .expect("split");
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let socket = s.root.join("rt/dreamerd.sock");
        let c = client::Client::connect(&socket, "test")
            .await
            .expect("connect");
        let deadline = Instant::now() + Duration::from_secs(10);
        let t = loop {
            let Response::TmuxTopology(t) =
                call(&c, Request::TmuxTopology(Default::default())).await
            else {
                panic!("not a topology");
            };
            if t.panes.len() >= 3 {
                break t;
            }
            assert!(Instant::now() < deadline, "three panes never showed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (a, b, other) = (t.panes[0].id, t.panes[1].id, t.panes[2].id);
        for (p, word) in [(a, "alpha"), (b, "beta")] {
            s.tmux
                .run_checked(&[
                    "send-keys",
                    "-t",
                    &format!("%{p}"),
                    &format!("echo {word}-$((1+1))"),
                    "Enter",
                ])
                .expect("type");
        }

        let dir = s.root.join("pkg");
        package(
            &dir,
            &APP_TS
                .replace("{A}", &a.to_string())
                .replace("{B}", &b.to_string())
                .replace("{C}", &other.to_string()),
        );
        let Response::AppsLaunch(launched) = call(
            &c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "test.embeds".into(),
                grant: Some(api::Grant {
                    methods: vec!["terminal.read".into()],
                    panes: vec![a, b],
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

        // The tree a desktop reads: both panes bound, the third refused,
        // the image resolved in the package, the one outside refused.
        let f = tree_until(
            &c,
            pane,
            &mut mirror,
            &mut after,
            "refused the third pane",
            |f| f.root.find("c").is_some() && !refused(f, "c").is_empty(),
        )
        .await;
        assert_eq!(f.panes(), vec![a, b], "{}", f.outline());
        assert!(refused(&f, "c").contains("outside"), "{}", refused(&f, "c"));
        assert!(
            matches!(
                &f.root.find("logo").expect("logo").kind,
                Kind::Image { width: 2, height: 1, refused, path, .. }
                    if refused.is_empty() && path.ends_with("img/logo.png")
            ),
            "{:?}",
            f.root.find("logo")
        );
        assert!(
            refused(&f, "out").contains("may not leave the app's package"),
            "{}",
            refused(&f, "out")
        );
        // The app hears of the refusal: the daemon's, on its status line.
        let f = tree_until(&c, pane, &mut mirror, &mut after, "said why", |f| {
            f.status.contains(&format!("terminal %{other}: "))
        })
        .await;
        assert!(f.status.contains("outside"), "{}", f.status);
        assert_eq!(
            f.root.find("t").map(|n| n.children.len()),
            Some(2),
            "the table"
        );

        // Its own terminal shows both panes' screens side by side, the
        // table, the refusal, and the image's placeholder.
        let screen = screen_until(
            &s,
            pane,
            &["alpha-2", "beta-2", "[image: logo.png 2x1]", "refused: "],
        )
        .await;
        let row = |w: &str| screen.lines().position(|l| l.contains(w));
        assert_eq!(row("alpha-2"), row("beta-2"), "side by side:\n{screen}");
        let col = |w: &str| screen.lines().find_map(|l| l.find(w));
        assert!(col("alpha-2") < col("beta-2"), "{screen}");
        assert!(screen.contains("right"), "{screen}");

        // Live: new output in a pane shows up.
        s.tmux
            .run_checked(&[
                "send-keys",
                "-t",
                &format!("%{b}"),
                "echo gamma-$((2+1))",
                "Enter",
            ])
            .expect("type");
        screen_until(&s, pane, &["gamma-3"]).await;

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
