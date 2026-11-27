//! `dreamer-bench memory`: the daemon's memory under the four kinds of
//! pressure, each against a fresh daemon:
//!
//! - **output:** `N` panes (64 by default) run `yes`;
//! - **lagging:** the same load, and one client subscribes to every busy
//!   pane, then stops reading;
//! - **paste:** a paste of `M` MiB (10 by default) into one pane;
//! - **extension:** a supervised extension that crashes in a loop.
//!
//! Each runs for `--seconds` (300 by default; the paste runs at least
//! until it is delivered) and samples, once a second, the daemon's
//! resident size, its heap in use, the host's journal, and on macOS its
//! physical footprint (which leaves out free pages the allocator keeps
//! mapped, where resident size counts them). Each scenario
//! states a bound built from the daemon's own budgets and records whether
//! the peak stayed under it: the peak footprint where it can be read
//! (macOS), otherwise the peak resident size.
//!
//! ```text
//! dreamer-bench memory [--scenario output|lagging|paste|extension]...
//!     [--seconds S] [--panes N] [--paste-mib M] [--backoff-ms B]
//!     [--warm-memory MIB] [--json FILE]
//! ```
//!
//! `--warm-memory` sets `tiers.warm_memory` (1 GiB by default). In the
//! output scenario a client replica then follows the first busy pane, as
//! a desktop view does, and the run records how many panes went Cold,
//! whether the followed one did, and what the daemon says its mirrors
//! hold against the ceiling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use serde_json::{Value, json};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

use super::{Rig, hardware, heap_kib, print_table, revision, rss_kib, wait_live, write_json};

const MIB: f64 = 1024.0 * 1024.0;

/// Output a pane's worker has no room for is held, up to this much, before
/// the mirror is rebuilt (docs/daemon.md, "Under pressure").
const HELD_PER_PANE: f64 = 8.0 * MIB;
/// A Warm mirror's screen and history: about 1 KiB for each line kept
/// as measured, with the 2,000-line history the rig sets.
const MIRROR_PER_PANE: f64 = 2000.0 * 1024.0;
/// A stalled connection holds up to this many encoded messages of up to
/// one record batch each (`CONNECTION_QUEUE`, `BATCH_BYTES` in
/// crates/server/src/conn.rs).
const CONNECTION_QUEUE: f64 = 256.0;
const BATCH_BYTES: f64 = 256.0 * 1024.0;
/// Each subscription's queue, in records (`SUBSCRIBER_QUEUE`).
const SUBSCRIBER_QUEUE: f64 = 4096.0;
/// What a running extension and its connection may hold, with allocator
/// noise: a crash loop must stay inside it however long it runs.
const EXTENSION_ALLOWANCE: f64 = 32.0 * MIB;

const YES: &str = "yes 'busy output for the memory benchmark, one line after another'";

struct Options {
    scenarios: Vec<String>,
    seconds: u64,
    panes: usize,
    paste_mib: usize,
    backoff_ms: u64,
    /// `tiers.warm_memory` in MiB; the daemon's default when `None`.
    warm_memory: Option<u64>,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut o = Options {
        scenarios: Vec::new(),
        seconds: 300,
        panes: 64,
        paste_mib: 10,
        backoff_ms: 50,
        warm_memory: None,
    };
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    let number = |v: Option<&String>, what: &str| -> Result<u64, String> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{what} needs a number"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scenario" => {
                let s = it.next().ok_or("--scenario needs a name")?;
                if !["output", "lagging", "paste", "extension"].contains(&s.as_str()) {
                    return Err(format!("unknown scenario {s}"));
                }
                o.scenarios.push(s.clone());
            }
            "--seconds" => o.seconds = number(it.next(), "--seconds")?.max(1),
            "--panes" => o.panes = number(it.next(), "--panes")?.max(1) as usize,
            "--paste-mib" => o.paste_mib = number(it.next(), "--paste-mib")?.max(1) as usize,
            "--backoff-ms" => o.backoff_ms = number(it.next(), "--backoff-ms")?,
            "--warm-memory" => o.warm_memory = Some(number(it.next(), "--warm-memory")?.max(1)),
            "--json" => json_out = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if o.scenarios.is_empty() {
        o.scenarios = ["output", "lagging", "paste", "extension"]
            .map(str::to_owned)
            .to_vec();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let mut bounds = Vec::new();
    let mut curves = serde_json::Map::new();
    for (n, s) in o.scenarios.iter().enumerate() {
        let r = rt.block_on(async {
            match s.as_str() {
                "output" => output(&o, n, false).await,
                "lagging" => output(&o, n, true).await,
                "paste" => paste(&o, n).await,
                _ => extension(&o, n).await,
            }
        })?;
        results.extend(r.results);
        bounds.push(r.bound);
        curves.insert(s.clone(), r.curve);
    }
    let report = json!({
        "workload": "memory",
        "panes": o.panes,
        "seconds": o.seconds,
        "paste_mib": o.paste_mib,
        "extension_backoff_ms": o.backoff_ms,
        "warm_memory_mib": o.warm_memory,
        "scenarios": o.scenarios,
        "hardware": hardware(),
        "revision": revision(),
        "results": results,
        "bounds": bounds,
        "curves": curves,
    });
    print_table(&report);
    for b in &bounds {
        println!(
            "{}: peak {} {:.1} MiB, bound {:.1} MiB: {} ({})",
            b["scenario"].as_str().unwrap_or(""),
            b["measured"].as_str().unwrap_or("RSS"),
            b["peak_mib"].as_f64().unwrap_or(f64::NAN),
            b["bound_mib"].as_f64().unwrap_or(f64::NAN),
            if b["met"].as_bool() == Some(true) {
                "met"
            } else {
                "not met"
            },
            b["basis"].as_str().unwrap_or("")
        );
    }
    if let Some(p) = json_out {
        write_json(&p, &report)?;
    }
    Ok(())
}

/// One scenario's rows, its bound, and its curve.
struct Outcome {
    results: Vec<Value>,
    bound: Value,
    curve: Value,
}

/// A rig like [`Rig::start`], with extra daemon environment and an
/// `extensions.yaml`.
fn rig(
    n: usize,
    env: &[(&str, String)],
    extensions: Option<&str>,
    warm_mib: Option<u64>,
) -> Result<Rig, String> {
    let root = PathBuf::from(format!("/tmp/drm-bmem-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).map_err(|e| e.to_string())?;
    let tmux = TmuxServer::start(120, 40, "/bin/sh").map_err(|e| format!("tmux: {e}"))?;
    tmux.run_checked(&["set-option", "-g", "history-limit", "2000"])
        .map_err(|e| e.to_string())?;
    let warm = warm_mib.map_or(String::new(), |m| format!("  warm_memory: {m}MiB\n"));
    std::fs::write(
        root.join("config/dreamer.yaml"),
        format!("tiers:\n  cold_after: never\n  warm_limit: none\n{warm}"),
    )
    .map_err(|e| e.to_string())?;
    if let Some(x) = extensions {
        std::fs::write(root.join("config/extensions.yaml"), x).map_err(|e| e.to_string())?;
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon = std::env::var_os("DREAMER_BENCH_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe.with_file_name("dreamerd"));
    if !daemon.is_file() {
        return Err(format!(
            "{} is missing; run `cargo xtask bench`",
            daemon.display()
        ));
    }
    let mut cmd = Command::new(&daemon);
    cmd.args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
        .arg(tmux_program())
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let daemon = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", daemon.display()))?;
    Ok(Rig { root, tmux, daemon })
}

/// The daemon's memory once a second.
struct Curve {
    pid: u32,
    t0: Instant,
    next: Instant,
    samples: Vec<[f64; 5]>,
}

impl Curve {
    fn new(pid: u32) -> Curve {
        let now = Instant::now();
        Curve {
            pid,
            t0: now,
            next: now,
            samples: Vec::new(),
        }
    }

    /// Wait for the next second, then sample: time, RSS, heap in use, the
    /// host's journal, and the physical footprint (macOS), in seconds and
    /// MiB (NaN when not available).
    async fn tick(&mut self, c: &Client) -> Result<(), String> {
        tokio::time::sleep_until(self.next.into()).await;
        self.next = (self.next + Duration::from_secs(1)).max(Instant::now());
        let rss = rss_kib(self.pid)? as f64 / 1024.0;
        let heap = heap_kib(c).await.map_or(f64::NAN, |k| k / 1024.0);
        let journal = journal(c)
            .await
            .map_or(f64::NAN, |l| l.host_journal_bytes as f64 / MIB);
        let footprint = platform::phys_footprint(self.pid).map_or(f64::NAN, |b| b as f64 / MIB);
        self.samples.push([
            self.t0.elapsed().as_secs_f64(),
            rss,
            heap,
            journal,
            footprint,
        ]);
        Ok(())
    }

    fn peak(&self, i: usize) -> f64 {
        self.samples
            .iter()
            .map(|s| s[i])
            .filter(|v| v.is_finite())
            .fold(f64::NAN, f64::max)
    }

    fn last(&self, i: usize) -> f64 {
        self.samples.last().map_or(f64::NAN, |s| s[i])
    }

    /// Column `i` at the end less halfway through: whether it still grows.
    fn second_half_growth(&self, i: usize) -> f64 {
        let mid = self
            .samples
            .get(self.samples.len() / 2)
            .map_or(f64::NAN, |s| s[i]);
        self.last(i) - mid
    }

    fn json(&self) -> Value {
        json!({
            "columns": ["seconds", "rss_mib", "heap_mib", "journal_mib", "footprint_mib"],
            "samples": self
                .samples
                .iter()
                .map(|s| s.iter().map(|v| if v.is_finite() { json!(v) } else { Value::Null }).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        })
    }
}

async fn journal(c: &Client) -> Result<api::MirrorList, String> {
    match c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    {
        Ok(Response::TerminalMirrors(l)) => Ok(l),
        Ok(_) => Err("unexpected reply".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Rows common to every scenario, and the bound's verdict.
struct Rows {
    tag: String,
    out: Vec<Value>,
}

impl Rows {
    fn put(&mut self, measure: &str, value: f64, unit: &str) {
        let measure = format!("{}: {measure}", self.tag);
        eprintln!("bench: {measure}: {value:.2} {unit}");
        self.out
            .push(json!({ "measure": measure, "value": value, "unit": unit }));
    }

    fn finish(mut self, before: (f64, f64), curve: &Curve, bound: f64, basis: &str) -> Outcome {
        self.put("daemon RSS before", before.0, "MiB");
        self.put("daemon RSS, peak", curve.peak(1), "MiB");
        self.put("daemon RSS at the end", curve.last(1), "MiB");
        self.put(
            "RSS change over the second half",
            curve.second_half_growth(1),
            "MiB",
        );
        // Where the footprint can be read (macOS) the bound is on it: RSS
        // there also counts free pages the allocator keeps and the system
        // has not taken back.
        let on_footprint = curve.peak(4).is_finite();
        if on_footprint {
            self.put("daemon footprint, peak", curve.peak(4), "MiB");
            self.put("daemon footprint at the end", curve.last(4), "MiB");
            self.put(
                "footprint change over the second half",
                curve.second_half_growth(4),
                "MiB",
            );
        }
        self.put("heap in use before", before.1, "MiB");
        self.put("heap in use, peak", curve.peak(2), "MiB");
        self.put("heap in use at the end", curve.last(2), "MiB");
        self.put("host journal, peak", curve.peak(3), "MiB");
        self.put("stated bound", bound, "MiB");
        let (peak, measured) = if on_footprint {
            (curve.peak(4), "footprint")
        } else {
            (curve.peak(1), "RSS")
        };
        Outcome {
            bound: json!({
                "scenario": self.tag,
                "measured": measured,
                "peak_mib": peak,
                "bound_mib": bound,
                "met": peak.is_finite() && peak <= bound,
                "basis": basis,
            }),
            results: self.out,
            curve: curve.json(),
        }
    }
}

/// The daemon at rest with its first pane live: RSS and heap, in MiB.
async fn at_rest(rig: &Rig, c: &Client) -> Result<(u32, (f64, f64)), String> {
    wait_live(c, 1, Duration::from_secs(15)).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pid = rig.pid()?;
    let rss = rss_kib(pid)? as f64 / 1024.0;
    let heap = heap_kib(c).await.map_or(f64::NAN, |k| k / 1024.0);
    Ok((pid, (rss, heap)))
}

/// Sustained output on N panes; with `lagging`, also a client that
/// subscribes to every busy pane and then stops reading.
async fn output(o: &Options, n: usize, lagging: bool) -> Result<Outcome, String> {
    let rig = rig(n, &[], None, o.warm_memory)?;
    let c = rig.client().await?;
    let (pid, before) = at_rest(&rig, &c).await?;
    let first: Vec<u32> = super::mirrors(&c).await?.iter().map(|m| m.pane).collect();
    rig.windows(o.panes, YES)?;
    wait_live(&c, o.panes + 1, Duration::from_secs(120)).await?;
    let busy: Vec<u32> = super::mirrors(&c)
        .await?
        .iter()
        .map(|m| m.pane)
        .filter(|p| !first.contains(p))
        .collect();
    let tag = if lagging {
        format!("lagging viewer, {} busy panes", o.panes)
    } else {
        format!("sustained output, {} panes", o.panes)
    };
    let mut rows = Rows {
        tag,
        out: Vec::new(),
    };

    // The lagging viewer: subscribe while still reading (the replies come
    // on the same stream as the records), then stop reading.
    let mut viewer = None;
    if lagging {
        let mut v = Client::connect_with(&rig.socket(), "lagging", 1)
            .await
            .map_err(|e| e.to_string())?;
        let mut rx = v.pushed().ok_or("no pushed stream")?;
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let reader = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    p = rx.recv() => if p.is_none() { break },
                }
            }
            rx
        });
        let mut subs = Vec::new();
        for p in &busy {
            match v
                .call(Request::TerminalSubscribe(api::TerminalSubscribe {
                    pane: *p,
                    ..api::TerminalSubscribe::default()
                }))
                .await
            {
                Ok(Response::TerminalSubscribe(s)) => subs.push(s),
                Ok(_) => return Err("unexpected reply to terminal.subscribe".into()),
                Err(e) => return Err(format!("subscribing to %{p}: {e}")),
            }
        }
        let _ = stop.send(());
        let rx = reader.await.map_err(|e| e.to_string())?;
        viewer = Some((v, rx, subs));
    }

    // With a Warm memory ceiling, a view follows the first busy pane: the
    // policy may demote the others, never it.
    let followed = match (o.warm_memory, lagging, busy.first()) {
        (Some(_), false, Some(p)) => Some((*p, follow(&rig, *p).await?)),
        _ => None,
    };
    let mut followed_cold = false;
    let mut cold_max = 0usize;

    let mut curve = Curve::new(pid);
    let end = Instant::now() + Duration::from_secs(o.seconds);
    while Instant::now() < end {
        curve.tick(&c).await?;
        if let Some((p, _)) = &followed {
            let list = journal(&c).await?;
            let cold = |m: &&api::MirrorStatus| m.tier == api::Tier::Cold as i32;
            cold_max = cold_max.max(list.mirrors.iter().filter(cold).count());
            followed_cold |= list.mirrors.iter().filter(cold).any(|m| m.pane == *p);
        }
    }
    let list = journal(&c).await?;
    let reattaches: u32 = list.mirrors.iter().map(|m| m.reattaches).sum();
    rows.put(
        "mirror re-attaches (output held past 8 MiB)",
        f64::from(reattaches),
        "",
    );
    let ingested: u64 = list.mirrors.iter().map(|m| m.ingested_bytes).sum();
    rows.put(
        "output ingested",
        ingested as f64 / MIB / curve.t0.elapsed().as_secs_f64(),
        "MiB/s",
    );
    // What the journal really costs: heap growth over the bytes it counts
    // against its budget (mirror pages are outside the allocator).
    if list.host_journal_bytes > 0 {
        rows.put(
            "heap growth per journal byte counted",
            (curve.last(2) - before.1) * MIB / list.host_journal_bytes as f64,
            "bytes",
        );
    }
    let journal_cap =
        (list.journal_host_limit as f64).min(list.journal_pane_limit as f64 * (o.panes + 1) as f64);
    let panes = (o.panes + 1) as f64;
    let mut bound = before.0 + (journal_cap + panes * (HELD_PER_PANE + MIRROR_PER_PANE)) / MIB;
    let mut basis = format!(
        "RSS before + journal {:.0} MiB + {} panes x (8 MiB held output + 2 MiB mirror)",
        journal_cap / MIB,
        o.panes + 1
    );

    if let Some((v, mut rx, subs)) = viewer {
        // The largest retained record, on average per pane: what one
        // queued record costs.
        let record = list
            .mirrors
            .iter()
            .filter(|m| m.journal_records > 0)
            .map(|m| m.journal_bytes as f64 / f64::from(m.journal_records))
            .fold(0.0, f64::max);
        rows.put("average retained record, largest pane", record, "bytes");
        let queued = CONNECTION_QUEUE * BATCH_BYTES + subs.len() as f64 * SUBSCRIBER_QUEUE * record;
        bound += queued / MIB;
        basis += &format!(
            " + stalled connection 64 MiB + {} subscriptions x 4,096 records x {record:.0} bytes",
            subs.len(),
        );

        // Read again: every subscription must have ended, as lagged.
        let want: Vec<u64> = subs.iter().map(|s| s.id).collect();
        let mut ends: HashMap<u64, i32> = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        while ends.len() < want.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(Pushed::Event { topic, payload, .. }))
                    if topic == api::topic::TERMINAL_END =>
                {
                    if let Ok(api::Notification::TerminalEnd(e)) =
                        api::Notification::decode(&topic, &payload)
                        && want.contains(&e.subscription)
                    {
                        ends.insert(e.subscription, e.reason);
                    }
                }
                Ok(Some(Pushed::Closed { .. }) | None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
        }
        let lagged = ends
            .values()
            .filter(|r| **r == api::EndReason::Lagged as i32)
            .count();
        rows.put(
            &format!("subscriptions ended as lagged, of {}", subs.len()),
            lagged as f64,
            "",
        );
        // Keep reading from here on, so replies get through.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        // Resuming from where each subscription started: the journal no
        // longer holds that point, so the daemon answers with a checkpoint.
        let mut checkpoints = 0usize;
        for s in &subs {
            if let Ok(Response::TerminalSubscribe(r)) = v
                .call(Request::TerminalSubscribe(api::TerminalSubscribe {
                    pane: s.pane,
                    resume_epoch: Some(s.epoch),
                    resume_generation: Some(s.generation),
                    resume_sequence: Some(s.sequence),
                    ..api::TerminalSubscribe::default()
                }))
                .await
                && r.mode == api::SubscribeMode::Checkpoint as i32
            {
                checkpoints += 1;
            }
        }
        rows.put(
            &format!(
                "resumes from the start answered with a checkpoint (journal trimmed), of {}",
                subs.len()
            ),
            checkpoints as f64,
            "",
        );
        drain.abort();
        if lagged < subs.len() {
            basis += "; NOT every stalled subscription ended as lagged";
            bound = 0.0;
        }
    }
    if let (Some((_, feed)), Some(ceiling)) = (&followed, o.warm_memory) {
        let cold = list
            .mirrors
            .iter()
            .filter(|m| m.tier == api::Tier::Cold as i32)
            .count();
        rows.put("panes Cold at the end", cold as f64, "");
        rows.put("panes Cold, most at once", cold_max as f64, "");
        rows.put(
            "followed pane went Cold (1: yes)",
            f64::from(u8::from(followed_cold)),
            "",
        );
        rows.put(
            "frames the follower took",
            feed.stats.frames.load(std::sync::atomic::Ordering::Relaxed) as f64,
            "",
        );
        // What the daemon estimates its mirrors hold, as its tier policy
        // logs it after each demotion round.
        let held = policy_estimates(&rig);
        rows.put("Warm memory ceiling", ceiling as f64, "MiB");
        rows.put(
            "mirrors' memory after a demotion round, largest (daemon's estimate)",
            held.iter().copied().fold(f64::NAN, f64::max),
            "MiB",
        );
        rows.put(
            "mirrors' memory after the last demotion round (daemon's estimate)",
            held.last().copied().unwrap_or(f64::NAN),
            "MiB",
        );
        basis += &format!("; Warm memory ceiling {ceiling} MiB");
        if followed_cold {
            basis += "; the followed pane went Cold";
            bound = 0.0;
        }
    }
    Ok(rows.finish(before, &curve, bound, &basis))
}

/// Follow a pane with a client replica that takes every frame, as a
/// desktop view does.
async fn follow(rig: &Rig, pane: u32) -> Result<std::sync::Arc<replica::feed::Feed>, String> {
    let feed = std::sync::Arc::new(
        replica::feed::Feed::start(rig.socket(), Some(pane), None).map_err(|e| e.to_string())?,
    );
    super::next_frame(&feed, Instant::now() + Duration::from_secs(30))
        .await
        .ok_or(format!("pane %{pane} never showed a frame"))?;
    let wake = feed.wake.clone();
    let weak = std::sync::Arc::downgrade(&feed);
    tokio::spawn(async move {
        while wake.recv().await.is_ok() {
            let Some(f) = weak.upgrade() else { return };
            drop(f.take());
        }
    });
    Ok(feed)
}

/// The "mirrors now hold about N MiB" figures the daemon's tier policy
/// logged, in order.
fn policy_estimates(rig: &Rig) -> Vec<f64> {
    let log = std::fs::read_to_string(rig.root.join("st/dreamerd.log")).unwrap_or_default();
    log.lines()
        .filter_map(|l| {
            let rest = l.split("mirrors now hold about ").nth(1)?;
            rest.split(' ').next()?.parse().ok()
        })
        .collect()
}

/// A large paste into one pane, whose program reads and discards it.
async fn paste(o: &Options, n: usize) -> Result<Outcome, String> {
    let rig = rig(n, &[], None, o.warm_memory)?;
    let c = rig.client().await?;
    let (pid, before) = at_rest(&rig, &c).await?;
    let pane = rig
        .tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "stty raw -echo; exec cat >/dev/null",
        ])?
        .trim()
        .trim_start_matches('%')
        .parse::<u32>()
        .map_err(|_| "no pane id for the paste")?;
    wait_live(&c, 2, Duration::from_secs(15)).await?;
    // Text, as a person would paste it: lines of 79 characters.
    let size = o.paste_mib << 20;
    let line = b"pasted text for the memory benchmark, a line of seventy-nine characters ......\n";
    let data: Vec<u8> = line.iter().copied().cycle().take(size).collect();
    let paster = rig.client().await?;
    let mut rows = Rows {
        tag: format!("paste of {} MiB", o.paste_mib),
        out: Vec::new(),
    };
    let mut curve = Curve::new(pid);
    let started = Instant::now();
    let mut task = tokio::spawn(async move {
        let r = client::paste(&paster, pane, None, &data).await;
        (r.map_err(|e| e.to_string()), started.elapsed())
    });
    let mut delivered = None;
    let end = started + Duration::from_secs(o.seconds);
    let limit = started + Duration::from_secs(o.seconds.max(600));
    while delivered.is_none() || Instant::now() < end {
        if delivered.is_none() && Instant::now() > limit {
            task.abort();
            return Err("the paste was not delivered within 10 minutes".into());
        }
        curve.tick(&c).await?;
        if delivered.is_none()
            && let Ok(r) = tokio::time::timeout(Duration::ZERO, &mut task).await
        {
            let (r, took) = r.map_err(|e| e.to_string())?;
            r?;
            delivered = Some(took);
        }
    }
    let took = delivered.unwrap_or_default().as_secs_f64();
    rows.put("paste delivered in", took, "s");
    rows.put("paste rate", o.paste_mib as f64 / took.max(1e-3), "MiB/s");
    let bound = before.0 + 4.0 * o.paste_mib as f64;
    let basis = format!(
        "RSS before + 4 x the paste: its gathered parts, the encoded copy, and parts in flight ({} MiB)",
        4 * o.paste_mib
    );
    Ok(rows.finish(before, &curve, bound, &basis))
}

/// An extension that answers `extension.hello`, subscribes to events, and
/// exits with an error: a crash loop for as long as the scenario runs.
const CRASHER: &str = r#"IFS= read -r line || exit 1
id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"jsonrpc":"2.0","id":"%s","result":{"version":"crash-loop"}}\n' "$id"
printf '{"jsonrpc":"2.0","id":1,"method":"events.subscribe","params":{}}\n'
IFS= read -r line
echo "crashing on purpose" >&2
exit 1
"#;

/// A supervised extension that crashes in a loop. The daemon parks it
/// after 8 failures in a row; the scenario restarts it each time.
async fn extension(o: &Options, n: usize) -> Result<Outcome, String> {
    let script = std::env::temp_dir().join(format!("drm-bmem-crash-{}.sh", std::process::id()));
    std::fs::write(&script, CRASHER).map_err(|e| e.to_string())?;
    let yaml = format!(
        "extensions:\n  - name: crasher\n    argv: [/bin/sh, {}]\n    grant:\n      methods: [events.subscribe]\n      all_panes: true\n",
        script.display()
    );
    let rig = rig(
        n,
        &[(
            "DREAMER_TEST_EXTENSION_BACKOFF_MS",
            o.backoff_ms.to_string(),
        )],
        Some(&yaml),
        o.warm_memory,
    );
    let r = async {
        let rig = rig?;
        let c = rig.client().await?;
        let (pid, before) = at_rest(&rig, &c).await?;
        let mut rows = Rows {
            tag: format!("extension crash loop, {} ms first backoff", o.backoff_ms),
            out: Vec::new(),
        };
        let mut curve = Curve::new(pid);
        let mut parked = 0u32;
        let mut last_error = None;
        let end = Instant::now() + Duration::from_secs(o.seconds);
        while Instant::now() < end {
            curve.tick(&c).await?;
            let Ok(Response::ExtensionsList(l)) =
                c.call(Request::ExtensionsList(api::Empty {})).await
            else {
                continue;
            };
            if let Some(e) = l.extensions.iter().find_map(|x| x.last_error.clone()) {
                last_error = Some(e);
            }
            if l.extensions
                .iter()
                .any(|x| x.state == api::ExtensionState::Failed as i32)
            {
                parked += 1;
                c.call(Request::ExtensionsRestart(api::ExtensionRef {
                    name: "crasher".into(),
                }))
                .await
                .map_err(|e| format!("extensions.restart: {e}"))?;
            }
        }
        let crashes = std::fs::read_to_string(rig.root.join("st/extensions/crasher.log"))
            .unwrap_or_default()
            .matches("crashing on purpose")
            .count();
        eprintln!("bench: the extension's last error: {last_error:?}");
        rows.put("extension runs that crashed", crashes as f64, "");
        rows.put("times parked and restarted", f64::from(parked), "");
        let bound = before.0 + EXTENSION_ALLOWANCE / MIB;
        Ok(rows.finish(
            before,
            &curve,
            bound,
            "RSS before + 32 MiB for a running extension, its connection, and allocator noise",
        ))
    }
    .await;
    let _ = std::fs::remove_file(&script);
    r
}
