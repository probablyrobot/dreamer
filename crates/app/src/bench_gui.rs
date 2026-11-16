//! The GUI benchmark (spec 16.5, 16.6), driven from inside the app so each
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
//!    every 20 ms; for 5 s, each window frame's terminal paint time and the
//!    time between frames.
//! 5. **Key to echo under that load.**
//! 6. **Tab switch:** selecting the other tab until each of its panes has
//!    painted its content.
//!
//! The results are written to the file as JSON, and the process exits.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, WeakEntity};
use serde_json::json;

use crate::workspace::{FRAME_LOG, Workspace};

fn fail(why: &str) -> ! {
    eprintln!("bench: {why}");
    std::process::exit(3)
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
            fail(&format!("never saw: {what}"));
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
            Duration::from_secs(5),
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

pub(crate) async fn run(
    any: AnyWindowHandle,
    ws: WeakEntity<Workspace>,
    daemon: Vec<String>,
    out: PathBuf,
    cx: &mut AsyncApp,
) {
    let started = *crate::STARTED.get_or_init(Instant::now);
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
    let visible = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
    put("visible panes", visible.len() as f64, "panes");
    put(
        "launch to every visible pane painted (daemon running)",
        ms(first - started),
        "ms",
    );

    // 2. Idle.
    sleep(cx, Duration::from_secs(1)).await;
    let before = terminal_render::paint_count();
    sleep(cx, Duration::from_secs(3)).await;
    put(
        "terminal paints over 3 s idle",
        (terminal_render::paint_count() - before) as f64,
        "paints",
    );

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

    // 4. Load on every other visible pane.
    let busy: Vec<u32> = visible.iter().copied().filter(|p| *p != typing).collect();
    for p in &busy {
        tmux(
            &daemon,
            &[
                "send-keys",
                "-t",
                &format!("%{p}"),
                "i=0; while :; do i=$((i+1)); echo \"line $i the quick brown fox jumps over the lazy dog\"; sleep 0.02; done",
                "Enter",
            ],
        );
    }
    sleep(cx, Duration::from_secs(2)).await;
    if let Ok(mut log) = FRAME_LOG.lock() {
        *log = Some(Vec::new());
    }
    sleep(cx, Duration::from_secs(5)).await;
    let frames = FRAME_LOG
        .lock()
        .ok()
        .and_then(|mut l| l.take())
        .unwrap_or_default();
    let mut work: Vec<f64> = frames
        .windows(2)
        .map(|w| (w[1].1 - w[0].1) as f64 / 1e6)
        .collect();
    let mut gaps: Vec<f64> = frames.windows(2).map(|w| ms(w[1].0 - w[0].0)).collect();
    put(
        &format!("frames in 5 s with {} busy panes", busy.len()),
        frames.len() as f64,
        "frames",
    );
    put(
        "terminal paint time per frame, p50",
        pct(&mut work, 0.5),
        "ms",
    );
    put(
        "terminal paint time per frame, p95",
        pct(&mut work, 0.95),
        "ms",
    );
    put(
        "terminal paint time per frame, max",
        pct(&mut work, 1.0),
        "ms",
    );
    put("time between frames, p50", pct(&mut gaps, 0.5), "ms");
    put("time between frames, p95", pct(&mut gaps, 0.95), "ms");

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
            let t0 = Instant::now();
            let _ = cx.update_window(any, |_, _, cx| {
                if let Some(ws) = ws.upgrade() {
                    ws.update(cx, |w, cx| w.click_tab_for_smoke(target, cx));
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

    let report = json!({ "workload": "gui", "results": results });
    if let Err(e) = std::fs::write(
        &out,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    ) {
        fail(&format!("{}: {e}", out.display()));
    }
    std::process::exit(0);
}
