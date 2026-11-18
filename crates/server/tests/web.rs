//! Web panes through a real daemon and an isolated tmux: `web.open` makes a
//! placeholder pane (a new window, or a split) that the topology reports
//! with its page, shows the address to plain tmux clients, refuses URLs a
//! web pane may not show; `web.state` keeps the page's title (shown to
//! plain clients too) and scroll position there; and all of it lives in
//! tmux, so a restarted daemon reports it again.

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
        // Short (a socket lives here) and one per test of the process.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/drm-web-{}-{n}", std::process::id()));
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

async fn web_state(c: &Client, req: api::WebState) -> Result<(), api::RpcError> {
    match c.call(Request::WebState(req)).await {
        Ok(Response::WebState(_)) => Ok(()),
        Ok(other) => panic!("web.state: {other:?}"),
        Err(client::ClientError::Rpc(e)) => Err(e),
        Err(e) => panic!("web.state: {e}"),
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

    // The desktop drawing the page keeps its title and scroll position in
    // the placeholder; plain clients see the title, the address, and where
    // to see the page.
    web_state(
        &c,
        api::WebState {
            pane,
            title: "Release notes".into(),
            scroll_x: 0,
            scroll_y: 640,
            ..api::WebState::default()
        },
    )
    .await
    .expect("web.state");
    let t = topology(&c).await;
    let p = t.panes.iter().find(|p| p.id == pane).expect("web pane");
    assert_eq!(
        (p.title.as_str(), p.web_scroll_x, p.web_scroll_y),
        ("Release notes", 0, 640)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = screen(&c, pane).await;
        if text.contains("Release notes")
            && text.contains("web: data:text/html")
            && text.contains("Open in Dreamer")
        {
            break;
        }
        assert!(Instant::now() < deadline, "{text}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Only web panes keep a page's state.
    let e = web_state(
        &c,
        api::WebState {
            pane: first,
            ..api::WebState::default()
        },
    )
    .await
    .expect_err("not a web pane");
    assert_eq!(e.code, api::code::INVALID_PARAMS);

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
    let p = t.panes.iter().find(|p| p.id == pane).expect("web pane");
    assert_eq!((p.web_scroll_x, p.web_scroll_y), (0, 640));
}

async fn navigate(c: &Client, req: api::WebNavigate) -> Result<(), api::RpcError> {
    match c.call(Request::WebNavigate(req)).await {
        Ok(Response::WebNavigate(_)) => Ok(()),
        Ok(other) => panic!("web.navigate: {other:?}"),
        Err(client::ClientError::Rpc(e)) => Err(e),
        Err(e) => panic!("web.navigate: {e}"),
    }
}

/// A connection narrowed to an agent's grant at `level`.
async fn agent(s: &Setup, level: api::ActLevel) -> Client {
    let c = s.client().await;
    c.call(Request::GrantsNarrow(api::Grant {
        methods: api::agent_methods(level as i32),
        all_panes: true,
        level: level as i32,
        ..api::Grant::default()
    }))
    .await
    .expect("grants.narrow");
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_goes_to_other_addresses_and_agents_open_pages_in_their_own_panes() {
    let s = Setup::new();
    let c = s.client().await;
    let t = topology(&c).await;
    let first = t.panes[0].id;
    let pane = open(
        &c,
        api::WebOpen {
            url: PAGE.into(),
            pane: Some(first),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("web.open");
    let url_of = |t: &api::Topology, id: u32| {
        t.panes
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.web.clone())
    };

    // `dream browser --pane`: another address in the same pane, scrolled
    // to its top.
    web_state(
        &c,
        api::WebState {
            pane,
            title: "page".into(),
            scroll_y: 300,
            ..api::WebState::default()
        },
    )
    .await
    .expect("web.state");
    navigate(
        &c,
        api::WebNavigate {
            pane,
            url: "https://example.com/next".into(),
            ..api::WebNavigate::default()
        },
    )
    .await
    .expect("web.navigate");
    let t = topology(&c).await;
    assert_eq!(
        url_of(&t, pane).as_deref(),
        Some("https://example.com/next")
    );
    let p = t.panes.iter().find(|p| p.id == pane).expect("pane");
    assert_eq!((p.web_scroll_x, p.web_scroll_y), (0, 0));

    // The page followed a link: the desktop reports where it is now, and
    // the placeholder keeps it (plain clients see it).
    web_state(
        &c,
        api::WebState {
            pane,
            title: "Followed".into(),
            url: Some("https://example.com/followed".into()),
            ..api::WebState::default()
        },
    )
    .await
    .expect("web.state with a url");
    let t = topology(&c).await;
    assert_eq!(
        url_of(&t, pane).as_deref(),
        Some("https://example.com/followed")
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen(&c, pane)
        .await
        .contains("web: https://example.com/followed")
    {
        assert!(Instant::now() < deadline, "{}", screen(&c, pane).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A place a web pane may not show (Chromium's error page) is not kept.
    web_state(
        &c,
        api::WebState {
            pane,
            title: "Followed".into(),
            url: Some("chrome-error://chromewebdata/".into()),
            ..api::WebState::default()
        },
    )
    .await
    .expect("web.state");
    let t = topology(&c).await;
    assert_eq!(
        url_of(&t, pane).as_deref(),
        Some("https://example.com/followed")
    );

    // Only web panes navigate, and only to pages.
    let e = navigate(
        &c,
        api::WebNavigate {
            pane: first,
            url: "https://example.com/".into(),
            ..api::WebNavigate::default()
        },
    )
    .await
    .expect_err("not a web pane");
    assert_eq!(e.code, api::code::INVALID_PARAMS);
    let e = navigate(
        &c,
        api::WebNavigate {
            pane,
            url: "javascript:alert(1)".into(),
            ..api::WebNavigate::default()
        },
    )
    .await
    .expect_err("a script URL");
    assert_eq!(e.code, api::code::INVALID_PARAMS);

    // An agent that only observes opens nothing.
    let watcher = agent(&s, api::ActLevel::Observe).await;
    let e = open(
        &watcher,
        api::WebOpen {
            url: PAGE.into(),
            pane: Some(first),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect_err("observe");
    assert_eq!(e.code, api::code::FORBIDDEN);

    // An agent at `own` opens a page in a window of its own and owns that
    // pane: it may load another address there, but not in the user's.
    let session = t.sessions[0].id;
    let own = agent(&s, api::ActLevel::Own).await;
    let mine = open(
        &own,
        api::WebOpen {
            url: PAGE.into(),
            session: Some(session),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("an agent opens a page");
    navigate(
        &own,
        api::WebNavigate {
            pane: mine,
            url: "https://example.com/mine".into(),
            ..api::WebNavigate::default()
        },
    )
    .await
    .expect("in its own pane");
    let split = open(
        &own,
        api::WebOpen {
            url: PAGE.into(),
            pane: Some(first),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect_err("split the user's pane");
    assert_eq!(split.code, api::code::FORBIDDEN);
    let theirs = navigate(
        &own,
        api::WebNavigate {
            pane,
            url: "https://example.com/theirs".into(),
            ..api::WebNavigate::default()
        },
    )
    .await
    .expect_err("navigate the user's page");
    assert_eq!(theirs.code, api::code::FORBIDDEN);
    // Reporting a page's state stays the desktop's.
    let e = web_state(
        &own,
        api::WebState {
            pane: mine,
            ..api::WebState::default()
        },
    )
    .await
    .expect_err("web.state");
    assert_eq!(e.code, api::code::FORBIDDEN);
}

/// A connection narrowed to an agent's grant at `level`, with
/// `web.inspect`.
async fn inspector(s: &Setup, level: api::ActLevel) -> Client {
    let c = s.client().await;
    let mut methods = api::agent_methods(level as i32);
    methods.push(api::INSPECT_WEB.into());
    c.call(Request::GrantsNarrow(api::Grant {
        methods,
        all_panes: true,
        level: level as i32,
        ..api::Grant::default()
    }))
    .await
    .expect("grants.narrow");
    c
}

/// The error a call ends with.
async fn refused(c: &Client, r: Request) -> api::RpcError {
    match c.call(r).await {
        Err(client::ClientError::Rpc(e)) => e,
        other => panic!("not refused: {other:?}"),
    }
}

fn read(pane: u32) -> Request {
    Request::WebRead(api::WebRead {
        pane,
        tree: true,
        wait_ms: Some(5_000),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_are_read_and_driven_only_under_web_inspect_and_in_a_desktop() {
    let s = Setup::new();
    let c = s.client().await;
    let t = topology(&c).await;
    let (first, session) = (t.panes[0].id, t.sessions[0].id);
    let user = open(
        &c,
        api::WebOpen {
            url: PAGE.into(),
            pane: Some(first),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("web.open");

    // An agent's grant does not hold web.inspect unless it is given.
    let plain = agent(&s, api::ActLevel::Elsewhere).await;
    let e = refused(&plain, read(user)).await;
    assert_eq!(e.code, api::code::FORBIDDEN);
    assert!(e.message.contains("web.read"), "{}", e.message);

    // With it, reading passes the grant at every level; with no desktop to
    // show the page, the call says so at once.
    let watcher = inspector(&s, api::ActLevel::Observe).await;
    let e = refused(&watcher, read(user)).await;
    assert_eq!(e.code, api::code::UNAVAILABLE);
    assert!(e.message.contains("no Dreamer window"), "{}", e.message);
    // Clicking and typing act on the pane: never at observe, and at own
    // only in a pane the agent opened.
    let click = |pane| {
        Request::WebClick(api::WebClick {
            pane,
            selector: Some("button".into()),
            ..api::WebClick::default()
        })
    };
    assert_eq!(
        refused(&watcher, click(user)).await.code,
        api::code::FORBIDDEN
    );
    let own = inspector(&s, api::ActLevel::Own).await;
    assert_eq!(refused(&own, click(user)).await.code, api::code::FORBIDDEN);
    let mine = open(
        &own,
        api::WebOpen {
            url: PAGE.into(),
            session: Some(session),
            ..api::WebOpen::default()
        },
    )
    .await
    .expect("web.open as an agent");
    let typed = Request::WebType(api::WebType {
        pane: mine,
        text: "hi".into(),
        submit: true,
        wait_ms: Some(5_000),
        ..api::WebType::default()
    });
    assert_eq!(
        refused(&own, typed.clone()).await.code,
        api::code::UNAVAILABLE
    );

    // A desktop that does not show the pane answers why, and the caller
    // hears it.
    let desktop = s.client().await;
    let answered = tokio::spawn(async move {
        let Ok(Response::AppsCaptureWait(r)) = desktop
            .call(Request::AppsCaptureWait(api::AppCaptureWait {
                wait_ms: Some(10_000),
            }))
            .await
        else {
            panic!("apps.capture_wait");
        };
        let asked = (r.pane, r.method.clone());
        desktop
            .call(Request::AppsCaptureDone(api::AppCaptureDone {
                id: r.id,
                error: Some(format!("pane %{} is not shown in a Dreamer window", r.pane)),
                ..api::AppCaptureDone::default()
            }))
            .await
            .expect("apps.capture_done");
        asked
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let e = loop {
        let e = refused(&own, typed.clone()).await;
        if e.code != api::code::UNAVAILABLE || Instant::now() > deadline {
            break e;
        }
        // The desktop's wait has not begun yet.
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(e.code, api::code::PRECONDITION, "{}", e.message);
    assert_eq!(
        e.message,
        format!("pane %{mine} is not shown in a Dreamer window")
    );
    assert_eq!(
        answered.await.expect("desktop"),
        (mine, "web.type".to_owned())
    );

    // A screenshot needs an absolute path.
    let e = refused(
        &watcher,
        Request::WebScreenshot(api::WebScreenshot {
            pane: user,
            path: "shot.png".into(),
            wait_ms: None,
        }),
    )
    .await;
    assert_eq!(e.code, api::code::INVALID_PARAMS);
}
