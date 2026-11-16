//! `dreamer-bench`: the headless workloads of spec 16.5 and 16.6, run
//! against a real tmux server and a release `dreamerd` with private
//! directories (never the user's). Run it through `cargo xtask bench`,
//! which builds it in release mode with the daemon.
//!
//! ```text
//! dreamer-bench headless [--panes N] [--json FILE]
//! ```
//!
//! Each measure says what it timed. Results are printed as a table and,
//! with `--json`, written for the record together with the machine they
//! ran on.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use serde_json::{Value, json};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("headless") => headless(&args[1..]),
        _ => Err("usage: dreamer-bench headless [--panes N] [--json FILE]".into()),
    };
    if let Err(e) = r {
        eprintln!("dreamer-bench: {e}");
        std::process::exit(1);
    }
}

/// An isolated tmux server and daemon.
struct Rig {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Rig {
    fn start() -> Result<Rig, String> {
        let root = PathBuf::from(format!("/tmp/drm-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let tmux = TmuxServer::start(120, 40, "/bin/sh").map_err(|e| format!("tmux: {e}"))?;
        // Plenty of windows; keep each shell's history short so memory is
        // the daemon's, not scrollback held for nothing.
        tmux.run_checked(&["set-option", "-g", "history-limit", "2000"])
            .map_err(|e| e.to_string())?;
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
        let child = Command::new(&daemon)
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
            .map_err(|e| format!("{}: {e}", daemon.display()))?;
        Ok(Rig {
            root,
            tmux,
            daemon: child,
        })
    }

    fn socket(&self) -> PathBuf {
        self.root.join("rt/dreamerd.sock")
    }

    async fn client(&self) -> Result<Client, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match Client::connect(&self.socket(), "bench").await {
                Ok(c) => return Ok(c),
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(format!("the daemon never answered: {e}")),
            }
        }
    }

    /// The daemon's process id, from its lock file.
    fn pid(&self) -> Result<u32, String> {
        std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| "no daemon pid".into())
    }

    fn tmux(&self, args: &[&str]) -> Result<String, String> {
        self.tmux
            .run_checked(args)
            .map_err(|e| format!("tmux {args:?}: {e}"))
    }

    /// `n` more windows, each running `command`, in batches of one tmux
    /// invocation each.
    fn windows(&self, n: usize, command: &str) -> Result<(), String> {
        let mut left = n;
        while left > 0 {
            let k = left.min(16);
            let mut args: Vec<&str> = Vec::new();
            for i in 0..k {
                if i > 0 {
                    args.push(";");
                }
                args.extend(["new-window", "-d", command]);
            }
            self.tmux(&args)?;
            left -= k;
        }
        Ok(())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Ok(pid) = self.pid()
            && platform::process_alive(pid)
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Resident memory of a process, in KiB.
fn rss_kib(pid: u32) -> Result<u64, String> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| format!("no rss for {pid}"))
}

/// CPU time a process has used, in seconds.
fn cpu_seconds(pid: u32) -> Result<f64, String> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Fields after the command's closing parenthesis; utime and stime
        // are the 14th and 15th fields overall, in clock ticks (100 a
        // second on Linux).
        let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        return Ok((ticks(11) + ticks(12)) / 100.0);
    }
    // macOS: [[dd-]hh:]mm:ss.cc
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let (days, t) = t.split_once('-').map_or((0.0, t.as_str()), |(d, r)| {
        (d.parse::<f64>().unwrap_or(0.0), r)
    });
    let secs = t
        .split(':')
        .try_fold(0.0, |acc, p| p.parse::<f64>().map(|v| acc * 60.0 + v))
        .map_err(|_| format!("cannot read cpu time `{t}`"))?;
    Ok(days * 86400.0 + secs)
}

async fn mirrors(c: &Client) -> Result<Vec<api::MirrorStatus>, String> {
    match c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
    {
        Ok(Response::TerminalMirrors(l)) => Ok(l.mirrors),
        Ok(_) => Err("unexpected reply".into()),
        Err(e) => Err(e.to_string()),
    }
}

async fn wait_live(c: &Client, n: usize, limit: Duration) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    loop {
        let m = mirrors(c).await?;
        let live = m
            .iter()
            .filter(|m| m.state == api::MirrorState::Live as i32)
            .count();
        if live >= n {
            return Ok(());
        }
        if Instant::now() > deadline {
            let mut states = std::collections::BTreeMap::<i32, usize>::new();
            for x in &m {
                *states.entry(x.state).or_default() += 1;
            }
            let why: Vec<&str> = m
                .iter()
                .filter_map(|x| x.detail.as_deref())
                .take(3)
                .collect();
            return Err(format!(
                "only {live} of {n} panes became live; {} mirrored, by state {states:?}; {why:?}",
                m.len()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Percent of one core the process used over `over`.
async fn idle_cpu(pid: u32, over: Duration) -> Result<f64, String> {
    let a = cpu_seconds(pid)?;
    tokio::time::sleep(over).await;
    let b = cpu_seconds(pid)?;
    Ok((b - a) / over.as_secs_f64() * 100.0)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The next frame from a replica, or `None` at the deadline.
async fn next_frame(
    feed: &replica::feed::Feed,
    deadline: Instant,
) -> Option<std::sync::Arc<terminal_ghostty::core::FrameSnapshot>> {
    loop {
        if let Some(f) = feed.take() {
            return Some(f);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || tokio::time::timeout(left, feed.wake.recv()).await.is_err() {
            return None;
        }
    }
}

/// Key to visible echo through the whole path: input sent from a client
/// replica, the daemon encoding it, tmux delivering it, the pty echoing
/// it (`cat` in a fresh window), tmux sending the echo, the daemon's
/// mirror, and the replica's frame. Each sample waits for the cursor to
/// move past the typed character.
async fn key_to_echo(rig: &Rig, c: &Client, samples: usize) -> Result<Vec<f64>, String> {
    let before: Vec<u32> = mirrors(c).await?.iter().map(|m| m.pane).collect();
    rig.windows(1, "cat")?;
    let pane = loop {
        let now = mirrors(c).await?;
        if let Some(m) = now
            .iter()
            .find(|m| !before.contains(&m.pane) && m.state == api::MirrorState::Live as i32)
        {
            break m.pane;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let feed =
        replica::feed::Feed::start(rig.socket(), Some(pane), None).map_err(|e| e.to_string())?;
    // The first frame.
    let first = next_frame(&feed, Instant::now() + Duration::from_secs(10))
        .await
        .ok_or("the pane never showed a frame")?;
    let mut cursor = (first.cursor.x, first.cursor.y);
    let mut out = Vec::with_capacity(samples);
    for i in 0..samples {
        // A fresh line every 40 characters, untimed.
        if i > 0 && i % 40 == 0 {
            feed.send(api::terminal_input::Event::Text(api::TextInput {
                text: "\r".into(),
            }));
            let deadline = Instant::now() + Duration::from_secs(5);
            while let Some(f) = next_frame(&feed, deadline).await {
                cursor = (f.cursor.x, f.cursor.y);
                if f.cursor.x == 0 && f.text_dump().lines().count() > 0 {
                    // cat printed the line back; wait for that too.
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    if let Some(f) = feed.take() {
                        cursor = (f.cursor.x, f.cursor.y);
                    }
                    break;
                }
            }
        }
        let sent = Instant::now();
        feed.send(api::terminal_input::Event::Text(api::TextInput {
            text: "k".into(),
        }));
        let deadline = sent + Duration::from_secs(5);
        loop {
            let Some(f) = next_frame(&feed, deadline).await else {
                return Err(format!("no echo for keystroke {i}"));
            };
            let now = (f.cursor.x, f.cursor.y);
            if now != cursor {
                out.push(ms(sent.elapsed()));
                cursor = now;
                break;
            }
        }
    }
    out.sort_by(f64::total_cmp);
    Ok(out)
}

fn hardware() -> Value {
    let sh = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let (model, cpu, mem) = if cfg!(target_os = "macos") {
        (
            sh("sysctl", &["-n", "hw.model"]),
            sh("sysctl", &["-n", "machdep.cpu.brand_string"]),
            sh("sysctl", &["-n", "hw.memsize"]),
        )
    } else {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu = cpuinfo
            .lines()
            .find_map(|l| {
                l.strip_prefix("model name")
                    .map(|r| r.trim_start_matches([' ', '\t', ':']).to_owned())
            })
            .unwrap_or_default();
        let mem = std::fs::read_to_string("/proc/meminfo")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:").map(|r| r.trim().to_owned()))
            .unwrap_or_default();
        (String::new(), cpu, mem)
    };
    json!({
        "model": model,
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "memory": mem,
        "os": sh("uname", &["-srm"]),
    })
}

fn headless(args: &[String]) -> Result<(), String> {
    let mut panes = 1000usize;
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--panes" => {
                panes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--panes needs a number")?;
            }
            "--json" => json_out = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(run_headless(panes))?;
    let report = json!({ "workload": "headless", "panes": panes, "hardware": hardware(), "results": result });
    print_table(&report);
    if let Some(p) = json_out {
        write_json(&p, &report)?;
    }
    Ok(())
}

fn write_json(p: &Path, v: &Value) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", p.display()))
}

fn print_table(report: &Value) {
    println!("machine: {}", report["hardware"]);
    println!("{:<52} {:>12}  unit", "measure", "value");
    if let Some(rows) = report["results"].as_array() {
        for r in rows {
            println!(
                "{:<52} {:>12.2}  {}",
                r["measure"].as_str().unwrap_or(""),
                r["value"].as_f64().unwrap_or(f64::NAN),
                r["unit"].as_str().unwrap_or("")
            );
        }
    }
}

async fn run_headless(panes: usize) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut put = |measure: &str, value: f64, unit: &str| {
        eprintln!("bench: {measure}: {value:.2} {unit}");
        out.push(json!({ "measure": measure, "value": value, "unit": unit }));
    };
    let rig = Rig::start()?;
    let c = rig.client().await?;
    wait_live(&c, 1, Duration::from_secs(15)).await?;
    let pid = rig.pid()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let base = rss_kib(pid)?;
    put("daemon RSS, 1 pane", base as f64 / 1024.0, "MiB");

    // The one-pane typing budget, before any load.
    let echo = key_to_echo(&rig, &c, 200).await?;
    put("key to echo, 1 pane, p50", percentile(&echo, 0.5), "ms");
    put("key to echo, 1 pane, p95", percentile(&echo, 0.95), "ms");

    // N idle panes, all Warm.
    let started = Instant::now();
    rig.windows(panes.saturating_sub(2), "/bin/sh")?;
    wait_live(&c, panes, Duration::from_secs(600)).await?;
    put(
        &format!("mirror {panes} panes from creation"),
        started.elapsed().as_secs_f64(),
        "s",
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    let warm = rss_kib(pid)?;
    put(
        &format!("daemon RSS, {panes} Warm panes"),
        warm as f64 / 1024.0,
        "MiB",
    );
    put(
        "memory per Warm pane",
        (warm.saturating_sub(base)) as f64 / panes.max(1) as f64,
        "KiB",
    );
    put(
        &format!("daemon CPU, {panes} idle Warm panes"),
        idle_cpu(pid, Duration::from_secs(10)).await?,
        "% of a core",
    );

    // All of them Cold (nothing follows them).
    let all: Vec<u32> = mirrors(&c).await?.iter().map(|m| m.pane).collect();
    let started = Instant::now();
    let mut cold = Vec::new();
    for p in &all {
        let r = c
            .call(Request::TerminalSetTier(api::SetTier {
                pane: *p,
                tier: api::Tier::Cold as i32,
                endpoint: None,
            }))
            .await;
        if r.is_ok() {
            cold.push(*p);
        }
    }
    put(
        &format!("demote {} panes to Cold", cold.len()),
        started.elapsed().as_secs_f64(),
        "s",
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    let colder = rss_kib(pid)?;
    put(
        &format!("daemon RSS, {} Cold panes", cold.len()),
        colder as f64 / 1024.0,
        "MiB",
    );
    put(
        "memory per Cold pane (RSS after demotion; the allocator may keep freed pages)",
        (colder.saturating_sub(base)) as f64 / cold.len().max(1) as f64,
        "KiB",
    );
    put(
        &format!("daemon CPU, {} idle Cold panes", cold.len()),
        idle_cpu(pid, Duration::from_secs(10)).await?,
        "% of a core",
    );

    // Opening a Cold pane: a read that attaches it again.
    let mut opens = Vec::new();
    for p in cold.iter().take(100) {
        let t = Instant::now();
        let r = c
            .call(Request::TerminalRead(api::ReadParams {
                pane: *p,
                history: Some(10),
                endpoint: None,
            }))
            .await;
        if r.is_ok() {
            opens.push(ms(t.elapsed()));
        }
    }
    opens.sort_by(f64::total_cmp);
    put(
        "open a Cold pane (read), p50",
        percentile(&opens, 0.5),
        "ms",
    );
    put(
        "open a Cold pane (read), p95",
        percentile(&opens, 0.95),
        "ms",
    );

    // Typing while 63 other panes produce heavy output.
    rig.windows(
        63,
        "yes 'busy output for the benchmark, one line after another'",
    )?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let echo = key_to_echo(&rig, &c, 200).await?;
    put(
        "key to echo, 63 busy panes, p50",
        percentile(&echo, 0.5),
        "ms",
    );
    put(
        "key to echo, 63 busy panes, p95",
        percentile(&echo, 0.95),
        "ms",
    );
    put(
        "daemon CPU, 63 busy panes",
        idle_cpu(pid, Duration::from_secs(5)).await?,
        "% of a core",
    );
    Ok(out)
}
