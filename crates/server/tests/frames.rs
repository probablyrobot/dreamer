//! Screen frames through the real daemon: a client that asked for the
//! `terminal.frames` feature follows a pane under heavy output on frames,
//! each frame equals the daemon's own screen at the frame's sequence, a
//! subscription switches between the stream and frames many times with no
//! record lost or repeated, and a client that did not ask never gets a
//! frame.

mod frames_check;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use frames_check::Checker;
use prost::Message as _;
use terminal_ghostty::core::{GridSize, Row};
use terminal_ghostty::{Engine, EngineConfig};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Daemon {
    root: PathBuf,
    tmux: TmuxServer,
    launcher: std::process::Child,
}

impl Daemon {
    fn start() -> Daemon {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-frm-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let launcher = Command::new(EXE)
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
        Daemon {
            root,
            tmux,
            launcher,
        }
    }

    /// A client asking for `features`, holding plenty of pushed data so a
    /// test can make calls without reading it meanwhile.
    async fn client(&self, features: &[&str]) -> Client {
        let socket = self.root.join("rt/dreamerd.sock");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_features(&socket, "test", 1 << 20, features).await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn type_line(&self, line: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.launcher.kill();
        let _ = self.launcher.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const FRAMES: &str = wire::feature::TERMINAL_FRAMES;

/// Colored lines with wide characters and attributes, then plain numbers
/// as fast as the pane takes them, then a marker.
const HEAVY: &str = "i=0; while [ $i -lt 1500 ]; do printf '\\033[3%dm%05d \\033[1mbold\\033[0m 世界 \\033[4;38;2;10;200;30mrgb\\033[0m\\n' $((i%8)) $i; i=$((i+1)); done; seq 1 150000; echo HEAVY-DONE";

async fn subscribe(c: &Client, req: api::TerminalSubscribe) -> api::Subscription {
    match c
        .call(Request::TerminalSubscribe(req))
        .await
        .expect("subscribe")
    {
        Response::TerminalSubscribe(s) => s,
        other => panic!("{other:?}"),
    }
}

async fn set_feed(c: &Client, subscription: u64, feed: api::TerminalFeed) -> api::FeedSwitch {
    match c
        .call(Request::TerminalSetFeed(api::SetFeed {
            subscription,
            feed: feed as i32,
        }))
        .await
        .expect("set feed")
    {
        Response::TerminalSetFeed(s) => s,
        other => panic!("{other:?}"),
    }
}

fn shows(view: &replica::screen::ScreenView, text: &str) -> bool {
    view.rows.iter().any(|r| r.text() == text)
}

/// A client following the pane on the stream, as the reference: an engine
/// restored from the checkpoint, then every record in order.
struct Reference {
    engine: Option<Engine>,
    scrollback: usize,
    applied: u64,
    records: Vec<api::TerminalRecord>,
}

impl Reference {
    fn config(&self) -> EngineConfig {
        let mut c = EngineConfig::new(GridSize { cols: 80, rows: 24 });
        c.scrollback_lines = Some(self.scrollback);
        c.scrollback_bytes = Some(usize::MAX);
        c.continuation_max_bytes = Some(tmux_backend::bootstrap::CONTINUATION_MAX_BYTES);
        c
    }

    fn take(&mut self, p: Pushed) {
        match p {
            Pushed::Bulk { topic, data, .. } if topic == "terminal.checkpoint" => {
                let d = api::CheckpointData::decode(data.as_slice()).expect("checkpoint");
                self.engine =
                    Some(Engine::from_snapshot(&d.snapshot, &self.config()).expect("restore"));
                self.applied = d.sequence;
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
                let b = api::RecordBatch::decode(payload.as_slice()).expect("batch");
                self.records.extend(b.records);
            }
            other => panic!("reference: unexpected {other:?}"),
        }
    }

    /// Apply the records in order; after each, `check` sees the engine at
    /// that sequence.
    fn replay(&mut self, mut check: impl FnMut(u64, &mut Engine)) {
        let engine = self.engine.as_mut().expect("checkpoint");
        for r in std::mem::take(&mut self.records) {
            if r.sequence <= self.applied {
                continue;
            }
            assert_eq!(r.sequence, self.applied + 1, "reference: a gap");
            match (r.cols, r.rows) {
                (Some(c), Some(rows)) => engine
                    .resize(
                        GridSize {
                            cols: c as u16,
                            rows: rows as u16,
                        },
                        mirror::worker::CELL_PX,
                    )
                    .expect("resize"),
                _ => drop(engine.feed(&r.data).expect("feed")),
            }
            self.applied = r.sequence;
            check(r.sequence, engine);
        }
    }
}

/// What a frame showed, after it was applied.
struct Seen {
    rows: Vec<Arc<Row>>,
    cursor: terminal_ghostty::core::Cursor,
    top: u64,
    modes: terminal_ghostty::core::InputModes,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_equal_the_mirrors_screen_at_their_sequence_under_heavy_output() {
    let d = Daemon::start();
    // The reference subscribes first, so its checkpoint is at or before
    // every frame.
    let mut r = d.client(&[]).await;
    let mut r_pushed = r.pushed().expect("pushed");
    let rs = subscribe(
        &r,
        api::TerminalSubscribe {
            pane: 0,
            ..Default::default()
        },
    )
    .await;
    let mut reference = Reference {
        engine: None,
        scrollback: rs.scrollback_lines as usize,
        applied: rs.sequence,
        records: Vec::new(),
    };
    let mut c = d.client(&[FRAMES]).await;
    assert!(c.has_feature(FRAMES));
    let mut pushed = c.pushed().expect("pushed");
    let s = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            feed: api::TerminalFeed::Frames as i32,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s.mode, api::SubscribeMode::Frames as i32, "{s:?}");
    assert_eq!((s.frame, s.checkpoint_bytes), (1, 0));
    assert!(s.sequence >= rs.sequence);
    let mut check = Checker::new(&s);
    let mut seen: HashMap<u64, Seen> = HashMap::new();

    d.type_line(HEAVY);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut done_at = None;
    loop {
        tokio::select! {
            p = r_pushed.recv() => reference.take(p.expect("reference open")),
            p = pushed.recv() => {
                let p = p.expect("open");
                // Never anything but frames on the frames feed.
                assert!(
                    matches!(&p, Pushed::Bulk { topic, .. } if topic == "terminal.frame"),
                    "{p:?}"
                );
                check.push(p);
                let v = &check.view;
                seen.insert(v.sequence, Seen {
                    rows: v.rows.clone(),
                    cursor: v.cursor,
                    top: v.top,
                    modes: v.modes,
                });
                if done_at.is_none() && shows(v, "HEAVY-DONE") {
                    done_at = Some(v.sequence);
                }
            }
            () = tokio::time::sleep_until(deadline) => panic!("the output never finished on frames"),
        }
        // Done once the reference has every record up to the last frame.
        if let Some(at) = done_at
            && reference.records.last().is_some_and(|r| r.sequence >= at)
        {
            break;
        }
    }
    assert!(check.frames >= 10, "only {} frames", check.frames);
    assert_eq!(check.full_frames, 1, "only the first frame is full");

    // The daemon's own screen at each frame's sequence, from the reference
    // built of its checkpoint and records.
    let mut compared = 0;
    reference.replay(|seq, engine| {
        let Some(want) = seen.get(&seq) else {
            return;
        };
        let f = engine.frame().expect("frame");
        assert_eq!(want.rows.len(), f.rows.len(), "rows at {seq}");
        for (i, (a, b)) in want.rows.iter().zip(&f.rows).enumerate() {
            assert_eq!(**a, **b, "row {i} at sequence {seq}");
        }
        assert_eq!(want.cursor, f.cursor, "cursor at {seq}");
        assert_eq!(want.top, f.view.offset, "first row's number at {seq}");
        assert_eq!(
            want.modes,
            engine.input_modes().expect("modes"),
            "modes at {seq}"
        );
        compared += 1;
    });
    assert!(compared >= 10, "compared {compared} frames");
    assert!(
        seen.values().any(|s| s
            .rows
            .iter()
            .any(|r| r.cells.iter().any(|c| c.style.attrs.0 != 0))),
        "styled cells reached the frames"
    );
    drop(r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_feeds_back_and_forth_loses_and_repeats_nothing() {
    let d = Daemon::start();
    let mut c = d.client(&[FRAMES]).await;
    let mut pushed = c.pushed().expect("pushed");
    let s = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            ..Default::default()
        },
    )
    .await;
    let mut check = Checker::new(&s);
    let drain = |check: &mut Checker, pushed: &mut Receiver<Pushed>| {
        while let Ok(p) = pushed.try_recv() {
            check.push(p);
        }
    };
    d.type_line(HEAVY);
    let mut switches = 0;
    for i in 0..40 {
        tokio::time::sleep(Duration::from_millis(15 + (i % 4) * 10)).await;
        let feed = if i % 2 == 0 {
            api::TerminalFeed::Frames
        } else {
            api::TerminalFeed::Stream
        };
        let r = set_feed(&c, s.id, feed).await;
        check.switched(r);
        switches += 1;
        drain(&mut check, &mut pushed);
    }
    // Back on frames, to the end of the output.
    let r = set_feed(&c, s.id, api::TerminalFeed::Frames).await;
    check.switched(r);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !(check.settled() && shows(&check.view, "HEAVY-DONE")) {
        let p = tokio::time::timeout_at(deadline, pushed.recv())
            .await
            .expect("the output never finished")
            .expect("open");
        check.push(p);
    }
    assert!(switches == 40 && check.checkpoints >= 20, "{check:?}");
    assert!(check.records > 0 && check.frames > 20, "{check:?}");
    // A connection that asked may also switch a subscription it made on
    // the stream; an unknown one is refused.
    let e = c
        .call(Request::TerminalSetFeed(api::SetFeed {
            subscription: 999,
            feed: api::TerminalFeed::Frames as i32,
        }))
        .await
        .expect_err("unknown subscription");
    assert!(
        matches!(e, client::ClientError::Rpc(ref e) if e.code == api::code::NOT_FOUND),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_did_not_ask_never_gets_a_frame() {
    let d = Daemon::start();
    let mut c = d.client(&[]).await;
    assert!(!c.has_feature(FRAMES));
    let mut pushed = c.pushed().expect("pushed");
    let refused = c
        .call(Request::TerminalSubscribe(api::TerminalSubscribe {
            pane: 0,
            feed: api::TerminalFeed::Frames as i32,
            ..Default::default()
        }))
        .await
        .expect_err("frames without the feature");
    assert!(
        matches!(refused, client::ClientError::Rpc(ref e) if e.code == api::code::INVALID_REQUEST),
        "{refused}"
    );
    let s = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s.mode, api::SubscribeMode::Checkpoint as i32);
    let refused = c
        .call(Request::TerminalSetFeed(api::SetFeed {
            subscription: s.id,
            feed: api::TerminalFeed::Frames as i32,
        }))
        .await
        .expect_err("a switch to frames without the feature");
    assert!(
        matches!(refused, client::ClientError::Rpc(ref e) if e.code == api::code::INVALID_REQUEST),
        "{refused}"
    );
    let mut check = Checker::new(&s);
    d.type_line("seq 1 20000; echo QUIET-DONE");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut tail = String::new();
    while !tail.contains("QUIET-DONE") {
        let p = tokio::time::timeout_at(deadline, pushed.recv())
            .await
            .expect("the output never finished")
            .expect("open");
        assert!(
            !matches!(&p, Pushed::Bulk { topic, .. } if topic == "terminal.frame"),
            "a frame reached a client that did not ask"
        );
        if let Pushed::Event { topic, payload, .. } = &p
            && topic == "terminal.records"
        {
            for r in api::RecordBatch::decode(payload.as_slice())
                .expect("batch")
                .records
            {
                tail.push_str(&String::from_utf8_lossy(&r.data));
            }
            let keep = tail.len().saturating_sub(64);
            tail = tail.split_off(tail.floor_char_boundary(keep));
        }
        check.push(p);
    }
    assert_eq!(check.frames, 0);
}
