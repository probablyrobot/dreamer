//! `dreamer-bench streams`: panes replaying fixed byte corpora (plain
//! text, escape-heavy TUI redraws, alternate-screen synchronized updates,
//! Kitty graphics), with a client following one of them and typing into a
//! quiet pane alongside (spec 16.5).
//!
//! Each corpus gets a fresh tmux server and daemon, so memory growth is
//! that corpus's alone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use terminal_ghostty::core::GridSize;
use terminal_ghostty::{Engine, EngineConfig};

use crate::corpus;
use crate::{
    Rig, cpu_seconds, hardware, heap_kib, key_to_echo, mirrors, percentile, print_table, revision,
    rss_kib, wait_live, write_json,
};

const MIB: f64 = 1024.0 * 1024.0;

pub fn streams(args: &[String]) -> Result<(), String> {
    let mut panes = 16usize;
    let mut seconds = 10u64;
    let mut only: Vec<String> = Vec::new();
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--panes" => {
                panes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n >= 1)
                    .ok_or("--panes needs a number of at least 1")?;
            }
            "--seconds" => {
                seconds = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n >= 1)
                    .ok_or("--seconds needs a number of at least 1")?;
            }
            "--corpus" => {
                let c = it.next().ok_or("--corpus needs a name")?;
                if !corpus::NAMES.contains(&c.as_str()) {
                    return Err(format!(
                        "no corpus {c}; there are {}",
                        corpus::NAMES.join(", ")
                    ));
                }
                only.push(c.clone());
            }
            "--json" => json_out = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let names: Vec<&str> = corpus::NAMES
        .iter()
        .copied()
        .filter(|n| only.is_empty() || only.iter().any(|o| o == n))
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let mut corpora = Vec::new();
    for name in &names {
        let bytes = corpus::generate(name).ok_or("unknown corpus")?;
        corpora.push(json!({
            "name": name,
            "what": corpus::describe(name),
            "bytes": bytes.len(),
            "fnv1a64": format!("{:016x}", corpus::fnv(&bytes)),
        }));
        eprintln!("bench: corpus {name}, {} bytes", bytes.len());
        results.extend(rt.block_on(run(name, &bytes, panes, seconds))?);
    }
    let split = format!(
        "{panes} busy pane{} replay the corpus in a loop with `cat`; a client replica follows the first, as a visible pane; the others have no client (Warm, unpainted); keys are typed into one more, quiet pane running `cat`",
        if panes == 1 { "" } else { "s" }
    );
    let report = json!({
        "workload": "streams",
        "panes": panes,
        "seconds": seconds,
        "split": split,
        "corpora": corpora,
        "hardware": hardware(),
        "revision": revision(),
        "results": results,
    });
    println!("panes: {split}");
    for c in &corpora {
        println!(
            "corpus {}: {} bytes, fnv1a64 {}; {}",
            c["name"].as_str().unwrap_or(""),
            c["bytes"],
            c["fnv1a64"].as_str().unwrap_or(""),
            c["what"].as_str().unwrap_or("")
        );
    }
    print_table(&report);
    if let Some(p) = json_out {
        write_json(&p, &report)?;
    }
    Ok(())
}

/// The corpus fed to one engine configured as a daemon mirror, on this
/// thread, with no tmux, transport, or frames: what the VT parser alone
/// sustains, in MiB/s. Fails when the images corpus placed no image, so
/// the workload cannot quietly measure something else.
fn parse_alone(name: &str, bytes: &[u8]) -> Result<f64, String> {
    let size = GridSize {
        cols: corpus::COLS as u16,
        rows: corpus::ROWS as u16,
    };
    let mut config = EngineConfig::new(size);
    config.scrollback_lines = Some(2000);
    config.scrollback_bytes = Some(usize::MAX);
    config.continuation_max_bytes = Some(terminal_ghostty::core::family::CONTINUATION_MAX_BYTES);
    let mut e = Engine::new(config).map_err(|e| e.to_string())?;
    e.resize(size, terminal_ghostty::core::family::CANONICAL_CELL_PX)
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut fed = 0usize;
    // At least three passes and half a second.
    while fed < 3 * bytes.len() || started.elapsed() < Duration::from_millis(500) {
        for c in bytes.chunks(64 << 10) {
            e.feed(c).map_err(|e| e.to_string())?;
        }
        fed += bytes.len();
    }
    let secs = started.elapsed().as_secs_f64();
    if name == "images" {
        let placed = e.frame().map_err(|e| e.to_string())?.images.len();
        if placed == 0 || e.images_skipped() > 0 {
            return Err(format!(
                "the images corpus placed {placed} images and skipped {}",
                e.images_skipped()
            ));
        }
    }
    Ok(fed as f64 / MIB / secs)
}

async fn run(name: &str, bytes: &[u8], panes: usize, seconds: u64) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut put = |measure: &str, value: f64, unit: &str| {
        let measure = format!("{name}: {measure}");
        eprintln!("bench: {measure}: {value:.2} {unit}");
        out.push(json!({ "measure": measure, "value": value, "unit": unit }));
    };
    put(
        "engine parse alone, one thread",
        parse_alone(name, bytes)?,
        "MiB/s",
    );

    let rig = Rig::start()?;
    let file = rig.root.join(format!("corpus-{name}.bin"));
    std::fs::write(&file, bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    let c = rig.client().await?;
    wait_live(&c, 1, Duration::from_secs(15)).await?;
    let pid = rig.pid()?;
    let tmux_pid: u32 = rig
        .tmux(&["display-message", "-p", "#{pid}"])?
        .trim()
        .parse()
        .map_err(|_| "no tmux pid")?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let rss_before = rss_kib(pid)?;
    let heap_before = heap_kib(&c).await?;

    // The busy panes.
    let before: Vec<u32> = mirrors(&c).await?.iter().map(|m| m.pane).collect();
    rig.windows(panes, &replay(&file))?;
    wait_live(&c, panes + 1, Duration::from_secs(120)).await?;
    let busy: Vec<u32> = mirrors(&c)
        .await?
        .iter()
        .map(|m| m.pane)
        .filter(|p| !before.contains(p))
        .collect();
    let visible = *busy.first().ok_or("no busy pane")?;
    let feed =
        replica::feed::Feed::start(rig.socket(), Some(visible), None).map_err(|e| e.to_string())?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // The measured window: at least `seconds`, and as long as typing takes.
    let ingested = |m: &[api::MirrorStatus]| -> u64 {
        m.iter()
            .filter(|m| busy.contains(&m.pane))
            .map(|m| m.ingested_bytes)
            .sum()
    };
    let rebuilds = |m: &[api::MirrorStatus]| -> u64 {
        m.iter()
            .filter(|m| busy.contains(&m.pane))
            .map(|m| u64::from(m.reattaches))
            .sum()
    };
    let m0 = mirrors(&c).await?;
    let (bytes0, rebuilds0) = (ingested(&m0), rebuilds(&m0));
    let frames0 = feed.stats.frames.load(std::sync::atomic::Ordering::Relaxed);
    let holds0 = feed
        .stats
        .holds_expired
        .load(std::sync::atomic::Ordering::Relaxed);
    let (cpu0, tcpu0) = (cpu_seconds(pid)?, cpu_seconds(tmux_pid)?);
    let started = Instant::now();
    let echo = key_to_echo(&rig, &c, 100).await?;
    let left = Duration::from_secs(seconds).saturating_sub(started.elapsed());
    tokio::time::sleep(left).await;
    let secs = started.elapsed().as_secs_f64();
    let (cpu1, tcpu1) = (cpu_seconds(pid)?, cpu_seconds(tmux_pid)?);
    let frames1 = feed.stats.frames.load(std::sync::atomic::Ordering::Relaxed);
    let holds1 = feed
        .stats
        .holds_expired
        .load(std::sync::atomic::Ordering::Relaxed);
    let m1 = mirrors(&c).await?;
    let mib = (ingested(&m1).saturating_sub(bytes0)) as f64 / MIB;
    let daemon_cpu = cpu1 - cpu0;

    put(
        &format!("daemon ingest, {panes} busy panes"),
        mib / secs,
        "MiB/s",
    );
    put("daemon CPU", daemon_cpu / secs * 100.0, "% of a core");
    put(
        "daemon CPU per MiB ingested",
        if mib > 0.0 {
            daemon_cpu * 1000.0 / mib
        } else {
            f64::NAN
        },
        "ms",
    );
    put("tmux CPU", (tcpu1 - tcpu0) / secs * 100.0, "% of a core");
    put(
        "replica frames built, visible pane",
        (frames1 - frames0) as f64 / secs,
        "per s",
    );
    put(
        "synchronized-output holds shown at the limit",
        (holds1 - holds0) as f64,
        "",
    );
    put(
        "key to echo in a quiet pane, p50",
        percentile(&echo, 0.5),
        "ms",
    );
    put(
        "key to echo in a quiet pane, p95",
        percentile(&echo, 0.95),
        "ms",
    );
    put(
        "mirror rebuilds (output outran a mirror)",
        (rebuilds(&m1) - rebuilds0) as f64,
        "",
    );
    put(
        "daemon RSS growth under load",
        (rss_kib(pid)? as f64 - rss_before as f64) / 1024.0,
        "MiB",
    );
    put(
        "daemon heap growth under load",
        (heap_kib(&c).await? - heap_before) / 1024.0,
        "MiB",
    );
    drop(feed);

    // Bounded: what stays held once the busy panes are gone.
    for p in &busy {
        let _ = rig.tmux(&["kill-pane", "-t", &format!("%{p}")]);
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    put(
        "daemon heap held after the busy panes close",
        (heap_kib(&c).await? - heap_before) / 1024.0,
        "MiB",
    );
    Ok(out)
}

/// A shell command replaying a file forever.
fn replay(file: &Path) -> String {
    format!("sh -c 'while :; do cat {}; done'", file.display())
}
