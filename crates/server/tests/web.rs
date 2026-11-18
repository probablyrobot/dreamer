//! Web panes through a real daemon and an isolated tmux: `web.open` makes a
//! placeholder pane (a new window, or a split) that the topology reports
//! with its page, shows the address to plain tmux clients, refuses URLs a
//! web pane may not show, and the mark lives in tmux, so a restarted daemon
//! reports it again.

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
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-web-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let daemon = Setup::daemon(&root, &tmux);
        Setup { root, tmux, daemon }
    }

    fn daemon(root: &std::path::Path, tmux: &TmuxServer) -> Child {
        Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_DATA_DIR", root.join("data"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd")
    }

    fn stop_daemon(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while platform::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
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
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn topology(c: &Client) -> api::Topology {
    match c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    {
        Ok(Response::TmuxTopology(t)) => t,
        other => panic!("topology: {other:?}"),
    }
}

async fn open(c: &Client, req: api::WebOpen) -> Result<u32, api::RpcError> {
    match c.call(Request::WebOpen(req)).await {
        Ok(Response::WebOpen(o)) => Ok(o.pane),
        Ok(other) => panic!("web.open: {other:?}"),
        Err(client::ClientError::Rpc(e)) => Err(e),
        Err(e) => panic!("web.open: {e}"),
    }
}

async fn screen(c: &Client, pane: u32) -> String {
    match c
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(10),
            endpoint: None,
        }))
        .await
    {
        Ok(Response::TerminalRead(t)) => t.lines.join("\n"),
        other => panic!("read: {other:?}"),
    }
}

const PAGE: &str = "data:text/html,<p>page</p>";

#[tokio::test(flavor = "multi_thread")]
async fn a_web_pane_is_a_marked_placeholder_that_survives_a_daemon_restart() {
    let mut s = Setup::new();
    let c = s.client().await;
    let t = topology(&c).await;
    let session = t.sessions[0].id;
    let first = t.panes[0].id;

    // A new window: the pane carries its page, and is what the session shows.
    let pane = open(
        &c,
        api::WebOpen {
            url: PAGE.into(),
            session: Some(session),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("web.open");
    let t = topology(&c).await;
    let p = t.panes.iter().find(|p| p.id == pane).expect("web pane");
    assert_eq!(p.web.as_deref(), Some(PAGE));
    let s0 = t
        .sessions
        .iter()
        .find(|x| x.id == session)
        .expect("session");
    assert_eq!(s0.active_window, Some(p.window));
    assert!(p.active);
    assert!(
        t.panes
            .iter()
            .filter(|p| p.id != pane)
            .all(|p| p.web.is_none())
    );

    // Plain tmux clients see the address.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen(&c, pane).await.contains("web: data:text/html") {
        assert!(Instant::now() < deadline, "{}", screen(&c, pane).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A split beside an existing pane.
    let split = open(
        &c,
        api::WebOpen {
            url: "https://example.com/".into(),
            pane: Some(first),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("split");
    let t = topology(&c).await;
    let p = t.panes.iter().find(|p| p.id == split).expect("split pane");
    assert_eq!(p.web.as_deref(), Some("https://example.com/"));
    assert_eq!(
        p.window,
        t.panes
            .iter()
            .find(|p| p.id == first)
            .expect("first")
            .window
    );

    // Pages only: no script URLs, no browser-internal pages.
    for url in ["javascript:alert(1)", "chrome://settings", ""] {
        let e = open(
            &c,
            api::WebOpen {
                url: url.into(),
                session: Some(session),
                ..api::WebOpen::default()
            },
        )
        .await
        .expect_err(url);
        assert_eq!(e.code, api::code::INVALID_PARAMS, "{url}");
    }
    // A mark set by hand in tmux to something that is not a page is not
    // reported as one.
    s.tmux
        .run_checked(&[
            "set-option",
            "-p",
            "-t",
            &format!("%{first}"),
            "@dreamer_surface",
            "web:javascript:alert(1)",
        ])
        .expect("set-option");

    // The marks live in tmux: a new daemon reports them again.
    drop(c);
    s.stop_daemon();
    s.daemon = Setup::daemon(&s.root, &s.tmux);
    let c = s.client().await;
    let t = topology(&c).await;
    let web: Vec<(u32, Option<&str>)> = t
        .panes
        .iter()
        .filter(|p| p.web.is_some())
        .map(|p| (p.id, p.web.as_deref()))
        .collect();
    assert_eq!(
        web,
        vec![(pane, Some(PAGE)), (split, Some("https://example.com/"))]
    );
}
