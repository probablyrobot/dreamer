//! The daemon switching view subscriptions between the stream and screen
//! frames on its own, through the real daemon: a pane goes to frames under
//! sustained output and back once it calms, without flipping at the
//! threshold; a view that selects, scrolls back, or searches in a pane on
//! frames brings it back to records and gets exactly what it gets with
//! frames off; a thousand switches under heavy output lose and repeat no
//! record; and effects (bells, title, clipboard) reach a view on frames
//! once each, before the frame that shows them.

mod frames_check;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use frames_check::Checker;
use prost::Message as _;
use replica::feed::{CellPos, Feed, Frames, Scroll, SelectEvent, ViewCommand};
use replica::{Follower, RetryPolicy};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::sync::mpsc::Receiver;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");
const FRAMES: &str = wire::feature::TERMINAL_FRAMES;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    /// A daemon whose `dreamer.yaml` holds `settings`.
    fn new(settings: &str) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-fsw-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::write(root.join("config/dreamer.yaml"), settings).expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
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

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    async fn client(&self, features: &[&str]) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect_features(&self.socket(), "test", 1 << 20, features).await {
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

    fn feed(&self, frames: Frames) -> Feed {
        Feed::start_frames(self.socket(), replica::Via::Local, Some(0), None, frames).expect("feed")
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

async fn hold(c: &Client, subscription: u64, hold: bool) {
    c.call(Request::TerminalHoldFeed(api::HoldFeed {
        subscription,
        hold,
    }))
    .await
    .expect("hold");
}

/// A subscription's pushed data, checked, with the text of its records
/// and frames kept for markers.
struct Watch {
    check: Checker,
    pushed: Receiver<Pushed>,
    /// The tail of the records' text.
    tail: String,
    /// When each automatic switch arrived, and to which feed.
    switches: Vec<(Instant, bool)>,
}

impl Watch {
    fn new(s: &api::Subscription, pushed: Receiver<Pushed>) -> Watch {
        Watch {
            check: Checker::new(s),
            pushed,
            tail: String::new(),
            switches: Vec::new(),
        }
    }

    fn take(&mut self, p: Pushed) {
        match &p {
            Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
                for r in api::RecordBatch::decode(payload.as_slice())
                    .expect("batch")
                    .records
                {
                    self.tail.push_str(&String::from_utf8_lossy(&r.data));
                }
                let keep = self.tail.len().saturating_sub(4096);
                self.tail = self.tail.split_off(self.tail.floor_char_boundary(keep));
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.feed" => {
                let s = api::FeedSwitch::decode(payload.as_slice()).expect("notice");
                self.switches
                    .push((Instant::now(), s.mode == api::SubscribeMode::Frames as i32));
            }
            _ => {}
        }
        self.check.push(p);
    }

    /// Whether the pane showed `marker`, on either feed.
    fn shows(&self, marker: &str) -> bool {
        self.tail.contains(marker)
            || (self.check.on_frames()
                && self
                    .check
                    .view
                    .rows
                    .iter()
                    .any(|r| r.text().contains(marker)))
    }

    /// Take pushed data until `done`, for at most `wait`.
    async fn until(&mut self, what: &str, wait: Duration, done: impl Fn(&Watch) -> bool) {
        let deadline = tokio::time::Instant::now() + wait;
        while !done(self) {
            let p = tokio::time::timeout_at(deadline, self.pushed.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{what}: timed out; switches {:?}, checker {:?}",
                        self.switches, self.check
                    )
                })
                .expect("open");
            self.take(p);
        }
    }

    /// Take pushed data for `wait`.
    async fn idle(&mut self, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(p) = tokio::time::timeout_at(deadline, self.pushed.recv()).await {
            self.take(p.expect("open"));
        }
    }
}

async fn views(c: &Client) -> Vec<api::ViewFeed> {
    match c
        .call(Request::DaemonDiagnostics(api::Empty {}))
        .await
        .expect("diagnostics")
    {
        Response::DaemonDiagnostics(d) => d.views,
        other => panic!("{other:?}"),
    }
}

/// About 35 KiB a second for `secs`: 2,000 characters every 50 ms,
/// between the test's two thresholds.
fn middling(secs: u32) -> String {
    format!(
        "i=0; while [ $i -lt {} ]; do printf '%2000s\\n' x; sleep 0.05; i=$((i+1)); done",
        secs * 20
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_pane_goes_to_frames_and_back_when_it_calms_without_flipping() {
    let s = Setup::new(
        "render:\n  frames_above: 128KiB\n  frames_below: 8KiB\n  frames_dwell: 500ms\n  frames_hold: 500ms\n",
    );
    let mut c = s.client(&[FRAMES]).await;
    let pushed = c.pushed().expect("pushed");
    let sub = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            automatic: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(sub.mode, api::SubscribeMode::Checkpoint as i32);
    let mut w = Watch::new(&sub, pushed);
    // Output between the thresholds: stays on the stream. Then heavy
    // output: to frames. Then between the thresholds again: stays on
    // frames. Then quiet: back to the stream.
    s.type_line(&format!(
        "{}; echo MID''1-DONE; seq 1 1500000; echo HEAVY''-DONE; {}; echo MID''2-DONE",
        middling(3),
        middling(3)
    ));
    let long = Duration::from_secs(60);
    w.until("the first middling output", long, |w| w.shows("MID1-DONE"))
        .await;
    assert!(
        w.switches.is_empty(),
        "output between the thresholds switched: {:?}",
        w.switches
    );
    w.until("frames under heavy output", long, |w| {
        !w.switches.is_empty()
    })
    .await;
    assert!(w.switches[0].1, "{:?}", w.switches);
    w.until("the end of the heavy output", long, |w| {
        w.shows("HEAVY-DONE")
    })
    .await;
    w.until("the second middling output", long, |w| w.shows("MID2-DONE"))
        .await;
    assert_eq!(
        w.switches.len(),
        1,
        "output between the thresholds after heavy output kept frames"
    );
    let calm = Instant::now();
    w.until("back to the stream once quiet", long, |w| {
        w.switches.len() == 2
    })
    .await;
    assert!(!w.switches[1].1, "{:?}", w.switches);
    // Not before the measuring window saw the quiet.
    assert!(w.switches[1].0 >= calm, "{:?}", w.switches);
    w.idle(Duration::from_millis(1500)).await;
    assert_eq!(w.switches.len(), 2, "a quiet pane stays on the stream");
    assert!(w.check.settled(), "{:?}", w.check);
    assert_eq!(w.check.notices, 2);
    assert!(
        w.check.frames > 0 && w.check.checkpoints >= 2,
        "{:?}",
        w.check
    );

    // The diagnostics show the subscription, its feed, and its switches.
    let v = views(&c).await;
    let mine = v
        .iter()
        .find(|v| v.subscription == sub.id && v.pane == 0)
        .unwrap_or_else(|| panic!("{v:?}"));
    assert_eq!(
        (mine.feed, mine.automatic, mine.switches, mine.to_frames),
        (api::TerminalFeed::Stream as i32, true, 2, 1),
        "{mine:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_switches_under_heavy_output_lose_and_repeat_nothing() {
    // Every switch here is the daemon's: a hold brings the subscription
    // back to the stream, and a release lets it go back to the frames it
    // asked for, 1 ms later.
    let s = Setup::new("render:\n  frames_dwell: 20ms\n  frames_hold: 1ms\n");
    let mut c = s.client(&[FRAMES]).await;
    let pushed = c.pushed().expect("pushed");
    let sub = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            feed: api::TerminalFeed::Frames as i32,
            ..Default::default()
        },
    )
    .await;
    let mut w = Watch::new(&sub, pushed);
    s.type_line("while :; do seq 1 200000; done");
    let wait = Duration::from_secs(30);
    w.until("output", wait, |w| w.check.frames > 2).await;
    let started = Instant::now();
    for i in 0..500 {
        hold(&c, sub.id, true).await;
        w.until("back on the stream", wait, |w| w.check.notices == 2 * i + 1)
            .await;
        hold(&c, sub.id, false).await;
        w.until("on frames again", wait, |w| w.check.notices == 2 * i + 2)
            .await;
    }
    // The last switch seen through, with output still coming.
    let frames = w.check.frames;
    w.until("frames after the last switch", wait, |w| {
        w.check.settled() && w.check.frames > frames + 2
    })
    .await;
    let elapsed = started.elapsed();
    assert_eq!(w.check.notices, 1000, "{:?}", w.check);
    assert!(w.check.checkpoints >= 500, "{:?}", w.check);
    assert!(w.check.records > 0, "{:?}", w.check);
    let v = views(&c).await;
    let mine = v
        .iter()
        .find(|v| v.subscription == sub.id)
        .unwrap_or_else(|| panic!("{v:?}"));
    assert_eq!((mine.switches, mine.to_frames), (1000, 500), "{mine:?}");
    eprintln!(
        "1000 switches in {elapsed:?}: {} records, {} frames, {} checkpoints",
        w.check.records, w.check.frames, w.check.checkpoints
    );
}

async fn focus(c: &Client, client: u64) {
    c.call(Request::TerminalInput(api::TerminalInput {
        pane: 0,
        client: Some(client),
        event: Some(api::terminal_input::Event::Focus(api::FocusInput {
            gained: true,
        })),
        ..api::TerminalInput::default()
    }))
    .await
    .expect("focus");
}

/// Bells, a title, and a clipboard write in the middle of heavy output,
/// then `FX-DONE-<tag>`.
fn effects(tag: u32) -> String {
    format!(
        "printf '\\033]2;FX-ONE\\007\\007'; seq 1 100000; printf '\\007\\033]52;c;RlgtQ0xJUA==\\007'; seq 1 100000; printf '\\007\\033]2;FX-TWO-{tag}\\007'; echo FX''-DONE-{tag}"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effects_reach_a_view_on_frames_once_each_and_before_their_frame() {
    let s = Setup::new("");
    let mut c = s.client(&[FRAMES]).await;
    let pushed = c.pushed().expect("pushed");
    let sub = subscribe(
        &c,
        api::TerminalSubscribe {
            pane: 0,
            feed: api::TerminalFeed::Frames as i32,
            client: Some(11),
            ..Default::default()
        },
    )
    .await;
    focus(&c, 11).await;
    let mut w = Watch::new(&sub, pushed);
    s.type_line(&effects(1));
    w.until("the end of the output", Duration::from_secs(60), |w| {
        w.shows("FX-DONE-1") && w.check.bells >= 3
    })
    .await;
    w.idle(Duration::from_millis(500)).await;
    // Each effect once, in order, before its frame (the checker), and
    // all of them on frames.
    assert_eq!(w.check.bells, 3, "{:?}", w.check);
    assert_eq!(w.check.effects, w.check.frame_effects, "{:?}", w.check);
    assert!(w.check.effects >= 3, "{:?}", w.check);
    assert_eq!(w.check.records, 0);

    // A view on frames takes them as a view on records does.
    let mut f = Follower::start_frames(
        s.socket(),
        replica::Via::Local,
        0,
        RetryPolicy::default(),
        Some(12),
        Frames::Always,
    )
    .await
    .expect("follow");
    focus(&c, 12).await;
    s.type_line(&effects(2));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let (mut bells, mut clip) = (0, Vec::new());
    loop {
        let shown = f
            .frame()
            .and_then(Result::ok)
            .is_some_and(|fr| fr.rows.iter().any(|r| r.text().contains("FX-DONE-2")));
        if shown && bells >= 3 {
            break;
        }
        let step = tokio::time::timeout_at(deadline, f.step())
            .await
            .unwrap_or_else(|_| panic!("timed out: {bells} bells, {:?}", f.meta))
            .expect("step");
        bells += step.bells;
        clip.extend(f.take_clipboard());
    }
    assert!(f.shows_frames(), "the view stayed on frames");
    assert_eq!(bells, 3);
    assert_eq!(clip, vec![b"FX-CLIP".to_vec()]);
    assert_eq!(f.meta.title.as_deref(), Some("FX-TWO-2"));
}

/// The newest frame of `feed` once `want` holds of it and of the feed.
async fn until_feed(feed: &Feed, what: &str, want: impl Fn(&Feed) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let _ = feed.take();
        if want(feed) {
            return;
        }
        if tokio::time::timeout_at(deadline, feed.wake.recv())
            .await
            .is_err()
        {
            panic!(
                "{what}: timed out; feed {:?}, on frames {}, held {}",
                feed.status(),
                feed.stats.on_frames.load(Relaxed),
                feed.stats.held.load(Relaxed),
            );
        }
    }
}

async fn text_of(feed: &Feed) -> String {
    feed.text().await.unwrap_or_default()
}

fn press(x: f32, y: f32) -> ViewCommand {
    ViewCommand::Select(SelectEvent::Press {
        at: CellPos { x, y },
        time_ns: 1,
    })
}

fn drag(x: f32, y: f32) -> ViewCommand {
    ViewCommand::Select(SelectEvent::Drag {
        at: CellPos { x, y },
    })
}

fn release(x: f32, y: f32) -> ViewCommand {
    ViewCommand::Select(SelectEvent::Release {
        at: CellPos { x, y },
    })
}

/// The same gesture on both views, then the text each selected.
async fn select_both(off: &Feed, on: &Feed, from: (f32, f32), to: (f32, f32)) -> (String, String) {
    for f in [off, on] {
        f.view(press(from.0, from.1));
        f.view(drag(to.0, to.1));
        f.view(release(to.0, to.1));
    }
    let a = off.selection_text().await.unwrap_or_default();
    let b = on.selection_text().await.unwrap_or_default();
    (a, b)
}

/// Clear the selection and scroll down: the view on frames lets the pane
/// go, and is back on frames after the hold.
async fn let_go(off: &Feed, on: &Feed) {
    for f in [off, on] {
        f.view(ViewCommand::ClearSelection);
        f.view(ViewCommand::Scroll(Scroll::Bottom));
    }
    until_feed(on, "back on frames", |f| {
        !f.stats.held.load(Relaxed) && f.stats.on_frames.load(Relaxed)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_scrolling_and_searching_on_frames_find_what_they_find_with_frames_off() {
    let s = Setup::new("render:\n  frames_hold: 200ms\n");
    s.type_line(
        "i=0; while [ $i -lt 300 ]; do echo \"line-$i alpha beta gamma $i\"; i=$((i+1)); done; echo CONTENT''-DONE",
    );
    let off = s.feed(Frames::Off);
    let on = s.feed(Frames::Always);
    let done = |f: &Feed| {
        f.status() == replica::feed::FeedStatus::Following && f.stats.frames.load(Relaxed) > 0
    };
    until_feed(&off, "the view on records", done).await;
    until_feed(&on, "the view on frames", |f| {
        done(f) && f.stats.on_frames.load(Relaxed)
    })
    .await;
    // Both show the end of the output.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let a = text_of(&off).await;
        if a.contains("\nCONTENT-DONE") {
            break;
        }
        assert!(Instant::now() < deadline, "the output never finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    until_feed(&on, "the end on frames", |f| {
        f.stats.on_frames.load(Relaxed)
    })
    .await;
    assert!(on.stats.on_frames.load(Relaxed));
    let switches = on.stats.feed_switches.load(Relaxed);

    // A selection on the screen, started on frames.
    let (a, b) = select_both(&off, &on, (2.0, 3.0), (25.0, 12.0)).await;
    assert!(a.contains("line-") && a.lines().count() >= 9, "{a:?}");
    assert_eq!(a, b, "a selection started on frames");
    assert!(on.stats.deferred.load(Relaxed) > 0, "the gesture waited");
    assert!(on.stats.held.load(Relaxed) && !on.stats.on_frames.load(Relaxed));
    let_go(&off, &on).await;

    // Scrolled into history, from frames.
    for f in [&off, &on] {
        f.view(ViewCommand::Scroll(Scroll::Delta(-150)));
    }
    let (a, b) = select_both(&off, &on, (0.0, 0.0), (40.0, 8.0)).await;
    assert!(a.contains("line-1"), "{a:?}");
    assert!(
        !a.contains("line-29"),
        "the selection is in history, not on the screen: {a:?}"
    );
    assert_eq!(a, b, "a selection in history, started on frames");
    let_go(&off, &on).await;

    // Search and the history's text (copy mode reads it), from frames.
    let query = || replica::search::Query {
        text: "gamma 1".into(),
        regex: false,
    };
    let found_off = off.search(query()).await.expect("search").expect("found");
    let found_on = on.search(query()).await.expect("search").expect("found");
    assert!(found_off.hits.len() > 100, "{found_off:?}");
    assert_eq!(found_off, found_on, "a search started on frames");
    let (text_off, text_on) = (text_of(&off).await, text_of(&on).await);
    assert!(text_off.contains("line-0 alpha"), "the whole history");
    assert_eq!(text_off, text_on, "the history's text from frames");
    // Done with the history: back on frames after the hold.
    until_feed(&on, "back on frames after the search", |f| {
        !f.stats.held.load(Relaxed) && f.stats.on_frames.load(Relaxed)
    })
    .await;
    assert!(
        on.stats.feed_switches.load(Relaxed) >= switches + 6,
        "three round trips"
    );
}
