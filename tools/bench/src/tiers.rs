//! `dreamer-bench tiers`: what a pane costs the daemon in each tier
//! (ADR 0004), idle and busy.
//!
//! ```text
//! dreamer-bench tiers [--panes N] [--stream text|escape] [--rate KIB]
//!                     [--seconds S] [--json FILE]
//! ```
//!
//! A Cold pane has no mirror, but tmux still sends its output to the
//! daemon, which reads and decodes it before dropping it. This workload
//! puts a number on that next to a Warm and a Hot pane:
//!
//! 1. **Idle.** `N` idle shells, measured Warm, then Cold, then Hot
//!    (each followed by a client replica in this process, as a desktop
//!    view follows it). Then they are closed.
//! 2. **Busy.** `N` panes writing output, measured Warm, then Cold, then
//!    Hot.
//! 3. **No daemon.** The daemon is stopped and tmux is measured alone with
//!    the same busy panes.
//!
//! Each step samples, over `S` seconds (10 by default), the CPU of the
//! daemon, of tmux, and in the Hot steps of the replicas, and the daemon's
//! heap. Per-pane figures are the growth over the step before the panes
//! existed, divided by `N`.
//!
//! The busy panes write `--rate KIB` KiB a second each (256 by default),
//! in ten bursts of `yes` output a second, so every tier sees the same
//! output and the CPU figures compare. `--rate 0` runs `yes` flat out
//! instead, as fast as tmux reads it. `--stream escape` writes lines full
//! of colour, attribute, and cursor sequences instead of plain text.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::Request;
use client::Client;
use serde_json::{Value, json};

use crate::{Rig, cpu_seconds, heap_kib, mirrors, rss_kib, wait_live};

/// The plain line, as the `busy` workload writes it.
const TEXT_LINE: &str = "busy output for the benchmark, one line after another";

/// A line of SGR colours (basic, 256, and direct), attributes on and off,
/// a carriage return, a cursor move, and an erase: terminal work on every
/// line instead of plain cells.
const ESCAPE_LINE: &str = "\x1b[1;31mbusy\x1b[0m \x1b[38;5;208mescape\x1b[0m \
\x1b[48;2;20;40;60mstream\x1b[0m \x1b[4mfor\x1b[24m the \x1b[7mbenchmark\x1b[27m \
\x1b[3;38;2;200;120;40mone line\x1b[23;39m\r\x1b[2C\x1b[K\x1b[32mrewritten\x1b[39m after another";

/// Bursts a second for a paced stream.
const BURSTS: usize = 10;

struct Options {
    panes: usize,
    escape: bool,
    /// KiB a second per pane; 0 for `yes` flat out.
    rate: usize,
    seconds: u64,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut o = Options {
        panes: 16,
        escape: false,
        rate: 256,
        seconds: 10,
    };
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    let number = |it: &mut std::slice::Iter<'_, String>, what: &str| {
        it.next()
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or(format!("{what} needs a number"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--panes" => o.panes = number(&mut it, "--panes")?.max(1),
            "--rate" => o.rate = number(&mut it, "--rate")?,
            "--seconds" => o.seconds = number(&mut it, "--seconds")?.max(1) as u64,
            "--stream" => {
                o.escape = match it.next().map(String::as_str) {
                    Some("text") => false,
                    Some("escape") => true,
                    _ => return Err("--stream is text or escape".into()),
                }
            }
            "--json" => json_out = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(measure(&o))?;
    let report = json!({
        "workload": "tiers", "panes": o.panes,
        "stream": if o.escape { "escape" } else { "text" },
        "rate_kib": o.rate, "seconds": o.seconds,
        "hardware": crate::hardware(), "revision": crate::revision(), "results": result,
    });
    crate::print_table(&report);
    if let Some(p) = json_out {
        crate::write_json(&p, &report)?;
    }
    Ok(())
}

/// The shell command a busy pane runs.
fn producer(o: &Options) -> String {
    let line = if o.escape { ESCAPE_LINE } else { TEXT_LINE };
    let printf = line.replace('\x1b', "\\033");
    if o.rate == 0 {
        return format!("l=$(printf '{printf}'); exec yes \"$l\"");
    }
    let lines = (o.rate * 1024 / BURSTS / (line.len() + 1)).max(1);
    format!(
        "l=$(printf '{printf}'); while :; do yes \"$l\" | head -n {lines}; sleep {}; done",
        1.0 / BURSTS as f64
    )
}

/// `n` new windows running `command`, by pane id.
fn windows(rig: &Rig, n: usize, command: &str) -> Result<Vec<u32>, String> {
    let mut panes = Vec::with_capacity(n);
    while panes.len() < n {
        let k = (n - panes.len()).min(16);
        let mut args: Vec<&str> = Vec::new();
        for i in 0..k {
            if i > 0 {
                args.push(";");
            }
            args.extend(["new-window", "-d", "-P", "-F", "#{pane_id}", command]);
        }
        let out = rig.tmux(&args)?;
        panes.extend(
            out.lines()
                .filter_map(|l| l.trim().strip_prefix('%')?.parse::<u32>().ok()),
        );
    }
    Ok(panes)
}

/// One sample: the daemon's, tmux's, and this process's CPU over the
/// window, the daemon's heap at its end, and the output the given panes'
/// mirrors took in meanwhile.
#[derive(Clone, Copy)]
struct Sample {
    daemon: f64,
    tmux: f64,
    replicas: f64,
    heap_kib: f64,
    /// MiB a second into mirrors; `None` when no pane had one.
    ingested: Option<f64>,
}

async fn ingested(c: &Client, panes: &[u32]) -> Result<Option<u64>, String> {
    let m = mirrors(c).await?;
    let live: Vec<u64> = m
        .iter()
        .filter(|m| panes.contains(&m.pane) && m.state == api::MirrorState::Live as i32)
        .map(|m| m.ingested_bytes)
        .collect();
    Ok((!live.is_empty()).then(|| live.iter().sum()))
}

async fn sample(
    c: &Client,
    pids: (u32, u32),
    panes: &[u32],
    over: Duration,
) -> Result<Sample, String> {
    let me = std::process::id();
    let before = ingested(c, panes).await?;
    let a = (cpu_seconds(pids.0)?, cpu_seconds(pids.1)?, cpu_seconds(me)?);
    let t = Instant::now();
    tokio::time::sleep(over).await;
    let b = (cpu_seconds(pids.0)?, cpu_seconds(pids.1)?, cpu_seconds(me)?);
    let secs = t.elapsed().as_secs_f64();
    let after = ingested(c, panes).await?;
    let pct = |a: f64, b: f64| (b - a) / secs * 100.0;
    Ok(Sample {
        daemon: pct(a.0, b.0),
        tmux: pct(a.1, b.1),
        replicas: pct(a.2, b.2),
        heap_kib: heap_kib(c).await?,
        ingested: match (before, after) {
            (Some(x), Some(y)) => Some(y.saturating_sub(x) as f64 / secs / 1048576.0),
            _ => None,
        },
    })
}

async fn set_tier(c: &Client, pane: u32, tier: api::Tier) -> Result<(), String> {
    c.call(Request::TerminalSetTier(api::SetTier {
        pane,
        tier: tier as i32,
        endpoint: None,
    }))
    .await
    .map(|_| ())
    .map_err(|e| format!("pane %{pane} to {tier:?}: {e}"))
}

/// Follow every pane with a client replica, as a desktop view does, and
/// mark it Hot. A Cold pane is attached again by the subscription.
async fn follow(
    rig: &Rig,
    c: &Client,
    panes: &[u32],
) -> Result<Vec<Arc<replica::feed::Feed>>, String> {
    let mut feeds = Vec::with_capacity(panes.len());
    for p in panes {
        feeds.push(Arc::new(
            replica::feed::Feed::start(rig.socket(), Some(*p), None).map_err(|e| e.to_string())?,
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    for (f, p) in feeds.iter().zip(panes) {
        crate::next_frame(f, deadline)
            .await
            .ok_or(format!("pane %{p} never showed a frame"))?;
    }
    for p in panes {
        set_tier(c, *p, api::Tier::Hot).await?;
    }
    // Take every frame on its wake, as a desktop view does; a frame
    // nobody takes is never presented.
    for f in &feeds {
        let wake = f.wake.clone();
        let f = Arc::downgrade(f);
        tokio::spawn(async move {
            while wake.recv().await.is_ok() {
                let Some(f) = f.upgrade() else { return };
                drop(f.take());
            }
        });
    }
    Ok(feeds)
}

/// Frames the feeds built and wakeups their owners took, so far.
fn feed_counts(feeds: &[Arc<replica::feed::Feed>]) -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    feeds.iter().fold((0, 0), |(f, w), feed| {
        (
            f + feed.stats.frames.load(Relaxed),
            w + feed.stats.owner_wakeups.load(Relaxed),
        )
    })
}

/// Demote every pane to Cold. A follower that has just gone may still be
/// counted for a moment, so a refusal is retried briefly.
async fn demote(c: &Client, panes: &[u32]) -> Result<(), String> {
    for p in panes {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match set_tier(c, *p, api::Tier::Cold).await {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Wait until every one of `panes` has a live mirror.
async fn wait_panes_live(c: &Client, panes: &[u32], limit: Duration) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    loop {
        let m = mirrors(c).await?;
        let live = m
            .iter()
            .filter(|m| panes.contains(&m.pane) && m.state == api::MirrorState::Live as i32)
            .count();
        if live == panes.len() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "only {live} of {} new panes became live",
                panes.len()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn measure(o: &Options) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut put = |measure: &str, value: f64, unit: &str| {
        eprintln!("bench: {measure}: {value:.2} {unit}");
        out.push(json!({ "measure": measure, "value": value, "unit": unit }));
    };
    let n = o.panes;
    let over = Duration::from_secs(o.seconds);
    let settle = Duration::from_secs(3);
    let rig = Rig::start()?;
    let c = rig.client().await?;
    wait_live(&c, 1, Duration::from_secs(15)).await?;
    let pid = rig.pid()?;
    let tmux_pid: u32 = rig
        .tmux(&["display-message", "-p", "#{pid}"])?
        .trim()
        .parse()
        .map_err(|_| "no tmux pid")?;
    let pids = (pid, tmux_pid);
    tokio::time::sleep(settle).await;
    let base = sample(&c, pids, &[], over).await?;
    put("daemon CPU, 1 idle pane", base.daemon, "% of a core");
    put("tmux CPU, 1 idle pane", base.tmux, "% of a core");

    // Per pane, against a baseline taken before the panes existed.
    let per_pane = |s: &Sample, b: &Sample| {
        (
            (s.daemon - b.daemon) / n as f64,
            (s.heap_kib - b.heap_kib).max(0.0) / n as f64,
        )
    };

    for busy in [false, true] {
        let what = if busy { "busy" } else { "idle" };
        // The idle panes start from the one-pane sample; the busy ones
        // from a new one, taken once the idle panes are gone.
        let baseline = if busy {
            tokio::time::sleep(settle).await;
            sample(&c, pids, &[], over).await?
        } else {
            sample(&c, pids, &[], Duration::ZERO)
                .await
                .map(|s| Sample {
                    heap_kib: s.heap_kib,
                    ..base
                })?
        };
        let command = if busy { producer(o) } else { "/bin/sh".into() };
        let panes = windows(&rig, n, &command)?;
        wait_panes_live(&c, &panes, Duration::from_secs(120)).await?;
        tokio::time::sleep(settle).await;

        let mut cpu = Vec::new();
        for tier in ["Warm", "Cold", "Hot"] {
            let feeds = match tier {
                "Cold" => {
                    demote(&c, &panes).await?;
                    tokio::time::sleep(settle).await;
                    None
                }
                "Hot" => {
                    let rss = rss_kib(std::process::id())?;
                    let f = follow(&rig, &c, &panes).await?;
                    tokio::time::sleep(settle).await;
                    Some((f, rss))
                }
                _ => None,
            };
            let counted = feeds
                .as_ref()
                .map(|(f, _)| (feed_counts(f), Instant::now()));
            let s = sample(&c, pids, &panes, over).await?;
            let tag = format!("{n} {what} {tier} panes");
            put(&format!("daemon CPU, {tag}"), s.daemon, "% of a core");
            put(&format!("tmux CPU, {tag}"), s.tmux, "% of a core");
            let (cpu_pane, heap_pane) = per_pane(&s, &baseline);
            put(
                &format!("daemon CPU per {what} {tier} pane"),
                cpu_pane,
                "% of a core",
            );
            put(&format!("heap per {what} {tier} pane"), heap_pane, "KiB");
            if busy && let Some(m) = s.ingested {
                put(&format!("output into mirrors, {tag}"), m, "MiB/s");
            }
            if let Some((feeds, rss)) = feeds {
                put(
                    &format!("client replicas CPU, {tag}"),
                    s.replicas,
                    "% of a core",
                );
                if let Some(((frames, wakeups), t)) = counted {
                    let (f, w) = feed_counts(&feeds);
                    let secs = t.elapsed().as_secs_f64();
                    put(
                        &format!("frames built per Hot replica, {tag}"),
                        (f - frames) as f64 / secs / n as f64,
                        "per second",
                    );
                    put(
                        &format!("owner wakeups per Hot replica, {tag}"),
                        (w - wakeups) as f64 / secs / n as f64,
                        "per second",
                    );
                }
                if busy && let Some(m) = s.ingested.filter(|m| *m > 0.0) {
                    put(
                        &format!("client replicas CPU per MiB/s, {tag}"),
                        s.replicas / m,
                        "% of a core",
                    );
                }
                let grown = rss_kib(std::process::id())?.saturating_sub(rss);
                put(
                    &format!("client replica RSS per {what} Hot pane"),
                    grown as f64 / n as f64,
                    "KiB",
                );
                drop(feeds);
            }
            cpu.push(cpu_pane);
        }
        if busy && cpu[0] > 0.0 {
            put(
                "busy Cold pane's daemon CPU against a busy Warm pane's",
                cpu[1] / cpu[0] * 100.0,
                "%",
            );
        }
        if !busy {
            let ids: Vec<String> = panes.iter().map(|p| format!("%{p}")).collect();
            for id in &ids {
                rig.tmux(&["kill-pane", "-t", id])?;
            }
        }
    }

    // tmux alone with the same busy panes: its own share of the cost.
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
    tokio::time::sleep(settle).await;
    let a = cpu_seconds(tmux_pid)?;
    let t = Instant::now();
    tokio::time::sleep(over).await;
    let tmux = (cpu_seconds(tmux_pid)? - a) / t.elapsed().as_secs_f64() * 100.0;
    put(
        &format!("tmux CPU, {n} busy panes, daemon stopped"),
        tmux,
        "% of a core",
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(escape: bool, rate: usize) -> Options {
        Options {
            panes: 1,
            escape,
            rate,
            seconds: 1,
        }
    }

    #[test]
    fn the_escape_stream_reaches_the_shell_as_octal_escapes() {
        let p = producer(&options(true, 0));
        assert!(!p.contains('\x1b'));
        assert!(!p.contains('%'), "printf would read a % as a conversion");
        assert!(p.starts_with("l=$(printf '\\033[1;31mbusy\\033[0m "));
    }
}
