//! The GUI benchmark, driven from inside the app so each
//! measure ends at a paint: `cargo xtask bench gui` prepares an isolated
//! tmux server and daemon and starts the app with `DREAMER_BENCH_GUI=FILE`.
//! Every pane prints `READY` first. The app then measures, in order:
//!
//! 1. **Launch:** from process start until every visible pane has painted
//!    its `READY` line (the daemon is already running).
//! 2. **Idle:** terminal paints over 3 s with nothing changing (target:
//!    none).
//! 3. **Key to echo, idle:** a key sent through the pane's input path until
//!    a paint shows the cursor moved.
//! 4. **Frame work under load:** every other visible pane prints a line
//!    every 20 ms, or replays a byte corpus nonstop (`DREAMER_BENCH_LOAD`);
//!    for 5 s, each window frame split into its parts (see
//!    [`frame_results`]) and the time between frames.
//! 5. **Key to echo under that load.**
//! 6. **Tab switch:** selecting the other tab until each of its panes has
//!    painted its content.
//! 7. **Streaming transcript:** a native panel drawing an agent session's
//!    transcript while a long answer streams in, a delta every 10 ms: each
//!    render's build time, the time between renders, and how long a delta
//!    waits to be drawn.
//!
//! The results are written to the file as JSON, and the process exits.
//! The harness passes the pane count, the visible pane count, the load,
//! the machine, and the revision in `DREAMER_BENCH_PANES`,
//! `DREAMER_BENCH_VISIBLE`, `DREAMER_BENCH_LOAD`, `DREAMER_BENCH_MACHINE`,
//! and `DREAMER_BENCH_REVISION` (each JSON); the file carries them as
//! given, with the display backend.
//!
//! **Not run.** A wait that runs out while the window does not draw when
//! asked (a locked or sleeping display, a window the system stopped
//! refreshing) proves nothing. So does an idle count of zero paints from a
//! window that is not drawing. Then the file says `"not_run": true` with
//! the reason, holds no results, and the process exits 5, as the gate's
//! smoke runs do.
//!
//! **Focus.** The window is opened without GPUI's 30 fps limit for
//! unfocused windows, asks for focus once, and the file records whether
//! it had focus at every measure (`window_focused`).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, WeakEntity};
use serde_json::json;

use crate::workspace::{FRAME_LOG, FrameMark, Workspace};

/// The results file.
static OUT: OnceLock<PathBuf> = OnceLock::new();

fn fail(why: &str) -> ! {
    eprintln!("bench: {why}");
    std::process::exit(3)
}

/// A value the harness passed in, as JSON.
fn given(name: &str) -> serde_json::Value {
    std::env::var(name)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The results file's contents: what ran, where, and the results, or
/// that nothing could be measured and why.
fn report(results: Vec<serde_json::Value>, not_run: Option<&str>) -> serde_json::Value {
    let mut r = json!({
        "workload": "gui",
        "panes": given("DREAMER_BENCH_PANES"),
        "visible": given("DREAMER_BENCH_VISIBLE"),
        "load": given("DREAMER_BENCH_LOAD"),
        "backend": crate::display_backend(),
        "hardware": given("DREAMER_BENCH_MACHINE"),
        "revision": given("DREAMER_BENCH_REVISION"),
        "not_measured": [NOT_MEASURED],
        // GPUI's unfocused-window frame limit is off in bench runs, so
        // these hold either way; recorded because the input path differs.
        "window_focused": FOCUSED_THROUGHOUT.load(std::sync::atomic::Ordering::Relaxed),
        "results": results,
    });
    if let Some(why) = not_run {
        // Whatever was measured before is not evidence.
        r["results"] = json!([]);
        r["not_run"] = json!(true);
        r["reason"] = json!(why);
    }
    r
}

fn write(report: &serde_json::Value) {
    let Some(out) = OUT.get() else { return };
    if let Err(e) = std::fs::write(
        out,
        serde_json::to_string_pretty(report).unwrap_or_default(),
    ) {
        fail(&format!("{}: {e}", out.display()));
    }
}

/// Nothing this run measured is evidence: record why and exit 5.
fn not_run(why: &str) -> ! {
    println!("bench: not run: {why}");
    write(&report(Vec::new(), Some(why)));
    std::process::exit(crate::INCONCLUSIVE)
}

/// Whether the window draws a frame when asked within half a second.
async fn drawing(any: AnyWindowHandle, cx: &mut AsyncApp) -> bool {
    let before = crate::workspace::draws();
    let _ = cx.update_window(any, |_, window, _| window.refresh());
    sleep(cx, Duration::from_millis(500)).await;
    crate::workspace::draws() != before
}

/// Whether a bench run is asked for (`DREAMER_BENCH_GUI`). Its window
/// is opened without GPUI's frame limit for unfocused windows (30 frames
/// a second): a compositor may keep focus elsewhere (Hyprland does not
/// hand it to a window started from a shell while another has it), and
/// what is measured is the app's latency, not that limit.
pub fn requested() -> bool {
    std::env::var_os("DREAMER_BENCH_GUI").is_some()
}

/// Whether the window had keyboard focus at every measure so far.
static FOCUSED_THROUGHOUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Note whether the window has focus now; asks for it once at the start.
fn note_focus(any: AnyWindowHandle, cx: &mut AsyncApp) {
    let focused = cx
        .update_window(any, |_, window, _| window.is_window_active())
        .unwrap_or(false);
    if !focused {
        FOCUSED_THROUGHOUT.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

const NOT_DRAWING: &str = "the window did not draw when asked (screen locked, display asleep, or the window hidden or behind another app)";

/// A wait for `what` ran out: not run if the window is not drawing, else
/// a failure.
async fn stalled(any: AnyWindowHandle, cx: &mut AsyncApp, what: &str) -> ! {
    if !drawing(any, cx).await {
        not_run(&format!("never saw {what}, and {NOT_DRAWING}"));
    }
    fail(&format!("never saw: {what}"))
}

fn read<T>(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&Workspace, &gpui::App) -> T,
) -> Option<T> {
    cx.update_window(any, |_, _, cx| ws.read_with(cx, |w, cx| f(w, cx)).ok())
        .ok()
        .flatten()
}

async fn sleep(cx: &mut AsyncApp, d: Duration) {
    cx.background_executor().timer(d).await;
}

/// Poll every millisecond until `check` holds; `fail` after `limit`.
async fn until(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    what: &str,
    limit: Duration,
    check: impl Fn(&Workspace, &gpui::App) -> bool,
) -> Instant {
    let deadline = Instant::now() + limit;
    loop {
        if read(any, ws, cx, &check) == Some(true) {
            return Instant::now();
        }
        if Instant::now() > deadline {
            stalled(any, cx, what).await;
        }
        sleep(cx, Duration::from_millis(1)).await;
    }
}

/// Every visible pane shows something: its `READY` line, or once a
/// small pane has scrolled it away, its prompt.
fn all_show(w: &Workspace, cx: &gpui::App, _text: &str) -> bool {
    let ids = w.pane_ids();
    !ids.is_empty()
        && ids.iter().all(|id| {
            w.pane_shell(*id)
                .is_some_and(|s| !s.read(cx).frame.text_dump().trim().is_empty())
        })
}

fn tmux(daemon: &[String], args: &[&str]) {
    let opt = |name: &str| {
        daemon
            .iter()
            .position(|a| a == name)
            .and_then(|i| daemon.get(i + 1))
            .cloned()
    };
    let program = opt("--tmux").unwrap_or_else(|| "tmux".into());
    let Some(socket) = opt("--tmux-socket") else {
        fail("the GUI benchmark needs an isolated tmux (-L)")
    };
    let _ = std::process::Command::new(program)
        .args(["-L", &socket])
        .args(args)
        .status();
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

/// `samples` keys into `pane`, each timed until a paint shows the cursor
/// moved.
async fn key_to_echo(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    pane: u32,
    samples: usize,
    cx: &mut AsyncApp,
) -> Vec<f64> {
    let cursor = |w: &Workspace, cx: &gpui::App| {
        w.pane_shell(pane).map(|s| {
            let f = &s.read(cx).frame;
            (f.cursor.x, f.cursor.y)
        })
    };
    let feed = read(any, ws, cx, |w, cx| {
        w.pane_shell(pane).and_then(|s| s.read(cx).feed.clone())
    })
    .flatten()
    .unwrap_or_else(|| fail("the typing pane has no feed"));
    let mut out = Vec::with_capacity(samples);
    for i in 0..samples {
        if i % 30 == 0 {
            // A fresh line (Ctrl-U), untimed.
            feed.send(api::terminal_input::Event::Text(api::TextInput {
                text: "\u{15}".into(),
            }));
            sleep(cx, Duration::from_millis(100)).await;
        }
        let before = read(any, ws, cx, cursor).flatten();
        let sent = Instant::now();
        feed.send(api::terminal_input::Event::Text(api::TextInput {
            text: "k".into(),
        }));
        // The pane's frame with the cursor moved, painted.
        until(
            any,
            ws,
            cx,
            "the echo painted",
            Duration::from_secs(20),
            |w, cx| {
                w.pane_shell(pane).is_some_and(|s| {
                    let f = &s.read(cx).frame;
                    Some((f.cursor.x, f.cursor.y)) != before && terminal_render::was_painted(f)
                })
            },
        )
        .await;
        out.push(ms(sent.elapsed()));
        sleep(cx, Duration::from_millis(10)).await;
    }
    out
}

/// Deltas a streaming transcript gets, one every [`DELTA_EVERY`].
const DELTAS: u64 = 500;
const DELTA_EVERY: Duration = Duration::from_millis(10);

/// A native panel draws an agent session's transcript while a long answer
/// streams into it: a delta every 10 ms for 5 s (about 4,000 words of
/// markdown, with code blocks), then a tool card and an approval card.
/// The deltas go straight into the panel's streams, as a follow pushes
/// them, so this measures the desktop's side: how long each panel render
/// takes to build, how often it renders, and how long a delta waits to be
/// drawn.
async fn transcript(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    put: &mut impl FnMut(&str, f64, &str),
) {
    use api::agent_session_event::Kind as K;
    use view_tree::{Bind, Frame, Kind, Node};
    let streams = app_model::bound::Streams::default();
    let mut root = Node::new("main", Kind::Column);
    root.children = vec![
        Node::new(
            "chat",
            Kind::Transcript {
                thinking: false,
                back: 0,
            },
        )
        .bound(Bind::Session("bench".into())),
        Node::new(
            "prompt",
            Kind::Composer {
                label: String::new(),
                value: String::new(),
                placeholder: "Ask".into(),
                attachments: Vec::new(),
            },
        )
        .bound(Bind::Session("bench".into())),
    ];
    let tree = Frame {
        title: "Transcript".into(),
        status: String::new(),
        focus: Some("prompt".into()),
        root,
    };
    let st = streams.clone();
    let panel = cx
        .update_window(any, |_, _, cx| {
            ws.upgrade().map(|ws| {
                ws.update(cx, |w, cx| {
                    let p = cx.new(|cx| crate::panel::AppPanel::fixed(tree, st, cx));
                    w.panel = Some(p.clone());
                    cx.notify();
                    p
                })
            })
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| fail("no workspace for the transcript"));
    let feed = |seq: u64, kind: K| {
        streams.take(&api::Notification::AgentSessionsEvents(
            api::AgentSessionEvents {
                session: "bench".into(),
                subscription: Some(1),
                events: vec![api::AgentSessionEvent {
                    seq,
                    unix_ms: 0,
                    kind: Some(kind),
                }],
                more: false,
            },
        ));
    };
    let text = |t: String| api::AgentText { text: t };
    feed(
        1,
        K::UserMessage(text("explain the build, at length".into())),
    );
    sleep(cx, Duration::from_millis(500)).await;
    if let Ok(mut log) = crate::panel::RENDER_LOG.lock() {
        *log = Some(Vec::new());
    }
    let mut fed: Vec<(Instant, u64)> = Vec::new();
    for i in 0..DELTAS {
        let delta = match i % 50 {
            0 => format!("\n\n## Part {}\n\n", i / 50 + 1),
            20 => "\n\n```rust\nfn main() {\n    println!(\"step\");\n}\n```\n\n".into(),
            _ => format!("Word {i} of the answer, with **some** `code` and more text. "),
        };
        feed(i + 2, K::TextDelta(text(delta)));
        fed.push((Instant::now(), streams.revision()));
        cx.update(|cx| panel.update(cx, |_, cx| cx.notify()));
        sleep(cx, DELTA_EVERY).await;
    }
    feed(
        DELTAS + 2,
        K::ToolCall(api::AgentToolCall {
            id: "t1".into(),
            name: "bash".into(),
            status: api::AgentToolStatus::Finished as i32,
            input: Some(r#"{"command":"cargo build"}"#.into()),
            output: Some("   Compiling dreamer\n    Finished".into()),
        }),
    );
    feed(
        DELTAS + 3,
        K::ApprovalRequest(api::AgentApprovalRequest {
            id: "a1".into(),
            title: "Allow bash?".into(),
            message: Some("cargo test".into()),
            options: Vec::new(),
            expires_unix_ms: None,
        }),
    );
    cx.update(|cx| panel.update(cx, |_, cx| cx.notify()));
    sleep(cx, Duration::from_millis(300)).await;
    let renders = crate::panel::RENDER_LOG
        .lock()
        .ok()
        .and_then(|mut l| l.take())
        .unwrap_or_default();
    let mut work: Vec<f64> = renders.iter().map(|r| r.1 as f64 / 1e6).collect();
    let mut gaps: Vec<f64> = renders.windows(2).map(|w| ms(w[1].0 - w[0].0)).collect();
    // A delta waits from being taken until a render that draws it.
    let mut waits: Vec<f64> = fed
        .iter()
        .filter_map(|(at, rev)| {
            renders
                .iter()
                .find(|r| r.2 >= *rev && r.0 >= *at)
                .map(|r| ms(r.0 - *at) + r.1 as f64 / 1e6)
        })
        .collect();
    put(
        &format!("transcript: panel renders while {DELTAS} deltas stream over 5 s"),
        renders.len() as f64,
        "renders",
    );
    put("transcript: deltas drawn", waits.len() as f64, "deltas");
    put("transcript: render time, p50", pct(&mut work, 0.5), "ms");
    put("transcript: render time, p95", pct(&mut work, 0.95), "ms");
    put("transcript: render time, max", pct(&mut work, 1.0), "ms");
    put(
        "transcript: time between renders, p95",
        pct(&mut gaps, 0.95),
        "ms",
    );
    put(
        "transcript: delta to drawn, p50",
        pct(&mut waits, 0.5),
        "ms",
    );
    put(
        "transcript: delta to drawn, p95",
        pct(&mut waits, 0.95),
        "ms",
    );
    let _ = cx.update_window(any, |_, _, cx| {
        if let Some(ws) = ws.upgrade() {
            ws.update(cx, |w, cx| {
                w.panel = None;
                cx.notify();
            });
        }
    });
}

/// What the frame split cannot show, said in every results file.
const NOT_MEASURED: &str = "GPU submission and present: GPUI times them only with its `profiler` feature, which this build does not enable; each frame's time ends when its last element has painted";

/// The command each busy pane runs: a line every 20 ms, or a corpus the
/// harness wrote (`DREAMER_BENCH_LOAD`) replayed nonstop with `cat`.
fn load_command(load: &serde_json::Value) -> String {
    match load["file"].as_str() {
        Some(file) => format!("while :; do cat '{file}'; done"),
        None => "i=0; while :; do i=$((i+1)); echo \"line $i the quick brown fox jumps over the lazy dog\"; sleep 0.02; done".into(),
    }
}

/// One window frame: when its render began, and from then until its last
/// element painted, all of it (`build`) and the terminal paint, shaping,
/// and image conversion within it, in nanoseconds.
#[derive(Debug, PartialEq)]
struct FrameSplit {
    at: Instant,
    build: u64,
    paint: u64,
    shape: u64,
    image: u64,
}

/// Each frame whose render start and paint end were both logged.
fn split_frames(marks: &[FrameMark]) -> Vec<FrameSplit> {
    marks
        .windows(2)
        .filter(|w| !w[0].end && w[1].end)
        .map(|w| FrameSplit {
            at: w[0].at,
            build: (w[1].at - w[0].at).as_nanos() as u64,
            paint: w[1].paint - w[0].paint,
            shape: w[1].shape - w[0].shape,
            image: w[1].image - w[0].image,
        })
        .collect()
}

/// The frame split as results. Per frame, from the workspace's render to
/// its last element painted:
/// - **frame build:** all of it (render, layout, prepaint, paint);
/// - **terminal paint:** painting terminal panes, including
/// - **shaping:** rows the shape cache did not hold, and
/// - **image conversion:** new or changed images made GPU-ready;
/// - **other frame work:** the build less terminal paint (layout, chrome,
///   building the element tree).
fn frame_results(frames: &[FrameSplit], put: &mut impl FnMut(&str, f64, &str)) {
    let ns = |f: fn(&FrameSplit) -> u64| -> Vec<f64> {
        frames.iter().map(|x| f(x) as f64 / 1e6).collect()
    };
    type Percentiles = &'static [(&'static str, f64)];
    const ALL: Percentiles = &[("p50", 0.5), ("p95", 0.95), ("max", 1.0)];
    const MOST: Percentiles = &[("p50", 0.5), ("p95", 0.95)];
    let parts: [(&str, Vec<f64>, Percentiles); 5] = [
        ("frame build per frame", ns(|f| f.build), ALL),
        ("terminal paint time per frame", ns(|f| f.paint), ALL),
        ("shaping per frame", ns(|f| f.shape), MOST),
        ("image conversion per frame", ns(|f| f.image), MOST),
        (
            "other frame work per frame",
            ns(|f| f.build.saturating_sub(f.paint)),
            MOST,
        ),
    ];
    for (what, mut v, ps) in parts {
        for (name, p) in ps {
            put(&format!("{what}, {name}"), pct(&mut v, *p), "ms");
        }
    }
    let mut gaps: Vec<f64> = frames.windows(2).map(|w| ms(w[1].at - w[0].at)).collect();
    put("time between frames, p50", pct(&mut gaps, 0.5), "ms");
    put("time between frames, p95", pct(&mut gaps, 0.95), "ms");
}

pub(crate) async fn run(
    any: AnyWindowHandle,
    ws: WeakEntity<Workspace>,
    daemon: Vec<String>,
    out: PathBuf,
    cx: &mut AsyncApp,
) {
    let started = *crate::STARTED.get_or_init(Instant::now);
    let _ = OUT.set(out);
    // Launch's frames, from the first render this run sees.
    if let Ok(mut log) = FRAME_LOG.lock() {
        *log = Some(Vec::new());
    }
    let mut results = Vec::new();
    let mut put = |measure: &str, value: f64, unit: &str| {
        println!("bench: {measure}: {value:.2} {unit}");
        results.push(json!({ "measure": measure, "value": value, "unit": unit }));
    };

    // 1. Launch to every visible pane painted.
    // Where launch goes: the window's view running, the topology, the first
    // pane's first frame with text.
    put(
        "launch: the window's first update",
        ms(Instant::now() - started),
        "ms",
    );
    let topo = until(
        any,
        &ws,
        cx,
        "the topology",
        Duration::from_secs(30),
        |w, _| !w.pane_ids().is_empty(),
    )
    .await;
    put("launch: the topology shown", ms(topo - started), "ms");
    let one = until(
        any,
        &ws,
        cx,
        "a pane's first frame",
        Duration::from_secs(30),
        |w, cx| {
            w.pane_ids().iter().any(|id| {
                w.pane_shell(*id)
                    .is_some_and(|s| !s.read(cx).frame.text_dump().trim().is_empty())
            })
        },
    )
    .await;
    put("launch: a pane's first frame", ms(one - started), "ms");
    let content = until(
        any,
        &ws,
        cx,
        "every visible pane's content",
        Duration::from_secs(30),
        |w, cx| all_show(w, cx, "READY"),
    )
    .await;
    put(
        "launch: every visible pane's content received (not yet painted)",
        ms(content - started),
        "ms",
    );
    let first = until(
        any,
        &ws,
        cx,
        "every visible pane painted ready",
        Duration::from_secs(30),
        |w, cx| {
            all_show(w, cx, "READY")
                && w.pane_ids().iter().all(|id| {
                    w.pane_shell(*id)
                        .is_some_and(|s| terminal_render::was_painted(&s.read(cx).frame))
                })
        },
    )
    .await;
    let launch_frames = split_frames(
        &FRAME_LOG
            .lock()
            .ok()
            .and_then(|mut l| l.take())
            .unwrap_or_default(),
    );
    let visible = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
    put("visible panes", visible.len() as f64, "panes");
    // Where launch went: how many frames were drawn before every visible
    // pane was painted, and the work in them.
    let total = |f: fn(&FrameSplit) -> u64| -> f64 {
        launch_frames.iter().map(f).sum::<u64>() as f64 / 1e6
    };
    put("launch: frames drawn", launch_frames.len() as f64, "frames");
    put("launch: frame build, total", total(|f| f.build), "ms");
    put("launch: terminal paint, total", total(|f| f.paint), "ms");
    put("launch: shaping, total", total(|f| f.shape), "ms");
    put(
        "launch: slowest frame build",
        launch_frames.iter().map(|f| f.build).max().unwrap_or(0) as f64 / 1e6,
        "ms",
    );
    // A first frame can be drawn on a locked screen; what follows needs a
    // window that keeps drawing.
    if !drawing(any, cx).await {
        not_run(NOT_DRAWING);
    }
    put(
        "launch to every visible pane painted (daemon running)",
        ms(first - started),
        "ms",
    );
    let _ = cx.update_window(any, |_, window, _| window.activate_window());
    sleep(cx, Duration::from_millis(500)).await;
    note_focus(any, cx);

    // 2. Idle.
    sleep(cx, Duration::from_secs(1)).await;
    let before = terminal_render::paint_count();
    sleep(cx, Duration::from_secs(3)).await;
    let idle = terminal_render::paint_count() - before;
    // No paints proves nothing from a window that is not drawing.
    if idle == 0 && !drawing(any, cx).await {
        not_run(&format!("idle: {NOT_DRAWING}"));
    }
    put("terminal paints over 3 s idle", idle as f64, "paints");
    note_focus(any, cx);

    // 3. Key to echo, idle: the first visible pane.
    let typing = *visible.first().unwrap_or_else(|| fail("no pane"));
    let mut echo = key_to_echo(any, &ws, typing, 100, cx).await;
    put(
        "key to echo incl. paint, idle, p50",
        pct(&mut echo, 0.5),
        "ms",
    );
    put(
        "key to echo incl. paint, idle, p95",
        pct(&mut echo, 0.95),
        "ms",
    );
    note_focus(any, cx);

    // 4. Load on every other visible pane.
    let load = load_command(&given("DREAMER_BENCH_LOAD"));
    let busy: Vec<u32> = visible.iter().copied().filter(|p| *p != typing).collect();
    for p in &busy {
        tmux(
            &daemon,
            &["send-keys", "-t", &format!("%{p}"), &load, "Enter"],
        );
    }
    sleep(cx, Duration::from_secs(2)).await;
    if let Ok(mut log) = FRAME_LOG.lock() {
        *log = Some(Vec::new());
    }
    let fast_before = terminal_render::fast_runs();
    sleep(cx, Duration::from_secs(5)).await;
    let fast_runs = terminal_render::fast_runs() - fast_before;
    let marks = FRAME_LOG
        .lock()
        .ok()
        .and_then(|mut l| l.take())
        .unwrap_or_default();
    let frames = split_frames(&marks);
    if frames.len() < 2 && !busy.is_empty() {
        if !drawing(any, cx).await {
            not_run(&format!("frames under load: {NOT_DRAWING}"));
        }
        fail("busy panes drew fewer than two frames in 5 s");
    }
    put(
        &format!("frames in 5 s with {} busy panes", busy.len()),
        frames.len() as f64,
        "frames",
    );
    note_focus(any, cx);
    frame_results(&frames, &mut put);
    put(
        "runs drawn without shaping (ASCII glyphs placed), 5 s under load",
        fast_runs as f64,
        "runs",
    );

    // 5. Key to echo under that load.
    let mut echo = key_to_echo(any, &ws, typing, 100, cx).await;
    put(
        &format!("key to echo incl. paint, {} busy panes, p50", busy.len()),
        pct(&mut echo, 0.5),
        "ms",
    );
    put(
        &format!("key to echo incl. paint, {} busy panes, p95", busy.len()),
        pct(&mut echo, 0.95),
        "ms",
    );
    note_focus(any, cx);
    for p in &busy {
        tmux(&daemon, &["send-keys", "-t", &format!("%{p}"), "C-c", ""]);
    }
    sleep(cx, Duration::from_millis(500)).await;

    // 6. Tab switch.
    let (tabs, active) = read(any, &ws, cx, |w, _| w.tabs()).unwrap_or_default();
    if tabs.len() >= 2 {
        let home = active.unwrap_or(tabs[0]);
        let other = *tabs.iter().find(|t| **t != home).unwrap_or(&tabs[1]);
        let mut switches = Vec::new();
        for i in 0..20 {
            let target = if i % 2 == 0 { other } else { home };
            let from: Vec<u32> = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
            // Frames shown before the switch do not count as painted by it.
            cx.update(|_| terminal_render::forget_painted());
            let t0 = Instant::now();
            let _ = cx.update_window(any, |_, window, cx| {
                if let Some(ws) = ws.upgrade() {
                    ws.update(cx, |w, cx| w.click_tab_for_smoke(target, window, cx));
                }
            });
            until(
                any,
                &ws,
                cx,
                "the other tab's panes",
                Duration::from_secs(10),
                |w, cx| {
                    w.tabs().1 == Some(target)
                        && w.pane_ids() != from
                        && all_show(w, cx, "READY")
                        && w.pane_ids().iter().all(|id| {
                            w.pane_shell(*id)
                                .is_some_and(|s| terminal_render::was_painted(&s.read(cx).frame))
                        })
                },
            )
            .await;
            switches.push(ms(t0.elapsed()));
            sleep(cx, Duration::from_millis(200)).await;
        }
        put(
            "tab switch to every pane painted, p50",
            pct(&mut switches, 0.5),
            "ms",
        );
        put(
            "tab switch to every pane painted, p95",
            pct(&mut switches, 0.95),
            "ms",
        );
    }

    note_focus(any, cx);

    // 7. A streaming transcript.
    transcript(any, &ws, cx, &mut put).await;
    note_focus(any, cx);

    write(&report(results, None));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(at: Instant, ms_in: u64, end: bool, paint: u64, shape: u64, image: u64) -> FrameMark {
        FrameMark {
            at: at + Duration::from_millis(ms_in),
            end,
            paint,
            shape,
            image,
        }
    }

    #[test]
    fn a_frame_is_its_render_start_to_its_last_paint() {
        let t = Instant::now();
        let marks = [
            // A paint end logged before any start (the log began mid-frame).
            mark(t, 0, true, 5, 1, 0),
            mark(t, 10, false, 5, 1, 0),
            mark(t, 13, true, 2_000_005, 500_001, 0),
            // A render whose frame never painted, then a full one.
            mark(t, 20, false, 2_000_005, 500_001, 0),
            mark(t, 30, false, 2_000_005, 500_001, 0),
            mark(t, 34, true, 3_000_005, 500_001, 700_000),
        ];
        let frames = split_frames(&marks);
        assert_eq!(
            frames,
            vec![
                FrameSplit {
                    at: t + Duration::from_millis(10),
                    build: 3_000_000,
                    paint: 2_000_000,
                    shape: 500_000,
                    image: 0,
                },
                FrameSplit {
                    at: t + Duration::from_millis(30),
                    build: 4_000_000,
                    paint: 1_000_000,
                    shape: 0,
                    image: 700_000,
                },
            ]
        );
        let mut got = Vec::new();
        frame_results(&frames, &mut |m: &str, v: f64, _: &str| {
            got.push((m.to_owned(), v))
        });
        let get = |m: &str| got.iter().find(|(n, _)| n == m).map(|(_, v)| *v);
        assert_eq!(get("frame build per frame, max"), Some(4.0));
        assert_eq!(get("other frame work per frame, p95"), Some(3.0));
        assert_eq!(get("time between frames, p50"), Some(20.0));
    }

    #[test]
    fn a_corpus_load_replays_its_file_and_no_load_paces_lines() {
        assert_eq!(
            load_command(&json!({ "name": "escape", "corpus": "sgr", "file": "/tmp/x/load.bin" })),
            "while :; do cat '/tmp/x/load.bin'; done"
        );
        assert!(load_command(&serde_json::Value::Null).contains("sleep 0.02"));
    }

    #[test]
    fn a_run_that_could_not_measure_says_so_and_holds_no_results() {
        let r = report(vec![json!({ "measure": "m", "value": 1.0 })], Some("why"));
        assert_eq!(r["not_run"], json!(true));
        assert_eq!(r["reason"], json!("why"));
        assert_eq!(r["results"], json!([]));
        assert_eq!(report(Vec::new(), None)["not_run"], serde_json::Value::Null);
    }
}
