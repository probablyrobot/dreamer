//! A web app against a real daemon and an isolated tmux: the example
//! package (`examples/apps/panes-web`) runs in a pane, where its tree is
//! its page, resolved on the app's own origin, and its terminal shows the
//! page's address. A desktop's `apps.call` for the pane runs as the app:
//! `tmux.topology` lists the panes under its grant, a method outside the
//! grant is refused, as are what a page may never call, a pane with no
//! app, and any caller but the user's own clients. `dream app check`
//! checks a web app's manifest and its page.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use view_tree::Kind;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

fn example() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/panes-web")
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

/// A program built beside `dreamer-termapp`.
fn beside(name: &str, package: &str) -> PathBuf {
    let p = PathBuf::from(APP).parent().expect("target dir").join(name);
    assert!(p.exists(), "{} is missing; build {package}", p.display());
    p
}

fn setup() -> Setup {
    let root = PathBuf::from(format!("/tmp/drm-web-app-{}", std::process::id()));
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

fn check(dir: &Path) -> (bool, String) {
    let out = Command::new(beside("dream", "dreamer-cli"))
        .args(["app", "check"])
        .arg(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output()
        .expect("dream app check");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

async fn call(
    c: &client::Client,
    pane: u32,
    method: &str,
    params: &str,
) -> Result<String, api::RpcError> {
    match c
        .call(Request::AppsCall(api::AppCall {
            pane,
            method: method.into(),
            params: params.into(),
        }))
        .await
    {
        Ok(Response::AppsCall(r)) => Ok(r.result),
        Ok(other) => panic!("apps.call: {other:?}"),
        Err(client::ClientError::Rpc(e)) => Err(e),
        Err(e) => panic!("apps.call: {e}"),
    }
}

#[test]
fn dream_app_check_checks_a_web_apps_manifest_and_page() {
    let (ok, text) = check(&example());
    assert!(ok, "{text}");

    let root = std::env::temp_dir().join(format!("drm-web-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    // No page in the package.
    std::fs::copy(example().join("app.toml"), root.join("app.toml")).expect("copy");
    let (ok, text) = check(&root);
    assert!(!ok && text.contains("web/"), "{text}");
    // A remote url, and an allow entry that is no origin.
    std::fs::create_dir_all(root.join("web")).expect("dir");
    std::fs::write(root.join("web/index.html"), "<p>hi</p>").expect("page");
    let manifest = std::fs::read_to_string(example().join("app.toml"))
        .expect("manifest")
        .replace(
            "allow = [\"https://tmux.github.io\"]",
            "url = \"https://example.com\"\nallow = [\"https://tmux.github.io/docs\"]",
        );
    std::fs::write(root.join("app.toml"), manifest).expect("manifest");
    let (ok, text) = check(&root);
    assert!(!ok, "{text}");
    assert!(
        text.contains("web.url: `https://example.com` is not a local address"),
        "{text}"
    );
    assert!(
        text.contains("web.allow: `https://tmux.github.io/docs` is not an origin"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_web_apps_calls_run_as_the_app_under_its_grant() {
    let s = setup();
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
            if let Ok(Response::TmuxTopology(t)) =
                c.call(Request::TmuxTopology(Default::default())).await
                && !t.panes.is_empty()
            {
                break t;
            }
            assert!(Instant::now() < deadline, "no pane");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let shell = t.panes[0].id;
        let Ok(Response::AppsLaunch(launched)) = c
            .call(Request::AppsLaunch(api::LaunchApp {
                app_id: "dreamer.panes-web".into(),
                grant: Some(api::Grant {
                    methods: vec!["tmux.topology".into()],
                    ..api::Grant::default()
                }),
                argv: vec![APP.into(), example().join("app.toml").display().to_string()],
                session: t.sessions[0].id,
                ..api::LaunchApp::default()
            }))
            .await
        else {
            panic!("not launched");
        };
        let pane = launched.pane;

        // The tree a desktop reads: the page, on the app's own origin.
        let mut mirror = view_tree::Mirror::default();
        let (mut after, deadline) = (0, Instant::now() + Duration::from_secs(15));
        let page = loop {
            if let Ok(Response::AppsTree(t)) = c
                .call(Request::AppsTree(api::AppTreeQuery {
                    pane,
                    after,
                    seq: mirror.seq,
                    wait_ms: Some(500),
                }))
                .await
            {
                after = t.revision;
                mirror.take(t.seq, &t.tree, &t.patches).expect("a tree");
                if let Some(Kind::Web { page, refused, .. }) =
                    mirror.frame.as_ref().map(|f| &f.root.kind)
                {
                    assert!(refused.is_empty(), "{refused}");
                    break page.clone();
                }
            }
            assert!(Instant::now() < deadline, "no page in {:?}", mirror.frame);
        };
        assert_eq!(page.url, "dreamer-app://dreamer.panes-web/index.html");
        assert_eq!(page.origin, "dreamer-app://dreamer.panes-web");
        assert_eq!(page.app, "dreamer.panes-web");
        assert!(page.dir.ends_with("panes-web/web"), "{}", page.dir);
        assert_eq!(page.allow, ["https://tmux.github.io"]);

        // Its terminal shows the page's address.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let screen = s
                .tmux
                .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
                .unwrap_or_default();
            if screen.contains("[web: dreamer-app://dreamer.panes-web/index.html; open in Dreamer")
            {
                break;
            }
            assert!(Instant::now() < deadline, "%{pane} shows:\n{screen}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // A call for the app runs under its grant.
        let listed = call(&c, pane, "tmux.topology", "{}")
            .await
            .expect("topology");
        let listed: serde_json::Value = serde_json::from_str(&listed).expect("json");
        let ids: Vec<u64> = listed["panes"]
            .as_array()
            .expect("panes")
            .iter()
            .filter_map(|p| p["id"].as_u64())
            .collect();
        assert!(
            ids.contains(&u64::from(pane)) && ids.contains(&u64::from(shell)),
            "{ids:?}"
        );
        let e = call(&c, pane, "terminal.list", "")
            .await
            .expect_err("outside");
        assert_eq!(e.code, api::code::FORBIDDEN, "{e:?}");
        assert!(
            e.message.contains("not in this connection's grant"),
            "{e:?}"
        );
        // Typing into the user's shell, which the app may not do.
        let typed = format!(
            r#"{{"action": {{"kind": {{"send_keys": {{"pane": {shell}, "keys": ["x"]}}}}}}}}"#
        );
        let e = call(&c, pane, "actions.submit", &typed)
            .await
            .expect_err("act");
        assert_eq!(e.code, api::code::FORBIDDEN, "{e:?}");
        // Reading a page, its own included, needs web.inspect in its grant.
        let e = call(&c, pane, "web.read", &format!(r#"{{"pane": {pane}}}"#))
            .await
            .expect_err("web.read");
        assert_eq!(e.code, api::code::FORBIDDEN, "{e:?}");
        // Never through a page: streams, and what the host itself does.
        for m in [
            "terminal.subscribe",
            "apps.publish_tree",
            "grants.narrow",
            "daemon.shutdown",
        ] {
            let e = call(&c, pane, m, "{}").await.expect_err(m);
            assert_eq!(e.code, api::code::FORBIDDEN, "{m}: {e:?}");
        }
        let e = call(&c, shell, "tmux.topology", "{}")
            .await
            .expect_err("no app");
        assert_eq!(e.code, api::code::NOT_FOUND, "{e:?}");
        let e = call(&c, pane, "tmux.topology", "[")
            .await
            .expect_err("bad json");
        assert_eq!(e.code, api::code::INVALID_PARAMS, "{e:?}");

        // Only the user's own clients pass calls on for an app.
        let narrow = client::Client::connect(&socket, "test")
            .await
            .expect("connect");
        narrow
            .call(Request::GrantsNarrow(api::Grant {
                methods: vec!["apps.call".into(), "tmux.topology".into()],
                all_panes: true,
                ..api::Grant::default()
            }))
            .await
            .expect("narrow");
        let e = call(&narrow, pane, "tmux.topology", "{}")
            .await
            .expect_err("narrow");
        assert_eq!(e.code, api::code::FORBIDDEN, "{e:?}");
        assert!(e.message.contains("only the user's own clients"), "{e:?}");

        // The app stops: its grant ends with it.
        c.call(Request::AppsInput(api::AppInput {
            pane,
            key: "q".into(),
            ..api::AppInput::default()
        }))
        .await
        .expect("quit");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match call(&c, pane, "tmux.topology", "{}").await {
                Err(e) if e.code == api::code::UNAVAILABLE || e.code == api::code::NOT_FOUND => {
                    break;
                }
                other => assert!(Instant::now() < deadline, "still answered: {other:?}"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}
