//! Daemon mirrors against real tmux (baseline 3.8) on an isolated server.

use std::time::Duration;

use domain::{ConnectionEpoch, PaneGeneration, ServerKey, TmuxPaneId};
use mirror::{MirrorConfig, MirrorHost, MirrorState, Retention, Tier};
use tmux_backend::server::tmux_program;
use tmux_backend::{Connection, ConnectionConfig, InventoryTracker, Socket, TmuxServer};

struct Harness {
    server: TmuxServer,
    host: MirrorHost,
    changes: tokio::sync::watch::Receiver<u64>,
    tracker: InventoryTracker,
}

fn config(server: &TmuxServer) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(
        tmux_program(),
        Socket::Name(server.socket().to_owned()),
        "main",
    );
    c.config_file = Some("/dev/null".into());
    c.env = Some(vec![
        (
            "PATH".into(),
            "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin".into(),
        ),
        ("HOME".into(), std::env::temp_dir().display().to_string()),
        ("TERM".into(), "xterm-256color".into()),
        ("PS1".into(), "$ ".into()),
    ]);
    c
}

async fn start(panes: usize) -> Harness {
    start_with(
        panes,
        MirrorConfig {
            workers: 2,
            worker_queue: 256,
            ..MirrorConfig::default()
        },
    )
    .await
}

async fn start_with(panes: usize, mirror_config: MirrorConfig) -> Harness {
    let server = TmuxServer::start(160, 50, "/bin/sh").expect("tmux");
    server
        .run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
        .expect("shell");
    for _ in 1..panes {
        server
            .run_checked(&["split-window", "-t", "main", "/bin/sh"])
            .expect("split");
        server
            .run_checked(&["select-layout", "-t", "main", "tiled"])
            .expect("layout");
    }
    let (conn, events) = Connection::spawn(config(&server)).await.expect("connect");
    let (host, changes) =
        MirrorHost::start(conn.clone(), events, ConnectionEpoch(1), mirror_config)
            .await
            .expect("mirrors");
    let (tracker, _) = InventoryTracker::start(conn, ServerKey("test".into()), "test")
        .await
        .expect("inventory");
    let mut h = Harness {
        server,
        host,
        changes,
        tracker,
    };
    h.sync().await;
    let all = h.panes();
    assert!(
        h.host.wait_live(&all, Duration::from_secs(10)).await,
        "{:#?}",
        h.host.status().await
    );
    h
}

impl Harness {
    fn panes(&self) -> Vec<TmuxPaneId> {
        self.tracker.inventory().panes.keys().copied().collect()
    }

    async fn sync(&mut self) {
        self.tracker.refresh().await.expect("refresh");
        let panes: Vec<(TmuxPaneId, PaneGeneration)> = self
            .panes()
            .into_iter()
            .map(|p| (p, self.tracker.generation(p).expect("generation")))
            .collect();
        self.host
            .sync(self.tracker.inventory().server.clone(), panes)
            .await;
    }

    fn tmux_lines(&self, pane: TmuxPaneId) -> Vec<String> {
        let text = self
            .server
            .run_checked(&[
                "capture-pane",
                "-p",
                "-J",
                "-S",
                "-",
                "-E",
                "-",
                "-t",
                &format!("%{}", pane.0),
            ])
            .expect("capture");
        normalize(text.lines().map(str::to_owned).collect())
    }

    async fn mirror_lines(&self, pane: TmuxPaneId) -> Vec<String> {
        let r = self
            .host
            .read(pane, 100_000)
            .await
            .expect("mirrored")
            .expect("read");
        normalize(r.lines)
    }
}

fn normalize(mut lines: Vec<String>) -> Vec<String> {
    for l in &mut lines {
        let t = l.trim_end().len();
        l.truncate(t);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heavy_concurrent_output_reaches_every_mirror_in_order() {
    let h = start(4).await;
    let panes = h.panes();
    const N: u32 = 30_000;
    // All four panes produce output at the same time.
    for p in &panes {
        let t = format!("%{}", p.0);
        h.server
            .run_checked(&[
                "send-keys",
                "-t",
                &t,
                "-l",
                &format!("seq 1 {N}; echo DONE-{}", p.0),
            ])
            .expect("type");
        h.server
            .run_checked(&["send-keys", "-t", &t, "Enter"])
            .expect("enter");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    for p in &panes {
        let done = format!("DONE-{}", p.0);
        loop {
            let tmux = h.tmux_lines(*p);
            let ours = h.mirror_lines(*p).await;
            let finished = tmux.iter().any(|l| l == &done) && ours.iter().any(|l| l == &done);
            if finished && tmux.len() == ours.len() && tmux == ours {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let first_diff = tmux.iter().zip(&ours).position(|(a, b)| a != b);
                panic!(
                    "pane %{}: mirror ({} lines) never matched tmux ({} lines); first difference at {first_diff:?}: {:?} vs {:?}",
                    p.0,
                    ours.len(),
                    tmux.len(),
                    first_diff.and_then(|i| ours.get(i)),
                    first_diff.and_then(|i| tmux.get(i))
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The retained numbers are contiguous and end at N: nothing was
        // reordered, dropped, or duplicated.
        let ours = h.mirror_lines(*p).await;
        let numbers: Vec<u32> = ours.iter().filter_map(|l| l.parse().ok()).collect();
        assert!(
            numbers.len() > 1000,
            "pane %{}: only {} numbers retained",
            p.0,
            numbers.len()
        );
        assert_eq!(numbers.last(), Some(&N));
        assert!(
            numbers.windows(2).all(|w| w[1] == w[0] + 1),
            "pane %{}: numbers out of order",
            p.0
        );
    }
    for s in h.host.status().await {
        let live = s.live.expect("live");
        assert_eq!(live.epoch, ConnectionEpoch(1));
        assert_eq!(live.pane.pane, s.pane);
        assert!(
            live.ingested > u64::from(N) * 5,
            "pane %{}: {} bytes",
            s.pane.0,
            live.ingested
        );
        assert!(live.error.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_respawned_pane_gets_a_new_mirror_and_old_results_are_gone() {
    let mut h = start(2).await;
    let pane = h.panes()[1];
    let t = format!("%{}", pane.0);
    h.server
        .run_checked(&["send-keys", "-t", &t, "-l", "echo before-respawn"])
        .expect("type");
    h.server
        .run_checked(&["send-keys", "-t", &t, "Enter"])
        .expect("enter");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !h
        .mirror_lines(pane)
        .await
        .iter()
        .any(|l| l == "before-respawn")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "output never mirrored"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = h
        .host
        .read(pane, 0)
        .await
        .expect("m")
        .expect("r")
        .generation;

    let seen = *h.changes.borrow_and_update();
    h.server
        .run_checked(&["respawn-pane", "-k", "-t", &t, "/bin/sh"])
        .expect("respawn");
    // tmux reports the respawn as a change (a structural notification or the
    // pane-pid subscription); nothing here polls.
    tokio::time::timeout(Duration::from_secs(5), h.changes.changed())
        .await
        .expect("the respawn was reported")
        .expect("watch");
    assert!(*h.changes.borrow() > seen);
    h.sync().await;
    assert!(h.host.wait_live(&[pane], Duration::from_secs(10)).await);
    let after = h.host.read(pane, 1000).await.expect("m").expect("r");
    assert!(
        after.generation > before,
        "{:?} -> {:?}",
        before,
        after.generation
    );
    assert!(!after.lines.iter().any(|l| l.contains("before-respawn")));
    let status = h.host.status().await;
    assert_eq!(status.iter().filter(|s| s.pane == pane).count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resizes_and_closed_panes_follow_tmux() {
    let mut h = start(2).await;
    let [a, b] = [h.panes()[0], h.panes()[1]];
    h.server
        .run_checked(&["resize-pane", "-t", &format!("%{}", a.0), "-y", "12"])
        .expect("resize");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = h.host.read(a, 0).await.expect("m").expect("r");
        let tmux_rows: u16 = h
            .server
            .format(&format!("%{}", a.0), "#{pane_height}")
            .expect("height")
            .trim()
            .parse()
            .expect("number");
        if r.rows == tmux_rows && tmux_rows == 12 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mirror {} rows, tmux {tmux_rows}",
            r.rows
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.server
        .run_checked(&["kill-pane", "-t", &format!("%{}", b.0)])
        .expect("kill");
    h.sync().await;
    let status = h.host.status().await;
    assert!(status.iter().all(|s| s.pane != b), "{status:#?}");
    assert!(h.host.read(b, 0).await.is_none());
    assert!(
        status
            .iter()
            .any(|s| s.pane == a && s.state == MirrorState::Live)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_retention_holds_per_pane_and_per_host_under_load() {
    let retention = Retention {
        per_pane_bytes: 64 << 10,
        host_bytes: 160 << 10,
        max_age: Duration::from_secs(600),
    };
    let h = start_with(
        3,
        MirrorConfig {
            workers: 2,
            worker_queue: 256,
            retention,
        },
    )
    .await;
    let panes = h.panes();
    for p in &panes {
        let t = format!("%{}", p.0);
        h.server
            .run_checked(&[
                "send-keys",
                "-t",
                &t,
                "-l",
                &format!("seq 1 40000; echo DONE-{}", p.0),
            ])
            .expect("type");
        h.server
            .run_checked(&["send-keys", "-t", &t, "Enter"])
            .expect("enter");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    for p in &panes {
        let done = format!("DONE-{}", p.0);
        while !h.mirror_lines(*p).await.iter().any(|l| l == &done) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "pane %{} never finished",
                p.0
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Retention never changes what the engine holds.
        assert_eq!(h.mirror_lines(*p).await, h.tmux_lines(*p));
    }
    let status = h.host.status().await;
    let mut total = 0;
    for s in &status {
        let l = s.live.as_ref().expect("live");
        assert!(
            l.journal_bytes <= retention.per_pane_bytes,
            "pane %{}: {} bytes",
            s.pane.0,
            l.journal_bytes
        );
        assert!(
            l.journal_first > 1,
            "pane %{}: nothing was trimmed",
            s.pane.0
        );
        // Contiguous: the retained records are exactly first..=sequence.
        assert_eq!(
            l.journal_first + l.journal_records as u64,
            l.sequence + 1,
            "pane %{}",
            s.pane.0
        );
        total += l.journal_bytes;
    }
    assert!(total <= retention.host_bytes, "host holds {total} bytes");
    assert_eq!(h.host.host_journal_bytes(), total);

    // Tiers: Warm by default, Hot on request, visible in status.
    assert!(
        status
            .iter()
            .all(|s| s.live.as_ref().expect("live").tier == Tier::Warm)
    );
    assert!(h.host.set_tier(panes[0], Tier::Hot).await);
    assert!(!h.host.set_tier(TmuxPaneId(9999), Tier::Hot).await);
    let after = h.host.status().await;
    let tier = |p: TmuxPaneId| {
        after
            .iter()
            .find(|s| s.pane == p)
            .and_then(|s| s.live.as_ref())
            .map(|l| l.tier)
    };
    assert_eq!(tier(panes[0]), Some(Tier::Hot));
    assert_eq!(tier(panes[1]), Some(Tier::Warm));
}
