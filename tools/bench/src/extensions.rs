//! `dreamer-bench extensions`: typing and cancelling in a quiet pane while
//! slow, stalled, and crashing extensions run against busy panes (spec
//! 16.3, 16.5), and what opening a monitor costs.
//!
//! The extensions are the harness's own stand-ins, never anything from the
//! user's configuration:
//!
//! - **slow filter:** a filter on the busy panes records each match as an
//!   event, and a Luau script spends 45 ms of its 50 ms running-time budget
//!   on every one;
//! - **overrunning filter:** a second script runs past its budget on every
//!   match, so the daemon stops it each time; when it is parked, the
//!   harness reloads it;
//! - **stalled raw tap:** a client follows every busy pane at the `raw`
//!   level and never reads (the gap path);
//! - **stalled extension:** a supervised extension follows every pane at
//!   the `raw` level and the event log, then stops reading its input and
//!   answering heartbeats, until the daemon kills it; it stalls again each
//!   time it is restarted;
//! - **crashing extension:** a supervised extension that exits with an
//!   error right after its hello, restarted as soon as it is parked;
//! - **monitor:** a supervised extension that does what a monitor pane
//!   does: follows every pane at the `text` level and the event log, and
//!   reads the topology once a second.
//!
//! The supervised stand-ins are this program (`--stand-in ROLE GO`). Each
//! starts dormant and does its part while the file `GO` exists, so one
//! daemon serves every phase:
//!
//! 1. **quiet:** one quiet pane, the monitor dormant;
//! 2. **quiet, monitor open;**
//! 3. **busy:** `N` panes (63 by default) run `yes`, the monitor closed;
//! 4. **busy, monitor open;**
//! 5. **busy, every slow extension:** the monitor and all of the above.
//!
//! Each phase measures, over `--seconds` (10 by default), the daemon's CPU,
//! resident size, heap in use, and output ingested, and checks once a
//! second that every busy pane's mirror sequence advanced: the canonical
//! reader never stalls. Then key to echo in a fresh `cat` pane (200
//! keystrokes) and Ctrl-C cancellation in a quiet shell (20), both through
//! a client replica as in `headless`. A phase's added daemon cost over the
//! one before is the extension overhead, reported apart from the terminal
//! cost of the busy panes.
//!
//! ```text
//! dreamer-bench extensions [--panes N] [--seconds S] [--samples K]
//!     [--without monitor|filter|scripts|raw-tap|stalled|crasher]... [--json FILE]
//! ```
//!
//! `--without` leaves one stand-in out (`filter` takes the scripts with
//! it), to find which one costs what.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use serde_json::{Value, json};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

use super::{
    Rig, cpu_seconds, hardware, heap_kib, key_to_echo, mirrors, ms, next_frame, percentile,
    print_table, revision, rss_kib, wait_live, write_json,
};

/// The one-pane key-to-echo budget (spec 16.5), plus the 10 ms allowed
/// under load.
const BUDGET_MS: f64 = 30.0 + 10.0;

/// Numbered lines, flat out. Not `yes`: a `text` tap compares lines by
/// content, and a run of identical lines shows it no new lines
/// (docs/taps.md), so filters and the monitor would have nothing to do.
const BUSY: &str =
    "awk 'BEGIN { for (i = 0; ; i++) print \"busy output for the extension benchmark, line \" i }'";

/// Spends 45 ms of the script's 50 ms budget on each match.
const SLOW_SCRIPT: &str = r"
function on_event(e)
    local t = os.clock()
    while os.clock() - t < 0.045 do end
end
";

/// Runs past its budget on each match: the daemon stops it every time.
const OVERRUN_SCRIPT: &str = r"
function on_event(e)
    while true do end
end
";

/// Ctrl-C in a quiet shell: each one clears the screen and prints a count.
const CANCEL_SCRIPT: &str = r#"n=0
trap 'n=$((n+1)); printf "\033[H\033[2Jcancelled %d\n" "$n"' INT
printf 'ready\n'
while :; do sleep 1000; done
"#;

/// What `--without` can leave out, to find which extension costs what.
const PARTS: [&str; 6] = [
    "monitor", "filter", "scripts", "raw-tap", "stalled", "crasher",
];

struct Options {
    panes: usize,
    seconds: u64,
    samples: usize,
    without: Vec<String>,
}

impl Options {
    fn with(&self, part: &str) -> bool {
        !self.without.iter().any(|w| w == part)
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) == Some("--stand-in") {
        return stand_in(&args[1..]);
    }
    let mut o = Options {
        panes: 63,
        seconds: 10,
        samples: 200,
        without: Vec::new(),
    };
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    let number = |v: Option<&String>, what: &str| -> Result<u64, String> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{what} needs a number"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--panes" => o.panes = number(it.next(), "--panes")? as usize,
            "--seconds" => o.seconds = number(it.next(), "--seconds")?.max(2),
            "--samples" => o.samples = number(it.next(), "--samples")?.max(1) as usize,
            "--without" => {
                let w = it.next().ok_or("--without needs a name")?;
                if !PARTS.contains(&w.as_str()) {
                    return Err(format!("--without takes one of {}", PARTS.join(", ")));
                }
                o.without.push(w.clone());
            }
            "--json" => json_out = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (results, checks) = rt.block_on(workload(&o))?;
    let report = json!({
        "workload": "extensions",
        "panes": o.panes,
        "seconds": o.seconds,
        "without": o.without,
        "hardware": hardware(),
        "revision": revision(),
        "results": results,
        "checks": checks,
    });
    print_table(&report);
    for c in &checks {
        println!(
            "check: {}: {}",
            c["check"].as_str().unwrap_or(""),
            if c["met"].as_bool() == Some(true) {
                "met"
            } else {
                "NOT met"
            }
        );
    }
    if let Some(p) = json_out {
        write_json(&p, &report)?;
    }
    Ok(())
}

/// A rig like [`Rig::start`], with the stand-in extensions configured and
/// a 100 ms first restart delay, so the crashing one keeps crashing.
fn rig() -> Result<Rig, String> {
    let root = PathBuf::from(format!("/tmp/drm-bext-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).map_err(|e| e.to_string())?;
    let tmux = TmuxServer::start(120, 40, "/bin/sh").map_err(|e| format!("tmux: {e}"))?;
    tmux.run_checked(&["set-option", "-g", "history-limit", "2000"])
        .map_err(|e| e.to_string())?;
    std::fs::write(
        root.join("config/dreamer.yaml"),
        "tiers:\n  cold_after: never\n  warm_limit: none\n",
    )
    .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // JSON strings are YAML strings: no path needs escaping by hand.
    let q = |p: &Path| serde_json::to_string(&p.display().to_string()).unwrap_or_default();
    let ext = |name: &str, role: &str, go: &str, methods: &str| {
        format!(
            "  - name: {name}\n    argv: [{}, extensions, --stand-in, {role}, {}]\n    grant:\n      methods: [{methods}]\n      all_panes: true\n",
            q(&exe),
            q(&root.join(go)),
        )
    };
    let yaml = format!(
        "extensions:\n{}{}{}",
        ext(
            "monitor",
            "monitor",
            "go-monitor",
            "taps.subscribe, taps.unsubscribe, events.subscribe, events.unsubscribe, tmux.topology"
        ),
        ext(
            "stalled",
            "stall",
            "go-stall",
            "taps.subscribe, events.subscribe"
        ),
        ext("crasher", "crash", "go-crash", "events.subscribe"),
    );
    std::fs::write(root.join("config/extensions.yaml"), yaml).map_err(|e| e.to_string())?;
    let daemon = std::env::var_os("DREAMER_BENCH_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe.with_file_name("dreamerd"));
    if !daemon.is_file() {
        return Err(format!(
            "{} is missing; run `cargo xtask bench`",
            daemon.display()
        ));
    }
    let child = Command::new(&daemon)
        .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
        .arg(tmux_program())
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .env("DREAMER_TEST_EXTENSION_BACKOFF_MS", "100")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", daemon.display()))?;
    Ok(Rig {
        root,
        tmux,
        daemon: child,
    })
}

async fn extensions(c: &Client) -> Result<Vec<api::ExtensionStatus>, String> {
    match c.call(Request::ExtensionsList(api::Empty {})).await {
        Ok(Response::ExtensionsList(l)) => match l.config_error {
            Some(e) => Err(format!("extensions.yaml: {e}")),
            None => Ok(l.extensions),
        },
        Ok(_) => Err("unexpected reply to extensions.list".into()),
        Err(e) => Err(format!("extensions.list: {e}")),
    }
}

async fn scripts(c: &Client) -> Result<Vec<api::ScriptStatus>, String> {
    match c.call(Request::ScriptsList(api::Empty {})).await {
        Ok(Response::ScriptsList(l)) => Ok(l.scripts),
        Ok(_) => Err("unexpected reply to scripts.list".into()),
        Err(e) => Err(format!("scripts.list: {e}")),
    }
}

async fn filters(c: &Client) -> Result<Vec<api::FilterStatus>, String> {
    match c.call(Request::FiltersList(api::Empty {})).await {
        Ok(Response::FiltersList(l)) => Ok(l.filters),
        Ok(_) => Err("unexpected reply to filters.list".into()),
        Err(e) => Err(format!("filters.list: {e}")),
    }
}

/// The monitor stand-in's pid, while it runs.
async fn monitor_pid(c: &Client) -> Option<u32> {
    extensions(c)
        .await
        .ok()?
        .into_iter()
        .find(|x| x.name == "monitor" && x.state == api::ExtensionState::Running as i32)?
        .pid
}

/// A quiet shell that Ctrl-C interrupts, followed by a replica.
struct Cancel {
    feed: replica::feed::Feed,
    count: u64,
}

impl Cancel {
    async fn start(rig: &Rig, c: &Client) -> Result<Cancel, String> {
        let script = rig.root.join("cancel.sh");
        std::fs::write(&script, CANCEL_SCRIPT).map_err(|e| e.to_string())?;
        let before: Vec<u32> = mirrors(c).await?.iter().map(|m| m.pane).collect();
        rig.windows(1, &format!("/bin/sh {}", script.display()))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let pane = loop {
            let now = mirrors(c).await?;
            if let Some(m) = now
                .iter()
                .find(|m| !before.contains(&m.pane) && m.state == api::MirrorState::Live as i32)
            {
                break m.pane;
            }
            if Instant::now() > deadline {
                return Err("the cancel pane never became live".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let feed = replica::feed::Feed::start(rig.socket(), Some(pane), None)
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let f = next_frame(&feed, deadline)
                .await
                .ok_or("the cancel pane never showed its prompt")?;
            if f.text_dump().contains("ready") {
                break;
            }
        }
        Ok(Cancel { feed, count: 0 })
    }

    /// Ctrl-C, each timed until the shell's trap has redrawn the pane.
    async fn measure(&mut self, samples: usize) -> Result<Vec<f64>, String> {
        let mut out = Vec::with_capacity(samples);
        for _ in 0..samples {
            // Let `sleep` start again after the last one.
            tokio::time::sleep(Duration::from_millis(50)).await;
            while self.feed.take().is_some() {}
            self.count += 1;
            let want = format!("cancelled {}\n", self.count);
            let sent = Instant::now();
            self.feed
                .send(api::terminal_input::Event::Key(api::KeyInput {
                    key: "KeyC".into(),
                    text: Some("c".into()),
                    mods: 2,
                    held: false,
                }));
            let deadline = sent + Duration::from_secs(5);
            loop {
                let Some(f) = next_frame(&self.feed, deadline).await else {
                    return Err(format!("Ctrl-C {} was never handled", self.count));
                };
                if f.text_dump().lines().any(|l| format!("{l}\n") == want) {
                    out.push(ms(sent.elapsed()));
                    break;
                }
            }
        }
        out.sort_by(f64::total_cmp);
        Ok(out)
    }
}

/// What one phase measured.
#[derive(Default)]
struct Phase {
    cpu: f64,
    rss_mib: f64,
    heap_mib: f64,
    ingest_mib_s: f64,
    stuck: usize,
    unmoved: usize,
    monitor_cpu: Option<f64>,
    monitor_rss_mib: Option<f64>,
}

struct Rows(Vec<Value>);

impl Rows {
    fn put(&mut self, measure: &str, value: f64, unit: &str) {
        eprintln!("bench: {measure}: {value:.2} {unit}");
        self.0
            .push(json!({ "measure": measure, "value": value, "unit": unit }));
    }
}

/// Phase measures; key to echo and cancellation are put by the caller.
async fn phase(
    c: &Client,
    pid: u32,
    busy: &[u32],
    seconds: u64,
    rows: &mut Rows,
    tag: &str,
) -> Result<Phase, String> {
    let seq = |l: &[api::MirrorStatus]| -> Vec<(u64, u64)> {
        busy.iter()
            .map(|p| {
                l.iter()
                    .find(|m| m.pane == *p)
                    .map_or((0, 0), |m| (m.sequence, m.ingested_bytes))
            })
            .collect()
    };
    let monitor = monitor_pid(c).await;
    let mcpu0 = monitor.and_then(|p| cpu_seconds(p).ok());
    let first = seq(&mirrors(c).await?);
    let mut last = first.clone();
    let mut stuck = 0usize;
    let cpu0 = cpu_seconds(pid)?;
    let t0 = Instant::now();
    let mut next = t0;
    for _ in 0..seconds {
        next += Duration::from_secs(1);
        tokio::time::sleep_until(next.into()).await;
        let now = seq(&mirrors(c).await?);
        stuck += now.iter().zip(&last).filter(|(a, b)| a.0 <= b.0).count();
        last = now;
    }
    let secs = t0.elapsed().as_secs_f64();
    let cpu = (cpu_seconds(pid)? - cpu0) / secs * 100.0;
    let ingested: u64 = last
        .iter()
        .zip(&first)
        .map(|(a, b)| a.1.saturating_sub(b.1))
        .sum();
    let unmoved = last.iter().zip(&first).filter(|(a, b)| a.0 <= b.0).count();
    let mut p = Phase {
        cpu,
        rss_mib: rss_kib(pid)? as f64 / 1024.0,
        heap_mib: heap_kib(c).await? / 1024.0,
        ingest_mib_s: ingested as f64 / 1024.0 / 1024.0 / secs,
        stuck,
        unmoved,
        ..Phase::default()
    };
    if let (Some(m), Some(a)) = (monitor, mcpu0)
        && let Ok(b) = cpu_seconds(m)
    {
        p.monitor_cpu = Some((b - a) / secs * 100.0);
        p.monitor_rss_mib = rss_kib(m).ok().map(|k| k as f64 / 1024.0);
    }
    rows.put(&format!("daemon CPU, {tag}"), p.cpu, "% of a core");
    rows.put(&format!("daemon RSS, {tag}"), p.rss_mib, "MiB");
    rows.put(&format!("daemon heap in use, {tag}"), p.heap_mib, "MiB");
    if !busy.is_empty() {
        rows.put(
            &format!("output ingested from busy panes, {tag}"),
            p.ingest_mib_s,
            "MiB/s",
        );
        rows.put(
            &format!(
                "busy pane-seconds without mirror progress, {tag}, of {}",
                busy.len() * seconds as usize
            ),
            p.stuck as f64,
            "",
        );
    }
    Ok(p)
}

/// Key to echo and Ctrl-C, put under `tag`; the key-to-echo p95.
async fn typing(
    rig: &Rig,
    c: &Client,
    cancel: &mut Cancel,
    samples: usize,
    rows: &mut Rows,
    tag: &str,
) -> Result<f64, String> {
    let echo = key_to_echo(rig, c, samples).await?;
    rows.put(
        &format!("key to echo, {tag}, p50"),
        percentile(&echo, 0.5),
        "ms",
    );
    let p95 = percentile(&echo, 0.95);
    rows.put(&format!("key to echo, {tag}, p95"), p95, "ms");
    let cancelled = cancel.measure((samples / 10).max(5)).await?;
    rows.put(
        &format!("Ctrl-C to the shell's redraw, {tag}, p50"),
        percentile(&cancelled, 0.5),
        "ms",
    );
    rows.put(
        &format!("Ctrl-C to the shell's redraw, {tag}, p95"),
        percentile(&cancelled, 0.95),
        "ms",
    );
    Ok(p95)
}

fn touch(p: &Path) -> Result<(), String> {
    std::fs::write(p, b"").map_err(|e| format!("{}: {e}", p.display()))
}

/// Wait until `f` holds for the extension list, or fail with `what`.
async fn wait_extensions(
    c: &Client,
    what: &str,
    f: impl Fn(&[api::ExtensionStatus]) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let l = extensions(c).await?;
        if f(&l) {
            return Ok(());
        }
        if Instant::now() > deadline {
            let states: Vec<String> = l
                .iter()
                .map(|x| {
                    format!(
                        "{} {} {}",
                        x.name,
                        x.state,
                        x.last_error.as_deref().unwrap_or("")
                    )
                })
                .collect();
            return Err(format!("{what}: {states:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The stalled raw tap: subscribes while still reading (the reply comes on
/// the same stream), then stops reading for good.
async fn stalled_tap(
    rig: &Rig,
    busy: &[u32],
) -> Result<(Client, tokio::sync::mpsc::Receiver<Pushed>), String> {
    let mut v = Client::connect_with(&rig.socket(), "stalled-tap", 1)
        .await
        .map_err(|e| e.to_string())?;
    let mut rx = v.pushed().ok_or("no pushed stream")?;
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let reader = tokio::spawn(async move {
        while !s.load(Ordering::Relaxed) {
            if let Ok(None) = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                break;
            }
        }
        rx
    });
    let r = v
        .call(Request::TapsSubscribe(api::TapSubscribe {
            level: api::TapLevel::Raw as i32,
            panes: busy.to_vec(),
            ..api::TapSubscribe::default()
        }))
        .await;
    stop.store(true, Ordering::Relaxed);
    let rx = reader.await.map_err(|e| e.to_string())?;
    match r {
        Ok(Response::TapsSubscribe(_)) => Ok((v, rx)),
        Ok(_) => Err("unexpected reply to taps.subscribe".into()),
        Err(e) => Err(format!("taps.subscribe (raw): {e}")),
    }
}

/// Read the stalled tap again for a few seconds: the gaps it was told
/// about, and whether its connection survived.
async fn drain_tap(rx: &mut tokio::sync::mpsc::Receiver<Pushed>) -> (usize, bool) {
    let mut gaps = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) => {
                if let Ok(api::Notification::TapsData(d)) =
                    api::Notification::decode(&topic, &payload)
                    && d.gap.is_some()
                {
                    gaps += 1;
                }
            }
            Ok(Some(Pushed::Closed { .. }) | None) => return (gaps, false),
            Ok(Some(_)) => {}
            Err(_) => return (gaps, true),
        }
    }
}

/// Restarts the crashing extension and reloads the overrunning script
/// whenever they are parked, so both keep failing for the whole phase.
fn keeper(rig: &Rig, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<(u64, u64)> {
    let socket = rig.socket();
    tokio::spawn(async move {
        let Ok(c) = Client::connect(&socket, "bench-keeper").await else {
            return (0, 0);
        };
        let (mut restarted, mut reloaded) = (0, 0);
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Ok(l) = extensions(&c).await
                && l.iter()
                    .any(|x| x.name == "crasher" && x.state == api::ExtensionState::Failed as i32)
                && c.call(Request::ExtensionsRestart(api::ExtensionRef {
                    name: "crasher".into(),
                }))
                .await
                .is_ok()
            {
                restarted += 1;
            }
            if let Ok(l) = scripts(&c).await
                && l.iter()
                    .any(|s| s.name == "overrun" && s.state == api::ScriptState::Failed as i32)
                && c.call(Request::ScriptsReload(api::ScriptSource {
                    name: "overrun".into(),
                    source: OVERRUN_SCRIPT.into(),
                }))
                .await
                .is_ok()
            {
                reloaded += 1;
            }
        }
        (restarted, reloaded)
    })
}

async fn workload(o: &Options) -> Result<(Vec<Value>, Vec<Value>), String> {
    let mut rows = Rows(Vec::new());
    let rig = rig()?;
    let c = rig.client().await?;
    wait_live(&c, 1, Duration::from_secs(15)).await?;
    wait_extensions(&c, "the stand-in extensions never all ran", |l| {
        l.len() == 3
            && l.iter()
                .all(|x| x.state == api::ExtensionState::Running as i32)
    })
    .await?;
    let pid = rig.pid()?;
    let mut cancel = Cancel::start(&rig, &c).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let go_monitor = rig.root.join("go-monitor");
    let s = o.seconds;

    // 1. Quiet: the one-pane reference.
    let quiet = phase(&c, pid, &[], s, &mut rows, "quiet").await?;
    let quiet_p95 = typing(&rig, &c, &mut cancel, o.samples, &mut rows, "quiet").await?;

    // 2. Quiet, the monitor open.
    let mut quiet_mon = None;
    if o.with("monitor") {
        touch(&go_monitor)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let tag = "quiet, monitor open";
        quiet_mon = Some(phase(&c, pid, &[], s, &mut rows, tag).await?);
        typing(&rig, &c, &mut cancel, o.samples, &mut rows, tag).await?;
        std::fs::remove_file(&go_monitor).map_err(|e| e.to_string())?;
    }

    // 3. Busy panes.
    let first: Vec<u32> = mirrors(&c).await?.iter().map(|m| m.pane).collect();
    rig.windows(o.panes, BUSY)?;
    wait_live(&c, first.len() + o.panes, Duration::from_secs(120)).await?;
    let busy: Vec<u32> = mirrors(&c)
        .await?
        .iter()
        .map(|m| m.pane)
        .filter(|p| !first.contains(p))
        .collect();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let tag = format!("{} busy panes", busy.len());
    let loaded = phase(&c, pid, &busy, s, &mut rows, &tag).await?;
    typing(&rig, &c, &mut cancel, o.samples, &mut rows, &tag).await?;

    // 4. Busy, the monitor open.
    let mut loaded_mon = None;
    if o.with("monitor") {
        touch(&go_monitor)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let tag = format!("{} busy panes, monitor open", busy.len());
        loaded_mon = Some(phase(&c, pid, &busy, s, &mut rows, &tag).await?);
        typing(&rig, &c, &mut cancel, o.samples, &mut rows, &tag).await?;
    }

    // 5. Every slow extension, the monitor still open.
    let grant = |methods: &[&str]| api::ScriptGrant {
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        panes: busy.clone(),
        all_panes: false,
    };
    if o.with("filter") {
        c.call(Request::FiltersInstall(api::FilterSpec {
            name: "slow".into(),
            panes: busy.clone(),
            pattern: "busy output".into(),
            literal: true,
            event: true,
            grant: Some(grant(&["taps.subscribe"])),
            ..api::FilterSpec::default()
        }))
        .await
        .map_err(|e| format!("filters.install: {e}"))?;
    }
    let scripted: &[(&str, &str)] = if o.with("filter") && o.with("scripts") {
        &[("slow", SLOW_SCRIPT), ("overrun", OVERRUN_SCRIPT)]
    } else {
        &[]
    };
    for &(name, source) in scripted {
        c.call(Request::ScriptsLoad(api::ScriptLoad {
            name: name.into(),
            source: source.into(),
            grant: Some(grant(&["events.subscribe"])),
            events: vec!["filters.matched".into()],
            limits: None,
        }))
        .await
        .map_err(|e| format!("scripts.load {name}: {e}"))?;
    }
    let mut tap = None;
    if o.with("raw-tap") {
        tap = Some(stalled_tap(&rig, &busy).await?);
    }
    let restarts_before: u32 = extensions(&c)
        .await?
        .iter()
        .filter(|x| x.name != "monitor")
        .map(|x| x.restarts)
        .sum();
    for part in ["stalled", "crasher"] {
        if o.with(part) {
            touch(&rig.root.join(if part == "stalled" {
                "go-stall"
            } else {
                "go-crash"
            }))?;
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let keep = keeper(&rig, stop.clone());
    let before = scripts(&c).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let tag = if o.without.is_empty() {
        format!("{} busy panes, every slow extension", busy.len())
    } else {
        format!(
            "{} busy panes, slow extensions without {}",
            busy.len(),
            o.without.join(", ")
        )
    };
    let slow = phase(&c, pid, &busy, s, &mut rows, &tag).await?;
    let slow_p95 = typing(&rig, &c, &mut cancel, o.samples, &mut rows, &tag).await?;
    stop.store(true, Ordering::Relaxed);
    let (crasher_restarts, overrun_reloads) = keep.await.map_err(|e| e.to_string())?;

    // What the extensions went through.
    let handled =
        |l: &[api::ScriptStatus], n: &str| l.iter().find(|s| s.name == n).map_or(0, |s| s.handled);
    let after = scripts(&c).await?;
    rows.put(
        "slow script: matches handled (45 ms each)",
        handled(&after, "slow").saturating_sub(handled(&before, "slow")) as f64,
        "",
    );
    let overrun = after.iter().find(|s| s.name == "overrun");
    rows.put(
        "overrunning script: restarts after a budget overrun",
        overrun.map_or(0, |s| s.restarts) as f64,
        "",
    );
    rows.put(
        "overrunning script: reloaded after being parked",
        overrun_reloads as f64,
        "",
    );
    if let Some(f) = filters(&c).await?.iter().find(|f| f.name == "slow") {
        rows.put("filter: lines matched", f.matched as f64, "");
        rows.put(
            "filter: matches over its rate, dropped",
            f.dropped as f64,
            "",
        );
    }
    let exts = extensions(&c).await?;
    let restarts = |n: &str| exts.iter().find(|x| x.name == n).map_or(0, |x| x.restarts);
    rows.put(
        "stalled and crashing extensions: restarts by the daemon",
        f64::from((restarts("stalled") + restarts("crasher")).saturating_sub(restarts_before)),
        "",
    );
    rows.put(
        "crashing extension: restarted after being parked",
        crasher_restarts as f64,
        "",
    );
    if let Some((_client, rx)) = tap.as_mut() {
        let (gaps, alive) = drain_tap(rx).await;
        rows.put(
            "stalled raw tap: gaps reported once read again",
            gaps as f64,
            "",
        );
        rows.put(
            "stalled raw tap: connection still open (1 yes, 0 no)",
            if alive { 1.0 } else { 0.0 },
            "",
        );
    }

    // Costs, extension apart from terminal.
    if let Some(quiet_mon) = &quiet_mon {
        rows.put(
            "monitor open, quiet: daemon CPU added",
            quiet_mon.cpu - quiet.cpu,
            "% of a core",
        );
        rows.put(
            "monitor open, quiet: daemon heap added",
            (quiet_mon.heap_mib - quiet.heap_mib) * 1024.0,
            "KiB",
        );
        rows.put(
            "monitor open, quiet: daemon RSS added",
            (quiet_mon.rss_mib - quiet.rss_mib) * 1024.0,
            "KiB",
        );
        if let Some(v) = quiet_mon.monitor_cpu {
            rows.put("monitor process CPU, quiet", v, "% of a core");
        }
    }
    rows.put(
        &format!(
            "{} busy panes: daemon CPU added (terminal cost)",
            busy.len()
        ),
        loaded.cpu - quiet.cpu,
        "% of a core",
    );
    if let Some(loaded_mon) = &loaded_mon {
        rows.put(
            "monitor open, busy: daemon CPU added",
            loaded_mon.cpu - loaded.cpu,
            "% of a core",
        );
        rows.put(
            "monitor open, busy: daemon heap added",
            (loaded_mon.heap_mib - loaded.heap_mib) * 1024.0,
            "KiB",
        );
        rows.put(
            "monitor open, busy: daemon RSS added",
            (loaded_mon.rss_mib - loaded.rss_mib) * 1024.0,
            "KiB",
        );
        if let Some(v) = loaded_mon.monitor_cpu {
            rows.put("monitor process CPU, busy", v, "% of a core");
        }
        if let Some(v) = loaded_mon.monitor_rss_mib {
            rows.put("monitor process RSS, busy", v, "MiB");
        }
    }
    let base = loaded_mon.as_ref().unwrap_or(&loaded);
    rows.put(
        "slow extensions: daemon CPU added (Luau scripts included)",
        slow.cpu - base.cpu,
        "% of a core",
    );
    rows.put(
        "slow extensions: daemon heap added",
        (slow.heap_mib - base.heap_mib) * 1024.0,
        "KiB",
    );

    let checks = vec![
        json!({
            "check": format!("key to echo p95 with every slow extension ({slow_p95:.1} ms) within the one-pane budget + 10 ms ({BUDGET_MS} ms)"),
            "met": slow_p95 <= BUDGET_MS,
        }),
        json!({
            "check": format!("key to echo p95 with every slow extension ({slow_p95:.1} ms) within this run's quiet p95 + 10 ms ({:.1} ms)", quiet_p95 + 10.0),
            "met": slow_p95 <= quiet_p95 + 10.0,
        }),
        json!({
            "check": format!("every busy pane's mirror advanced in every second with every slow extension ({} pane-seconds without progress, {} panes never moved)", slow.stuck, slow.unmoved),
            "met": slow.stuck == 0 && slow.unmoved == 0,
        }),
    ];
    for go in ["go-monitor", "go-stall", "go-crash"] {
        let _ = std::fs::remove_file(rig.root.join(go));
    }
    Ok((rows.0, checks))
}

/// One of the supervised stand-ins, speaking JSON-RPC on standard input
/// and output (docs/extensions.md). Dormant until the file `GO` exists.
///
/// - `monitor`: while `GO` exists, follows every pane at the `text` level
///   and the event log, reads the topology once a second, and reads
///   everything it is sent; when `GO` goes, it unsubscribes.
/// - `stall`: once `GO` exists, follows every pane at the `raw` level and
///   the event log, then stops reading and answering.
/// - `crash`: once `GO` exists, exits with an error, and again right after
///   each hello.
///
/// Every role exits when the benchmark's directory is gone.
fn stand_in(args: &[String]) -> Result<(), String> {
    let [role, go] = args else {
        return Err("usage: dreamer-bench extensions --stand-in monitor|stall|crash GO".into());
    };
    let role = role.clone();
    let go = PathBuf::from(go);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let send = {
        let out = out.clone();
        move |v: Value| {
            if let Ok(mut o) = out.lock() {
                let _ = writeln!(o, "{v}");
                let _ = o.flush();
            }
        }
    };
    let stalled = Arc::new(AtomicBool::new(false));
    // The monitor's subscribe requests by id, with the method that ends
    // each, and the subscriptions their replies named.
    let asked = Arc::new(Mutex::new(HashMap::<u64, &'static str>::new()));
    let subscriptions = Arc::new(Mutex::new(Vec::<(u64, &'static str)>::new()));

    {
        let (role, go, send, stalled, asked, subscriptions) = (
            role.clone(),
            go.clone(),
            send.clone(),
            stalled.clone(),
            asked.clone(),
            subscriptions.clone(),
        );
        std::thread::spawn(move || {
            let mut next_id = 1u64;
            let mut request = |method: &str, params: Value, ends: Option<&'static str>| {
                let id = next_id;
                next_id += 1;
                if let (Some(e), Ok(mut a)) = (ends, asked.lock()) {
                    a.insert(id, e);
                }
                send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
            };
            let tap = |level: api::TapLevel| {
                serde_json::to_value(api::TapSubscribe {
                    level: level as i32,
                    every_pane: true,
                    ..api::TapSubscribe::default()
                })
                .unwrap_or_default()
            };
            let mut open = false;
            let mut tick = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if go.parent().is_none_or(|d| !d.exists()) {
                    std::process::exit(0);
                }
                let wanted = go.exists();
                match role.as_str() {
                    "monitor" if wanted && !open => {
                        open = true;
                        request(
                            "taps.subscribe",
                            tap(api::TapLevel::Text),
                            Some("taps.unsubscribe"),
                        );
                        request("events.subscribe", json!({}), Some("events.unsubscribe"));
                    }
                    "monitor" if !wanted && open => {
                        open = false;
                        let subs: Vec<(u64, &str)> = subscriptions
                            .lock()
                            .map(|mut s| s.drain(..).collect())
                            .unwrap_or_default();
                        for (sub, method) in subs {
                            request(method, json!({ "subscription": sub }), None);
                        }
                    }
                    "monitor" if open && tick.elapsed() >= Duration::from_secs(1) => {
                        tick = Instant::now();
                        request("tmux.topology", json!({}), None);
                    }
                    "stall" if wanted && !stalled.load(Ordering::Relaxed) => {
                        request("taps.subscribe", tap(api::TapLevel::Raw), None);
                        request("events.subscribe", json!({}), None);
                        stalled.store(true, Ordering::Relaxed);
                    }
                    "crash" if wanted => {
                        eprintln!("crashing on purpose");
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
        });
    }

    // The reader: the daemon's requests, replies, and notifications.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if stalled.load(Ordering::Relaxed) {
            // Stop reading and answering for good; the daemon kills us.
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if go.parent().is_none_or(|d| !d.exists()) {
                    std::process::exit(0);
                }
            }
        }
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match (v.get("id"), v.get("method").and_then(Value::as_str)) {
            (Some(id), Some(method)) => {
                let result = match method {
                    "extension.hello" => json!({ "version": format!("bench stand-in {role}") }),
                    _ => json!({}),
                };
                send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                if method == "extension.shutdown" {
                    std::process::exit(0);
                }
                if method == "extension.hello" && role == "crash" && go.exists() {
                    eprintln!("crashing on purpose");
                    std::process::exit(1);
                }
            }
            (Some(id), None) => {
                // A reply to one of ours: keep the monitor's subscriptions.
                let ends = id.as_u64().and_then(|id| asked.lock().ok()?.remove(&id));
                if let Some(ends) = ends
                    && let Some(sub) = v.pointer("/result/id").and_then(Value::as_u64)
                    && let Ok(mut s) = subscriptions.lock()
                {
                    s.push((sub, ends));
                }
            }
            _ => {}
        }
    }
    Ok(())
}
