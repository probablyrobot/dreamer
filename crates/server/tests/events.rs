//! The event log through a real daemon: cursors
//! resume, and a subscriber that falls behind gets a gap naming what it
//! missed, then live events. The daemon runs with small queues, so its log
//! keeps 16 events.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{EventGapReason, Request, Response};
use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

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
            "/tmp/drm-ev-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("DREAMER_TEST_SMALL_QUEUES", "1")
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

    fn tmux(&self, args: &[&str]) {
        self.tmux.run_checked(args).expect("tmux command");
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

async fn subscribe(c: &mut Client, p: api::EventSubscribe) -> api::EventSubscription {
    match c
        .call(Request::EventsSubscribe(p))
        .await
        .expect("subscribe")
    {
        Response::EventsSubscribe(s) => s,
        other => panic!("unexpected {other:?}"),
    }
}

/// The next `events.batch` within `wait`.
async fn batch(rx: &mut Receiver<Pushed>, wait: Duration) -> Option<api::EventBatch> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "events.batch" => {
                return api::EventBatch::decode(payload.as_slice()).ok();
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Batches until one ends at or after `sequence`.
async fn until(rx: &mut Receiver<Pushed>, sequence: u64) -> Vec<api::EventBatch> {
    let mut got = Vec::new();
    loop {
        let b = batch(rx, Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("no batch reaching {sequence}"));
        let done = b.cursor >= sequence;
        got.push(b);
        if done {
            return got;
        }
    }
}

/// Make `n` topology changes, each seen by `rx` before the next, and
/// return the newest sequence.
async fn changes(s: &Setup, rx: &mut Receiver<Pushed>, n: usize) -> u64 {
    let mut last = 0;
    for i in 0..n {
        s.tmux(&[
            "new-window",
            "-d",
            "-t",
            "main",
            "-n",
            &format!("w{i}"),
            "/bin/sh",
        ]);
        loop {
            let b = batch(rx, Duration::from_secs(10))
                .await
                .expect("a topology event per change");
            if let Some(e) = b.events.last() {
                last = e.sequence;
                break;
            }
        }
    }
    last
}

#[tokio::test]
async fn cursors_resume_and_a_subscriber_that_falls_behind_gets_a_gap() {
    let s = Setup::new();
    let mut live = s.client().await;
    let mut live_rx = live.pushed().expect("pushed");
    // The daemon reaches tmux on first use; topology events follow from
    // then on.
    live.call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology");
    let sub = subscribe(
        &mut live,
        api::EventSubscribe {
            kinds: vec!["tmux".into()],
            ..api::EventSubscribe::default()
        },
    )
    .await;
    // Three changes; remember where the first one ended.
    let first = changes(&s, &mut live_rx, 1).await;
    let third = changes(&s, &mut live_rx, 2).await;
    assert!(third > first);

    // A second client resumes after the first change: the rest replays.
    let mut resumed = s.client().await;
    let mut resumed_rx = resumed.pushed().expect("pushed");
    subscribe(
        &mut resumed,
        api::EventSubscribe {
            kinds: vec!["tmux.topology".into()],
            after: Some(first),
            instance: Some(sub.instance.clone()),
            ..api::EventSubscribe::default()
        },
    )
    .await;
    let replay = until(&mut resumed_rx, third).await;
    assert!(replay.iter().all(|b| b.gap.is_none()), "{replay:?}");
    let seqs: Vec<u64> = replay
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.sequence))
        .collect();
    assert_eq!(seqs.first(), Some(&(first + 1)));
    assert_eq!(seqs.last(), Some(&third));
    assert!(replay[0].events.iter().all(|e| e.kind == "tmux.topology"));

    // A subscriber allowed to fall only one event behind, resuming from
    // the start: it skips ahead with a gap, then gets the newest event.
    let mut slow = s.client().await;
    let mut slow_rx = slow.pushed().expect("pushed");
    subscribe(
        &mut slow,
        api::EventSubscribe {
            after: Some(0),
            instance: Some(sub.instance.clone()),
            quota: Some(1),
            ..api::EventSubscribe::default()
        },
    )
    .await;
    let b = batch(&mut slow_rx, Duration::from_secs(10))
        .await
        .expect("batch");
    let gap = b.gap.expect("a gap for the skipped events");
    assert_eq!(gap.reason, EventGapReason::Lagging as i32);
    assert_eq!((gap.first, gap.last), (1, third - 1));
    assert_eq!(
        b.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        [third]
    );
    // Then live events, with no gap.
    let next = changes(&s, &mut live_rx, 1).await;
    let b = until(&mut slow_rx, next).await;
    assert!(b.iter().all(|b| b.gap.is_none()));

    // More changes than the log keeps: an old cursor has expired.
    let newest = changes(&s, &mut live_rx, 18).await;
    let mut late = s.client().await;
    let mut late_rx = late.pushed().expect("pushed");
    let sub2 = subscribe(
        &mut late,
        api::EventSubscribe {
            after: Some(first),
            instance: Some(sub.instance.clone()),
            ..api::EventSubscribe::default()
        },
    )
    .await;
    assert!(sub2.oldest > first + 1);
    let b = until(&mut late_rx, newest).await;
    let gap = b[0].gap.clone().expect("expired gap");
    assert_eq!(gap.reason, EventGapReason::Expired as i32);
    assert_eq!((gap.first, gap.last), (first + 1, sub2.oldest - 1));

    // A cursor from another daemon instance.
    let mut other = s.client().await;
    let mut other_rx = other.pushed().expect("pushed");
    subscribe(
        &mut other,
        api::EventSubscribe {
            after: Some(3),
            instance: Some("another-instance".into()),
            ..api::EventSubscribe::default()
        },
    )
    .await;
    let b = batch(&mut other_rx, Duration::from_secs(10))
        .await
        .expect("batch");
    assert_eq!(
        b.gap.expect("restarted gap").reason,
        EventGapReason::Restarted as i32
    );
}

#[tokio::test]
async fn action_outcomes_are_events_with_their_pane() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut rx = c.pushed().expect("pushed");
    subscribe(
        &mut c,
        api::EventSubscribe {
            kinds: vec!["actions".into()],
            ..api::EventSubscribe::default()
        },
    )
    .await;
    let Response::TmuxTopology(t) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .expect("topology")
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    let Response::ActionsSubmit(r) = c
        .call(Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action {
                kind: Some(api::action::Kind::SplitPane(api::SplitPane {
                    pane,
                    direction: api::SplitDirection::Vertical as i32,
                    argv: vec!["sleep".into(), "600".into()],
                    ..api::SplitPane::default()
                })),
            }),
            ..api::SubmitAction::default()
        }))
        .await
        .expect("split")
    else {
        panic!("split")
    };
    let b = batch(&mut rx, Duration::from_secs(10))
        .await
        .expect("batch");
    let e = &b.events[0];
    assert_eq!(e.kind, "actions.outcome");
    assert_eq!(e.subject.as_deref(), Some(r.id.as_str()));
    assert_eq!(e.state.as_deref(), Some("completed"), "{:?}", r.reason);
    assert_eq!(e.pane, Some(pane));
}
