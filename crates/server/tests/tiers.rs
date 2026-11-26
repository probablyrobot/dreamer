//! The Cold tier through a real daemon: a pane nothing follows
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

/// Settings that turn the tier policy off, for tests that set tiers
/// themselves.
const NO_POLICY: &str = "tiers:\n  cold_after: never\n  warm_limit: none\n  warm_memory: none\n";

impl Setup {
    fn new() -> Setup {
        Setup::with(NO_POLICY)
    }

    /// A daemon whose `dreamer.yaml` is `settings`.
    fn with(settings: &str) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-tier-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::write(root.join("config/dreamer.yaml"), settings).expect("settings");
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

    /// Replace `dreamer.yaml`; the daemon applies it live.
    fn settings(&self, text: &str) {
        let tmp = self.root.join("config/dreamer.yaml.new");
        std::fs::write(&tmp, text).expect("settings");
        std::fs::rename(tmp, self.root.join("config/dreamer.yaml")).expect("settings");
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

async fn states(c: &Client, panes: &[u32]) -> Vec<i32> {
    let Ok(Response::TerminalMirrors(l)) = c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    else {
        panic!("mirrors")
    };
    panes
        .iter()
        .map(|p| {
            l.mirrors
                .iter()
                .find(|m| m.pane == *p)
                .map_or(-1, |m| m.state)
        })
        .collect()
}

/// Wait until the panes' states are `want`, then check they stay so.
async fn until_states(c: &Client, panes: &[u32], want: &[api::MirrorState], what: &str) {
    let want: Vec<i32> = want.iter().map(|s| *s as i32).collect();
    let deadline = Instant::now() + Duration::from_secs(30);
    while states(c, panes).await != want {
        assert!(
            Instant::now() < deadline,
            "{what}: {:?}, wanted {want:?}",
            states(c, panes).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(states(c, panes).await, want, "{what}, two seconds on");
}

/// The tier policy (`tiers:` in `dreamer.yaml`, applied live): over the
/// Warm limit the quietest unfollowed panes go Cold first; a pane quiet for
/// longer than `cold_after` goes Cold; a followed pane never does.
#[tokio::test]
async fn the_tier_policy_demotes_quiet_unfollowed_panes_and_keeps_followed_ones() {
    use api::MirrorState::{Cold, Live};
    let s = Setup::new();
    let c = s.client().await;
    s.tmux
        .run_checked(&[
            "new-window",
            "-d",
            "/bin/sh",
            ";",
            "new-window",
            "-d",
            "/bin/sh",
            ";",
            "new-window",
            "-d",
            "/bin/sh",
            ";",
            "new-window",
            "-d",
            "/bin/sh",
        ])
        .expect("windows");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let mut panes: Vec<u32> = t.panes.iter().map(|p| p.id).collect();
    panes.sort_unstable();
    assert_eq!(panes.len(), 5);
    until_states(&c, &panes, &[Live; 5], "all live").await;

    // The first is followed by a view.
    let feed = replica::feed::Feed::start(s.root.join("rt/dreamerd.sock"), Some(panes[0]), None)
        .expect("feed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed.take().is_none() {
        assert!(Instant::now() < deadline, "the view never got a frame");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The last two wrote something just now: the second and third are the
    // quietest.
    for p in &panes[3..] {
        s.tmux
            .run_checked(&["send-keys", "-t", &format!("%{p}"), "echo busy", "Enter"])
            .expect("type");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    s.settings("tiers:\n  cold_after: never\n  warm_limit: 2\n");
    until_states(
        &c,
        &panes,
        &[Live, Cold, Cold, Live, Live],
        "a Warm limit of 2 demotes the two quietest unfollowed panes",
    )
    .await;

    s.settings("tiers:\n  cold_after: 2s\n  warm_limit: none\n");
    until_states(
        &c,
        &panes,
        &[Live, Cold, Cold, Cold, Cold],
        "quiet panes go Cold; the followed one stays",
    )
    .await;
    drop(feed);
}

/// The Warm memory ceiling (`tiers.warm_memory`): while the host's mirrors
/// together hold more, the quietest unfollowed panes go Cold, and no more
/// than it takes; a followed pane stays Warm even when it alone is over.
#[tokio::test]
async fn the_warm_memory_ceiling_demotes_quiet_unfollowed_panes_and_never_followed_ones() {
    use api::MirrorState::{Cold, Live};
    let s = Setup::new();
    let c = s.client().await;
    s.tmux
        .run_checked(&[
            "new-window",
            "-d",
            "/bin/sh",
            ";",
            "new-window",
            "-d",
            "/bin/sh",
            ";",
            "new-window",
            "-d",
            "/bin/sh",
        ])
        .expect("windows");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let mut panes: Vec<u32> = t.panes.iter().map(|p| p.id).collect();
    panes.sort_unstable();
    assert_eq!(panes.len(), 4);
    until_states(&c, &panes, &[Live; 4], "all live").await;

    // The first is followed by a view; the last two wrote just now, so the
    // second is the quietest.
    let feed = replica::feed::Feed::start(s.root.join("rt/dreamerd.sock"), Some(panes[0]), None)
        .expect("feed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed.take().is_none() {
        assert!(Instant::now() < deadline, "the view never got a frame");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for p in &panes[2..] {
        s.tmux
            .run_checked(&["send-keys", "-t", &format!("%{p}"), "echo busy", "Enter"])
            .expect("type");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Each pane holds a screen and no history: room for three and a half.
    let m = mirror(&c, panes[0]).await.expect("mirror");
    let screen = m.rows as usize * mirror::worker::line_bytes(m.cols as u16);
    let ceiling = screen * 7 / 2 / 1024;
    s.settings(&format!(
        "tiers:\n  cold_after: never\n  warm_limit: none\n  warm_memory: {ceiling}KiB\n"
    ));
    until_states(
        &c,
        &panes,
        &[Live, Cold, Live, Live],
        "one pane over the ceiling: the quietest unfollowed one goes Cold",
    )
    .await;

    s.settings("tiers:\n  cold_after: never\n  warm_limit: none\n  warm_memory: 1KiB\n");
    until_states(
        &c,
        &panes,
        &[Live, Cold, Cold, Cold],
        "every unfollowed pane goes Cold; the followed one stays",
    )
    .await;
    drop(feed);
}

/// A program's own default colors (OSC 10/11) set while its pane is Cold
/// survive the attach that wakes it: tmux keeps them and the attach
/// restores them, unlike a window style, which stays tmux's.
#[tokio::test]
async fn colors_a_program_set_while_cold_survive_the_attach() {
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Err(e) = c.call(tier(pane, api::Tier::Cold)).await {
        assert!(Instant::now() < deadline, "never went cold: {e}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    until_state(&c, pane, api::MirrorState::Cold, "cold").await;
    s.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            &format!("%{pane}"),
            "printf '\\033]11;#402020\\007'",
            "Enter",
        ])
        .expect("type");
    let deadline = Instant::now() + Duration::from_secs(10);
    while s
        .tmux
        .run_checked(&[
            "display-message",
            "-p",
            "-t",
            &format!("%{pane}"),
            "#{pane_bg}",
        ])
        .unwrap_or_default()
        .trim()
        != "#402020"
    {
        assert!(Instant::now() < deadline, "tmux never took the color");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A view wakes it.
    let feed = replica::feed::Feed::start(s.root.join("rt/dreamerd.sock"), Some(pane), None)
        .expect("feed");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(f) = feed.take() {
            let bg = f.colors.background;
            if (bg.r, bg.g, bg.b) == (0x40, 0x20, 0x20) {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the view never showed the program's own background"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a text or a meta tap delivered: the `line-N` lines, or the
/// `step-N` titles, in the order they arrived.
fn steps(level: api::TapLevel, d: &api::TapData, seen: &mut Vec<String>) {
    let new: Vec<String> = if level == api::TapLevel::Text {
        d.lines
            .iter()
            .filter(|l| l.starts_with("line-"))
            .cloned()
            .collect()
    } else {
        d.meta
            .as_ref()
            .and_then(|m| m.title.clone())
            .filter(|t| t.starts_with("step-"))
            .into_iter()
            .collect()
    };
    for s in new {
        if seen.last() != Some(&s) {
            seen.push(s);
        }
    }
}

/// A pane watched only by a text or a meta tap is followed: these taps
/// look at the mirror on a timer instead of subscribing to it, and the
/// pane still never goes Cold, on its own or when asked. Its output comes
/// with pauses longer than `tiers.cold_after`, and the tap gets every step
/// in order with no gap.
async fn a_looking_tap_keeps_its_pane_warm(level: api::TapLevel) {
    use prost::Message;
    let s = Setup::with("tiers:\n  cold_after: 1s\n  warm_limit: none\n  warm_memory: none\n");
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    until_state(&c, pane, api::MirrorState::Live, "the pane's mirror").await;
    // Looking once a second: a pane put to sleep between looks stays Cold
    // long enough to be seen.
    let Ok(Response::TapsSubscribe(sub)) = c
        .call(Request::TapsSubscribe(api::TapSubscribe {
            level: level as i32,
            panes: vec![pane],
            interval_ms: Some(1000),
            ..api::TapSubscribe::default()
        }))
        .await
    else {
        panic!("tap")
    };
    s.tmux
        .run_checked(&[
            "send-keys",
            "-t",
            &format!("%{pane}"),
            "for i in 1 2 3 4 5 6; do printf '\\033]2;step-%s\\007line-%s\\n' $i $i; sleep 1.5; done",
            "Enter",
        ])
        .expect("type");

    let want: Vec<String> = (1..=6)
        .map(|i| {
            if level == api::TapLevel::Text {
                format!("line-{i}")
            } else {
                format!("step-{i}")
            }
        })
        .collect();
    let mut seen = Vec::new();
    let mut cold = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while seen.len() < want.len() {
        assert!(Instant::now() < deadline, "the tap stopped at {seen:?}");
        if let Ok(Some(client::Pushed::Event { topic, payload, .. })) =
            tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            && topic == "taps.data"
        {
            let d = api::TapData::decode(payload.as_slice()).expect("tap data");
            if d.tap == sub.id {
                assert_eq!(d.gap, None, "a gap after {seen:?}");
                assert!(!d.ended, "the tap ended after {seen:?}");
                steps(level, &d, &mut seen);
            }
        }
        if mirror(&c, pane).await.map(|m| m.state) == Some(api::MirrorState::Cold as i32) {
            cold += 1;
        }
    }
    assert_eq!(seen, want, "every step, in order");
    assert_eq!(cold, 0, "the tapped pane was seen Cold {cold} times");

    let e = c
        .call(tier(pane, api::Tier::Cold))
        .await
        .expect_err("a tapped pane stays warm when asked to go Cold");
    assert!(e.to_string().contains("subscriber"), "{e}");
}

#[tokio::test]
async fn a_text_tap_keeps_its_pane_out_of_the_cold_tier() {
    a_looking_tap_keeps_its_pane_warm(api::TapLevel::Text).await;
}

#[tokio::test]
async fn a_meta_tap_keeps_its_pane_out_of_the_cold_tier() {
    a_looking_tap_keeps_its_pane_warm(api::TapLevel::Meta).await;
}
