//! The Cold tier through a real daemon (spec 16.6): a pane nothing follows
//! drops its mirror and its tmux output; one that is followed stays warm;
//! asking for a Cold pane's content attaches it again, with what it wrote
//! meanwhile.

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
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-tier-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
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
        Setup { root, tmux, daemon }
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

async fn mirror(c: &Client, pane: u32) -> Option<api::MirrorStatus> {
    let Ok(Response::TerminalMirrors(l)) = c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    else {
        panic!("mirrors")
    };
    l.mirrors.into_iter().find(|m| m.pane == pane)
}

async fn until_state(
    c: &Client,
    pane: u32,
    state: api::MirrorState,
    what: &str,
) -> api::MirrorStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(m) = mirror(c, pane).await
            && m.state == state as i32
        {
            return m;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: {:?}",
            mirror(c, pane).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn tier(pane: u32, tier: api::Tier) -> Request {
    Request::TerminalSetTier(api::SetTier {
        pane,
        tier: tier as i32,
        endpoint: None,
    })
}

#[tokio::test]
async fn a_cold_pane_holds_no_mirror_and_comes_back_with_what_it_wrote() {
    let s = Setup::new();
    let c = s.client().await;
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    until_state(&c, pane, api::MirrorState::Live, "the pane's mirror").await;

    // Followed by a view: it stays warm.
    let feed = replica::feed::Feed::start(s.root.join("rt/dreamerd.sock"), Some(pane), None)
        .expect("feed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed.take().is_none() {
        assert!(Instant::now() < deadline, "the view never got a frame");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let e = c
        .call(tier(pane, api::Tier::Cold))
        .await
        .expect_err("a followed pane stays warm");
    assert!(e.to_string().contains("subscriber"), "{e}");
    drop(feed);

    // Nothing follows it: Cold, with no mirror and no journal.
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Err(e) = c.call(tier(pane, api::Tier::Cold)).await {
        assert!(Instant::now() < deadline, "never went cold: {e}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let m = until_state(&c, pane, api::MirrorState::Cold, "cold").await;
    assert_eq!(m.tier, api::Tier::Cold as i32);
    assert_eq!((m.journal_bytes, m.sequence), (0, 0), "no mirror is kept");

    // It keeps running in tmux while Cold.
    s.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            &format!("%{pane}"),
            "echo WHILE''-COLD",
            "Enter",
        ])
        .expect("type");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", &format!("%{pane}")])
        .unwrap_or_default()
        .contains("WHILE-COLD")
    {
        assert!(Instant::now() < deadline, "tmux never showed the output");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        mirror(&c, pane).await.map(|m| m.state),
        Some(api::MirrorState::Cold as i32),
        "output alone does not wake a Cold pane"
    );

    // Reading it attaches it again, with what it wrote meanwhile.
    let started = Instant::now();
    let Ok(Response::TerminalRead(r)) = c
        .call(Request::TerminalRead(api::ReadParams {
            pane,
            history: Some(10),
            endpoint: None,
        }))
        .await
    else {
        panic!("read")
    };
    assert!(
        r.lines.iter().any(|l| l.trim_end() == "WHILE-COLD"),
        "{:?}",
        r.lines
    );
    eprintln!("cold to readable in {:?}", started.elapsed());
    let m = until_state(&c, pane, api::MirrorState::Live, "warm again").await;
    assert_eq!(m.tier, api::Tier::Warm as i32);

    // And it follows live output again.
    s.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            &format!("%{pane}"),
            "echo AFTER''-WAKE",
            "Enter",
        ])
        .expect("type");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Ok(Response::TerminalRead(r)) = c
            .call(Request::TerminalRead(api::ReadParams {
                pane,
                history: Some(10),
                endpoint: None,
            }))
            .await
        else {
            panic!("read")
        };
        if r.lines.iter().any(|l| l.trim_end() == "AFTER-WAKE") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live output never reached the mirror"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Many panes created at once all attach: attaches take turns rather than
/// overflowing the control connection's command queue (which failed them
/// for good before).
#[tokio::test]
async fn three_hundred_new_panes_all_become_live() {
    let s = Setup::new();
    let c = s.client().await;
    // In batches: tmux refuses one very long command line.
    for _ in 0..6 {
        let mut args: Vec<&str> = Vec::new();
        for i in 0..50 {
            if i > 0 {
                args.push(";");
            }
            args.extend(["new-window", "-d", "/bin/sh"]);
        }
        s.tmux.run_checked(&args).expect("windows");
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let Ok(Response::TerminalMirrors(l)) = c
            .call(Request::TerminalMirrors(api::TopologyParams::default()))
            .await
        else {
            panic!("mirrors")
        };
        let failed: Vec<_> = l
            .mirrors
            .iter()
            .filter(|m| m.state == api::MirrorState::Failed as i32)
            .map(|m| (m.pane, m.detail.clone()))
            .collect();
        assert!(failed.is_empty(), "attaches failed: {failed:?}");
        let live = l
            .mirrors
            .iter()
            .filter(|m| m.state == api::MirrorState::Live as i32)
            .count();
        if live == 301 {
            break;
        }
        assert!(Instant::now() < deadline, "{live} of 301 live");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
