//! `compare`: Dreamer next to other terminals on one machine, drawing the
//! same tmux session. Linux only for now.
//!
//! ```text
//! dreamer-bench compare [--terminal NAME]... [--frames off|on|always] [--seconds S] [--json FILE]
//! ```
//!
//! Every terminal draws the same isolated tmux session (its own socket,
//! no tmux configuration), so the work behind the window is identical:
//!
//! - **idle:** one pane at a shell prompt; memory after 6 s.
//! - **cat:** one pane; the wall time of `cat` of a 64 MiB text file in
//!   tmux, timed by the shell, and memory after it.
//! - **busy:** 16 tiled panes, 15 of them `cat` the file in a loop; the
//!   terminal's CPU (percent of one core) and peak memory over `S`
//!   seconds (20 by default), and the tmux server's CPU alongside.
//!
//! Memory is PSS (`/proc/PID/smaps_rollup`) summed over the terminal's
//! own processes. Dreamer is split into the desktop (`dreamer`) and the
//! daemon (`dreamerd`), which runs with private directories and the
//! session's socket. Each phase uses a tmux server of its own: reusing a
//! socket right after `kill-server` races the old server's shutdown.
//!
//! The other terminals (Ghostty, Alacritty, kitty, foot) run with their
//! built-in defaults, no user configuration; one that is not installed is
//! reported as skipped. Dreamer runs with its default settings and, with
//! `--frames`, `DREAMER_RENDER_FRAMES` set to that value, which the
//! results record.
//!
//! Whether each window had focus is recorded when the session is
//! Hyprland (asked through `hyprctl`; each window then opens alone on an
//! empty workspace, and the previous one is shown again at the end), and
//! is unknown elsewhere. Run from the machine's own desktop session; over
//! SSH, the logged-in session's display is found through
//! `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`, and
//! `HYPRLAND_INSTANCE_SIGNATURE`.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{cpu_seconds, hardware, revision, write_json};

/// The terminals compared, in order: name, the command that runs a
/// program in a new window with built-in defaults (the tmux client is
/// appended), and the process names counted as the terminal.
const TERMINALS: &[(&str, &[&str], &[&str])] = &[
    ("dreamer", &[], &["dreamer", "dreamerd"]),
    (
        "ghostty",
        &[
            "ghostty",
            "--config-default-files=false",
            "--gtk-single-instance=false",
            "-e",
        ],
        &["ghostty"],
    ),
    (
        "alacritty",
        &["alacritty", "--config-file", "/dev/null", "-e"],
        &["alacritty"],
    ),
    ("kitty", &["kitty", "--config", "NONE"], &["kitty"]),
    ("foot", &["foot", "-c", "/dev/null"], &["foot"]),
];

/// Dreamer's parts, by process name.
const DREAMER_PARTS: &[(&str, &str)] = &[("desktop", "dreamer"), ("daemon", "dreamerd")];

const CORPUS_BYTES: usize = 64 << 20;
const BUSY_PANES: usize = 16;

#[derive(Debug, PartialEq)]
struct Options {
    terminals: Vec<String>,
    frames: Option<String>,
    seconds: u64,
    json: Option<PathBuf>,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        terminals: Vec::new(),
        frames: None,
        seconds: 20,
        json: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--terminal" => {
                let t = it.next().map(String::as_str).unwrap_or("");
                if !TERMINALS.iter().any(|(n, ..)| *n == t) {
                    let names: Vec<&str> = TERMINALS.iter().map(|(n, ..)| *n).collect();
                    return Err(format!("--terminal is one of {}", names.join(", ")));
                }
                o.terminals.push(t.to_owned());
            }
            "--frames" => match it.next().map(String::as_str) {
                Some(f @ ("off" | "on" | "always")) => o.frames = Some(f.to_owned()),
                _ => return Err("--frames is off, on, or always".into()),
            },
            "--seconds" => {
                o.seconds = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|s| *s >= 1)
                    .ok_or("--seconds needs a number of at least 1")?;
            }
            "--json" => o.json = Some(PathBuf::from(it.next().ok_or("--json needs a file")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if o.terminals.is_empty() {
        o.terminals = TERMINALS.iter().map(|(n, ..)| (*n).to_owned()).collect();
    }
    Ok(o)
}

/// Memory and CPU of a terminal, or of one of Dreamer's parts.
#[derive(Debug, Default, Clone, PartialEq)]
struct Usage {
    idle_mib: f64,
    busy_cpu: f64,
    busy_peak_mib: f64,
}

/// One terminal's results, or why it was skipped.
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    terminal: String,
    skipped: Option<String>,
    processes: Vec<String>,
    /// Whether the window had focus in the idle and busy phases; `None`
    /// when that cannot be asked.
    focused: Option<bool>,
    busy_focused: Option<bool>,
    cat_seconds: Option<f64>,
    cat_mib: f64,
    tmux_cpu: f64,
    total: Usage,
    /// Dreamer's desktop and daemon.
    parts: Vec<(String, Usage)>,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let o = options(args)?;
    if !cfg!(target_os = "linux") {
        return Err("compare is Linux-only for now".into());
    }
    let env = session_env();
    let frames = o
        .frames
        .clone()
        .or_else(|| std::env::var("DREAMER_RENDER_FRAMES").ok());
    let bin = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or("no directory for dreamer-bench")?;
    for exe in ["dreamer", "dreamerd"] {
        if !bin.join(exe).is_file() {
            return Err(format!(
                "{} is missing; run `cargo xtask bench compare`",
                bin.join(exe).display()
            ));
        }
    }
    let tmux = which(&super::tmux_program().to_string_lossy())
        .ok_or("tmux is not installed (or DREAMER_TMUX does not name it)")?;
    let dir = PathBuf::from(format!("/tmp/drm-cmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let corpus = dir.join("corpus.txt");
    write_corpus(&corpus)?;
    let rig = Compare {
        dir: dir.clone(),
        corpus,
        bin,
        tmux,
        env,
        frames: frames.clone(),
        seconds: o.seconds,
    };

    let hypr = Hyprland::new(&rig.env);
    // An empty workspace: each window opens alone there and takes focus.
    let back = hypr.as_ref().map(|h| h.workspace());
    if let Some(h) = &hypr {
        h.dispatch("hl.dsp.focus({ workspace = 9 })");
    }
    let mut outcomes = Vec::new();
    let mut failed = None;
    for name in &o.terminals {
        let (_, argv, _) = TERMINALS
            .iter()
            .find(|(n, ..)| n == name)
            .ok_or("no such terminal")?;
        if name != "dreamer" && which(argv[0]).is_none() {
            println!("{name}: not installed, skipped");
            outcomes.push(Outcome {
                terminal: name.clone(),
                skipped: Some("not installed".into()),
                ..Outcome::default()
            });
            continue;
        }
        println!("=== {name}");
        match rig.terminal(name, hypr.as_ref()) {
            Ok(r) => outcomes.push(r),
            Err(e) => {
                failed = Some(format!("{name}: {e}"));
                break;
            }
        }
    }
    if let (Some(h), Some(b)) = (&hypr, back) {
        h.dispatch(&format!("hl.dsp.focus({{ workspace = {b} }})"));
    }
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(e) = failed {
        return Err(e);
    }

    let display = json!({
        "wayland": rig.env_var("WAYLAND_DISPLAY"),
        "x11": rig.env_var("DISPLAY"),
        "hyprland": hypr.is_some(),
    });
    let report = json!({
        "workload": "compare",
        "frames": frames,
        "seconds": o.seconds,
        "corpus_bytes": CORPUS_BYTES,
        "busy_panes": BUSY_PANES - 1,
        "display": display,
        "hardware": hardware(),
        "revision": revision(),
        "terminals": outcomes.iter().map(outcome_json).collect::<Vec<_>>(),
        "results": measures(&outcomes),
    });
    println!("machine: {}", report["hardware"]);
    println!("revision: {}", report["revision"]);
    println!(
        "Dreamer frames: {}",
        frames.as_deref().unwrap_or("default setting")
    );
    print!("{}", table(&outcomes));
    if let Some(p) = o.json {
        write_json(&p, &report)?;
    }
    Ok(())
}

/// What every phase shares.
struct Compare {
    dir: PathBuf,
    corpus: PathBuf,
    bin: PathBuf,
    tmux: PathBuf,
    /// The desktop session's variables, for everything started.
    env: Vec<(String, String)>,
    frames: Option<String>,
    seconds: u64,
}

/// A terminal's window, running.
struct Running {
    comms: &'static [&'static str],
    window: Child,
    daemon: Option<Child>,
    /// Dreamer's runtime directory, for the daemon's lock file.
    runtime: Option<PathBuf>,
}

impl Compare {
    fn env_var(&self, name: &str) -> Option<String> {
        self.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
    }

    fn tmux(&self, socket: &str, args: &[&str]) -> String {
        Command::new(&self.tmux)
            .args(["-L", socket, "-f", "/dev/null"])
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    }

    /// A session `w` with `panes` tiled panes, the first of `busy` + 1
    /// of them looping `cat` of the corpus. The panes' ids.
    fn session(&self, socket: &str, panes: usize, busy: usize) -> Vec<String> {
        self.tmux(socket, &["kill-server"]);
        self.tmux(
            socket,
            &["new-session", "-d", "-s", "w", "-x", "200", "-y", "50"],
        );
        self.tmux(socket, &["set-option", "-g", "history-limit", "2000"]);
        for _ in 1..panes {
            self.tmux(socket, &["split-window", "-t", "w"]);
            self.tmux(socket, &["select-layout", "-t", "w", "tiled"]);
        }
        let ids: Vec<String> = self
            .tmux(socket, &["list-panes", "-t", "w", "-F", "#{pane_id}"])
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let line = format!("while :; do cat {}; done", self.corpus.display());
        for p in ids.iter().skip(1).take(busy) {
            self.tmux(socket, &["send-keys", "-t", p, "-l", &line]);
            self.tmux(socket, &["send-keys", "-t", p, "Enter"]);
        }
        ids
    }

    fn start(&self, name: &str, socket: &str) -> Result<Running, String> {
        let (_, argv, comms) = TERMINALS
            .iter()
            .find(|(n, ..)| *n == name)
            .ok_or("no such terminal")?;
        let quiet = |c: &mut Command| {
            c.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        };
        let tmux = self.tmux.to_string_lossy().into_owned();
        if name != "dreamer" {
            let mut c = Command::new(argv[0]);
            c.args(&argv[1..])
                .args([&tmux, "-L", socket, "-f", "/dev/null", "attach", "-t", "w"])
                .envs(self.env.iter().map(|(k, v)| (k, v)))
                .process_group(0);
            quiet(&mut c);
            let window = c.spawn().map_err(|e| format!("{name}: {e}"))?;
            return Ok(Running {
                comms,
                window,
                daemon: None,
                runtime: None,
            });
        }
        // Private directories: never the user's daemon or settings.
        let d = self.dir.join(format!("dreamer-{socket}"));
        for s in ["rt", "st", "desktop", "config"] {
            std::fs::create_dir_all(d.join(s)).map_err(|e| e.to_string())?;
        }
        let dreamer_env = |c: &mut Command| {
            c.envs(self.env.iter().map(|(k, v)| (k, v)))
                .env("DREAMER_RUNTIME_DIR", d.join("rt"))
                .env("DREAMER_STATE_DIR", d.join("st"))
                .env("DREAMER_DESKTOP_STATE_DIR", d.join("desktop"))
                .env("DREAMER_CONFIG_DIR", d.join("config"))
                .env("DREAMER_FORBID_DEFAULT_TMUX", "1");
            match &self.frames {
                Some(f) => c.env("DREAMER_RENDER_FRAMES", f),
                None => c.env_remove("DREAMER_RENDER_FRAMES"),
            };
        };
        let mut c = Command::new(self.bin.join("dreamerd"));
        c.args(["--background", "--tmux-socket", socket, "--tmux", &tmux]);
        dreamer_env(&mut c);
        quiet(&mut c);
        let daemon = c.spawn().map_err(|e| format!("dreamerd: {e}"))?;
        std::thread::sleep(Duration::from_secs(3));
        let mut c = Command::new(self.bin.join("dreamer"));
        c.args(["--workspace", "-L", socket, "--tmux", &tmux])
            .process_group(0);
        dreamer_env(&mut c);
        quiet(&mut c);
        let window = c.spawn().map_err(|e| format!("dreamer: {e}"))?;
        Ok(Running {
            comms,
            window,
            daemon: Some(daemon),
            runtime: Some(d.join("rt")),
        })
    }

    /// [`Compare::start`], stopping the session's tmux server when the
    /// terminal does not start.
    fn started(&self, name: &str, socket: &str) -> Result<Running, String> {
        self.start(name, socket).inspect_err(|_| {
            self.tmux(socket, &["kill-server"]);
        })
    }

    /// The three phases for one terminal.
    fn terminal(&self, name: &str, hypr: Option<&Hyprland>) -> Result<Outcome, String> {
        let mut out = Outcome {
            terminal: name.to_owned(),
            ..Outcome::default()
        };
        let base = format!("drmcmp{}{name}", std::process::id());
        let wake = || {
            if let Some(h) = hypr {
                h.dispatch(r#"hl.dsp.dpms({ action = "on" })"#);
            }
        };
        let focused = |t: &Running| hypr.map(|h| h.active_pid().is_some_and(|p| t.has(p)));

        // Idle.
        let socket = format!("{base}i");
        self.session(&socket, 1, 0);
        wake();
        let t = self.started(name, &socket)?;
        std::thread::sleep(Duration::from_secs(6));
        out.focused = focused(&t);
        let idle = t.pss_by_part();
        out.total.idle_mib = mib(idle.iter().map(|(_, k)| k).sum());
        out.processes = t.processes();
        t.stop();
        self.tmux(&socket, &["kill-server"]);

        // Cat.
        let socket = format!("{base}c");
        let ids = self.session(&socket, 1, 0);
        wake();
        let t = self.started(name, &socket)?;
        std::thread::sleep(Duration::from_secs(4));
        let done = self.dir.join(format!("cat-{name}"));
        let _ = std::fs::remove_file(&done);
        let line = format!(
            "s=$(date +%s.%N); cat {}; e=$(date +%s.%N); echo \"$s $e\" > {}",
            self.corpus.display(),
            done.display()
        );
        if let Some(p) = ids.first() {
            self.tmux(&socket, &["send-keys", "-t", p, "-l", &line]);
            self.tmux(&socket, &["send-keys", "-t", p, "Enter"]);
        }
        let deadline = Instant::now() + Duration::from_secs(300);
        while !done.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(Duration::from_millis(300));
        out.cat_seconds = std::fs::read_to_string(&done)
            .ok()
            .and_then(|s| elapsed(&s));
        out.cat_mib = mib(t.pss_by_part().iter().map(|(_, k)| k).sum());
        t.stop();
        self.tmux(&socket, &["kill-server"]);

        // Busy.
        let socket = format!("{base}b");
        self.session(&socket, BUSY_PANES, BUSY_PANES - 1);
        wake();
        let t = self.started(name, &socket)?;
        std::thread::sleep(Duration::from_secs(5));
        out.busy_focused = focused(&t);
        let server: Option<u32> = self
            .tmux(&socket, &["display-message", "-p", "#{pid}"])
            .parse()
            .ok();
        let before = t.cpu_by_pid();
        let server_before = server.and_then(|p| cpu_seconds(p).ok());
        let t0 = Instant::now();
        let mut peak_total = 0u64;
        let mut peaks: HashMap<String, u64> = HashMap::new();
        while t0.elapsed() < Duration::from_secs(self.seconds) {
            std::thread::sleep(Duration::from_secs(1));
            let now = t.pss_by_part();
            peak_total = peak_total.max(now.iter().map(|(_, k)| k).sum());
            for (part, k) in now {
                let e = peaks.entry(part).or_default();
                *e = (*e).max(k);
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let after = t.cpu_by_pid();
        out.tmux_cpu = match (server, server_before) {
            (Some(p), Some(b)) => percent(cpu_seconds(p).unwrap_or(b) - b, secs),
            _ => f64::NAN,
        };
        let cpu = cpu_delta(&before, &after);
        out.total.busy_cpu = percent(cpu.values().map(|(_, s)| s).sum(), secs);
        out.total.busy_peak_mib = mib(peak_total);
        if name == "dreamer" {
            for (part, comm) in DREAMER_PARTS {
                let busy: f64 = cpu
                    .values()
                    .filter(|(c, _)| c.as_str() == *comm)
                    .map(|(_, s)| s)
                    .sum();
                let idle_kib = idle
                    .iter()
                    .find(|(c, _)| c.as_str() == *comm)
                    .map_or(0, |(_, k)| *k);
                out.parts.push((
                    (*part).to_owned(),
                    Usage {
                        idle_mib: mib(idle_kib),
                        busy_cpu: percent(busy, secs),
                        busy_peak_mib: mib(peaks.get(*comm).copied().unwrap_or(0)),
                    },
                ));
            }
        }
        t.stop();
        self.tmux(&socket, &["kill-server"]);
        Ok(out)
    }
}

impl Running {
    /// The terminal's processes: its window's and daemon's trees, and the
    /// daemon its lock file names, by process name.
    fn pids(&self) -> Vec<u32> {
        let mut roots = vec![self.window.id()];
        roots.extend(self.daemon.as_ref().map(Child::id));
        roots.extend(self.daemon_pid());
        let kids = children();
        let mut out: Vec<u32> = Vec::new();
        for r in roots {
            for p in tree(&kids, r) {
                if !out.contains(&p) && self.comms.contains(&comm(p).as_str()) {
                    out.push(p);
                }
            }
        }
        out.sort_unstable();
        out
    }

    fn daemon_pid(&self) -> Option<u32> {
        let lock = std::fs::read_to_string(self.runtime.as_ref()?.join("dreamerd.lock")).ok()?;
        lock.split_whitespace().next()?.parse().ok()
    }

    fn has(&self, pid: u32) -> bool {
        pid == self.window.id() || self.pids().contains(&pid)
    }

    fn processes(&self) -> Vec<String> {
        let mut names: Vec<String> = self.pids().into_iter().map(comm).collect();
        names.sort();
        names.dedup();
        names
    }

    /// PSS in KiB, summed by process name.
    fn pss_by_part(&self) -> Vec<(String, u64)> {
        let mut by: Vec<(String, u64)> = Vec::new();
        for p in self.pids() {
            let kib = std::fs::read_to_string(format!("/proc/{p}/smaps_rollup"))
                .ok()
                .and_then(|s| pss_kib(&s))
                .unwrap_or(0);
            let c = comm(p);
            match by.iter_mut().find(|(n, _)| *n == c) {
                Some((_, k)) => *k += kib,
                None => by.push((c, kib)),
            }
        }
        by
    }

    /// CPU seconds so far, with the process name, by process id.
    fn cpu_by_pid(&self) -> HashMap<u32, (String, f64)> {
        self.pids()
            .into_iter()
            .filter_map(|p| Some((p, (comm(p), cpu_seconds(p).ok()?))))
            .collect()
    }

    fn stop(mut self) {
        for p in self.pids() {
            let _ = Command::new("kill").arg(p.to_string()).status();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for c in [Some(&mut self.window), self.daemon.as_mut()]
            .into_iter()
            .flatten()
        {
            while c.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(p) = self.daemon_pid()
            && platform::process_alive(p)
        {
            let _ = Command::new("kill").args(["-9", &p.to_string()]).status();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// CPU seconds each process used between two readings; a process that
/// started in between counts from zero.
fn cpu_delta(
    before: &HashMap<u32, (String, f64)>,
    after: &HashMap<u32, (String, f64)>,
) -> HashMap<u32, (String, f64)> {
    after
        .iter()
        .map(|(p, (c, s))| {
            let b = before.get(p).map_or(0.0, |(_, b)| *b);
            (*p, (c.clone(), (s - b).max(0.0)))
        })
        .collect()
}

fn percent(cpu_seconds: f64, wall_seconds: f64) -> f64 {
    100.0 * cpu_seconds / wall_seconds.max(1e-3)
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

/// `Pss:` from `/proc/PID/smaps_rollup`, in KiB.
fn pss_kib(smaps_rollup: &str) -> Option<u64> {
    smaps_rollup.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next() == Some("Pss:"))
            .then(|| f.next()?.parse().ok())
            .flatten()
    })
}

/// The parent process id from `/proc/PID/stat`. The command name may
/// hold spaces and parentheses; the fields after its last `)` do not.
fn stat_ppid(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The shell's `start end` timestamps as seconds between them.
fn elapsed(s: &str) -> Option<f64> {
    let mut f = s.split_whitespace().map(|v| v.parse::<f64>().ok());
    let (a, b) = (f.next()??, f.next()??);
    Some(b - a)
}

/// Every process's children, by parent.
fn children() -> HashMap<u32, Vec<u32>> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(ppid) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| stat_ppid(&s))
        {
            kids.entry(ppid).or_default().push(pid);
        }
    }
    kids
}

fn tree(kids: &HashMap<u32, Vec<u32>>, root: u32) -> Vec<u32> {
    let (mut out, mut todo) = (Vec::new(), vec![root]);
    while let Some(p) = todo.pop() {
        if out.contains(&p) {
            continue;
        }
        out.push(p);
        todo.extend(kids.get(&p).into_iter().flatten());
    }
    out
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// `name` on `PATH`, or itself when it is a path.
fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The plain text the busy panes print: 64 MiB of one ASCII line.
fn write_corpus(file: &Path) -> Result<(), String> {
    const LINE: &str = "The quick brown fox jumps over the lazy dog; \
        0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ {}[]()<>=+-*/ ~!@#$%^&|\n";
    let text = LINE.repeat(CORPUS_BYTES.div_ceil(LINE.len()));
    std::fs::write(file, text).map_err(|e| format!("{}: {e}", file.display()))
}

/// Over SSH, the logged-in desktop session's variables that are not set
/// here: its runtime directory, Wayland display, X display, bus, and
/// Hyprland instance.
fn session_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Uid:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
        })
        .unwrap_or(0u32);
    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let rt = format!("/run/user/{uid}");
        env.push(("XDG_RUNTIME_DIR".into(), rt.clone()));
        rt
    });
    let listing = |dir: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort();
        v
    };
    if !has("WAYLAND_DISPLAY")
        && let Some(w) = listing(&rt)
            .into_iter()
            .find(|f| f.starts_with("wayland-") && !f.ends_with(".lock"))
    {
        env.push(("WAYLAND_DISPLAY".into(), w));
    }
    if !has("DISPLAY") {
        env.push(("DISPLAY".into(), ":0".into()));
    }
    if !has("DBUS_SESSION_BUS_ADDRESS") {
        env.push((
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path={rt}/bus"),
        ));
    }
    if !has("HYPRLAND_INSTANCE_SIGNATURE")
        && let Some(sig) = listing(&format!("{rt}/hypr")).into_iter().next()
    {
        env.push(("HYPRLAND_INSTANCE_SIGNATURE".into(), sig));
    }
    env
}

/// The Hyprland session, when there is one: which window has focus.
struct Hyprland {
    env: Vec<(String, String)>,
}

impl Hyprland {
    fn new(env: &[(String, String)]) -> Option<Hyprland> {
        let signature = env.iter().any(|(k, _)| k == "HYPRLAND_INSTANCE_SIGNATURE")
            || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
        let h = Hyprland { env: env.to_vec() };
        (signature && which("hyprctl").is_some() && !h.ctl(&["version"]).is_empty()).then_some(h)
    }

    fn ctl(&self, args: &[&str]) -> String {
        Command::new("hyprctl")
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    fn dispatch(&self, what: &str) {
        self.ctl(&["dispatch", what]);
    }

    fn json(&self, what: &str) -> Value {
        serde_json::from_str(&self.ctl(&["-j", what])).unwrap_or(Value::Null)
    }

    fn active_pid(&self) -> Option<u32> {
        self.json("activewindow")["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
    }

    fn workspace(&self) -> u64 {
        self.json("activeworkspace")["id"].as_u64().unwrap_or(1)
    }
}

/// The display name of a terminal.
fn title(name: &str) -> &str {
    match name {
        "dreamer" => "Dreamer",
        "ghostty" => "Ghostty",
        "alacritty" => "Alacritty",
        "kitty" => "kitty",
        "foot" => "foot",
        other => other,
    }
}

fn outcome_json(o: &Outcome) -> Value {
    if let Some(why) = &o.skipped {
        return json!({ "terminal": o.terminal, "skipped": why });
    }
    let usage = |u: &Usage| {
        json!({
            "idle_pss_mib": round1(u.idle_mib),
            "busy_cpu_pct": round1(u.busy_cpu),
            "busy_pss_peak_mib": round1(u.busy_peak_mib),
        })
    };
    let mut v = json!({
        "terminal": o.terminal,
        "processes": o.processes,
        "focused": o.focused,
        "busy_focused": o.busy_focused,
        "cat_64mib_s": o.cat_seconds.map(|s| (s * 100.0).round() / 100.0),
        "cat_pss_mib": round1(o.cat_mib),
        "busy_tmux_cpu_pct": round1(o.tmux_cpu),
    });
    if let (Some(m), Value::Object(u)) = (v.as_object_mut(), usage(&o.total)) {
        m.extend(u);
        for (part, u) in &o.parts {
            m.insert(part.clone(), usage(u));
        }
    }
    v
}

fn round1(v: f64) -> Value {
    if v.is_finite() {
        json!((v * 10.0).round() / 10.0)
    } else {
        Value::Null
    }
}

/// The results as the flat measures `bench report` reads: one per
/// terminal (and Dreamer part) and quantity.
fn measures(outcomes: &[Outcome]) -> Vec<Value> {
    let busy = format!("{BUSY_PANES} panes, {} busy", BUSY_PANES - 1);
    let mut out = Vec::new();
    let mut put = |who: &str, what: &str, value: f64, unit: &str| {
        if value.is_finite() {
            out.push(json!({ "measure": format!("{who}, {what}"), "value": value, "unit": unit }));
        }
    };
    for o in outcomes.iter().filter(|o| o.skipped.is_none()) {
        let t = title(&o.terminal);
        put(t, "memory, idle", o.total.idle_mib, "MiB");
        put(t, "cat 64 MiB", o.cat_seconds.unwrap_or(f64::NAN), "s");
        put(t, &format!("CPU, {busy}"), o.total.busy_cpu, "%");
        put(
            t,
            &format!("peak memory, {busy}"),
            o.total.busy_peak_mib,
            "MiB",
        );
        put(t, &format!("tmux CPU, {busy}"), o.tmux_cpu, "%");
        for (part, u) in &o.parts {
            let who = format!("{t} {part}");
            put(&who, "memory, idle", u.idle_mib, "MiB");
            put(&who, &format!("CPU, {busy}"), u.busy_cpu, "%");
            put(
                &who,
                &format!("peak memory, {busy}"),
                u.busy_peak_mib,
                "MiB",
            );
        }
    }
    out
}

/// The comparison as a plain table: a row per terminal, Dreamer's parts
/// indented under it, skipped terminals named.
fn table(outcomes: &[Outcome]) -> String {
    let row = |cells: [&str; 7]| {
        let [t, f, idle, cat, cpu, peak, tmux] = cells;
        let l = format!("{t:<12} {f:>7} {idle:>9} {cat:>8} {cpu:>9} {peak:>9} {tmux:>9}");
        format!("{}\n", l.trim_end())
    };
    let num = |v: f64, digits: usize| {
        if v.is_finite() {
            format!("{v:.digits$}")
        } else {
            "-".into()
        }
    };
    let pct = |v: f64| {
        if v.is_finite() {
            format!("{v:.1}%")
        } else {
            "-".into()
        }
    };
    let focus = |o: &Outcome| match (o.focused, o.busy_focused) {
        (Some(true), Some(true)) => "yes",
        (Some(false), Some(false)) => "no",
        (None, None) => "unknown",
        _ => "partly",
    };
    let mut s = row([
        "terminal", "focus", "idle MiB", "cat s", "busy CPU", "busy MiB", "tmux CPU",
    ]);
    for o in outcomes {
        let t = title(&o.terminal);
        if let Some(why) = &o.skipped {
            s += &format!("{t:<12} {why}\n");
            continue;
        }
        s += &row([
            t,
            focus(o),
            &num(o.total.idle_mib, 1),
            &num(o.cat_seconds.unwrap_or(f64::NAN), 2),
            &pct(o.total.busy_cpu),
            &num(o.total.busy_peak_mib, 1),
            &pct(o.tmux_cpu),
        ]);
        for (part, u) in &o.parts {
            s += &row([
                &format!("  {part}"),
                "",
                &num(u.idle_mib, 1),
                "",
                &pct(u.busy_cpu),
                &num(u.busy_peak_mib, 1),
                "",
            ]);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pss_is_read_from_smaps_rollup_not_rss() {
        let rollup = "55d0c4a00000-7ffd2b9fe000 ---p 00000000 00:00 0  [rollup]\n\
                      Rss:              215040 kB\n\
                      Pss:              123456 kB\n\
                      Pss_Dirty:         99000 kB\n\
                      Pss_Anon:          90000 kB\n";
        assert_eq!(pss_kib(rollup), Some(123456));
        assert_eq!(pss_kib("Rss: 4 kB\n"), None);
    }

    #[test]
    fn the_parent_is_found_past_a_command_name_with_parentheses() {
        // A window title as the command name: spaces and `) ` inside it.
        let stat = "4242 (Web Content) (x) S 4100 4242 4242 0 -1 4194560 812 0 0 0 7 3 0 0";
        assert_eq!(stat_ppid(stat), Some(4100));
        let kids = HashMap::from([(1, vec![2, 3]), (2, vec![4]), (9, vec![10])]);
        let mut t = tree(&kids, 1);
        t.sort_unstable();
        assert_eq!(t, vec![1, 2, 3, 4]);
    }

    #[test]
    fn cpu_counts_new_processes_from_zero_and_never_goes_negative() {
        let before = HashMap::from([(1, ("dreamer".into(), 10.0)), (2, ("dreamerd".into(), 4.0))]);
        let after = HashMap::from([
            (1, ("dreamer".to_owned(), 12.5)),
            // Same pid, reused after a restart: less than before.
            (2, ("dreamerd".to_owned(), 1.0)),
            (3, ("dreamerd".to_owned(), 0.5)),
        ]);
        let d = cpu_delta(&before, &after);
        assert_eq!(d[&1].1, 2.5);
        assert_eq!(d[&2].1, 0.0);
        assert_eq!(d[&3].1, 0.5);
        assert_eq!(percent(3.0, 20.0), 15.0);
    }

    #[test]
    fn cat_time_is_the_shells_two_timestamps() {
        assert_eq!(
            elapsed("1764150000.250000000 1764150002.000000000\n"),
            Some(1.75)
        );
        assert_eq!(elapsed("1764150000.25\n"), None);
    }

    #[test]
    fn options_pick_terminals_and_frames() {
        let args = |a: &str| a.split_whitespace().map(String::from).collect::<Vec<_>>();
        let o = options(&args(
            "--terminal foot --terminal dreamer --frames on --seconds 5",
        ))
        .expect("valid options");
        assert_eq!(o.terminals, ["foot", "dreamer"]);
        assert_eq!(o.frames.as_deref(), Some("on"));
        assert_eq!(o.seconds, 5);
        assert_eq!(
            options(&[]).expect("no options").terminals.len(),
            TERMINALS.len()
        );
        assert!(options(&args("--terminal xterm")).is_err());
        assert!(options(&args("--frames maybe")).is_err());
    }

    fn dreamer() -> Outcome {
        Outcome {
            terminal: "dreamer".into(),
            processes: vec!["dreamer".into(), "dreamerd".into()],
            focused: Some(true),
            busy_focused: Some(true),
            cat_seconds: Some(1.234),
            cat_mib: 300.0,
            tmux_cpu: 88.0,
            total: Usage {
                idle_mib: 140.0,
                busy_cpu: 317.0,
                busy_peak_mib: 475.0,
            },
            parts: vec![
                (
                    "desktop".into(),
                    Usage {
                        idle_mib: 90.0,
                        busy_cpu: 136.0,
                        busy_peak_mib: 210.0,
                    },
                ),
                (
                    "daemon".into(),
                    Usage {
                        idle_mib: 50.0,
                        busy_cpu: 128.0,
                        busy_peak_mib: 190.0,
                    },
                ),
            ],
            ..Outcome::default()
        }
    }

    #[test]
    fn the_table_splits_dreamer_and_names_skipped_terminals() {
        let kitty = Outcome {
            terminal: "kitty".into(),
            skipped: Some("not installed".into()),
            ..Outcome::default()
        };
        let foot = Outcome {
            terminal: "foot".into(),
            tmux_cpu: f64::NAN,
            ..Outcome::default()
        };
        assert_eq!(
            table(&[dreamer(), kitty, foot]),
            "terminal       focus  idle MiB    cat s  busy CPU  busy MiB  tmux CPU\n\
             Dreamer          yes     140.0     1.23    317.0%     475.0     88.0%\n\
             \x20 desktop                 90.0             136.0%     210.0\n\
             \x20 daemon                  50.0             128.0%     190.0\n\
             kitty        not installed\n\
             foot         unknown       0.0        -      0.0%       0.0         -\n"
        );
    }

    #[test]
    fn measures_name_the_terminal_and_part_and_leave_out_skipped_ones() {
        let skipped = Outcome {
            terminal: "ghostty".into(),
            skipped: Some("not installed".into()),
            ..Outcome::default()
        };
        let m = measures(&[dreamer(), skipped]);
        let names: Vec<&str> = m.iter().filter_map(|v| v["measure"].as_str()).collect();
        assert!(names.contains(&"Dreamer, CPU, 16 panes, 15 busy"));
        assert!(names.contains(&"Dreamer daemon, peak memory, 16 panes, 15 busy"));
        assert!(!names.iter().any(|n| n.starts_with("Ghostty")));
        let j = outcome_json(&dreamer());
        assert_eq!(j["desktop"]["busy_cpu_pct"], json!(136.0));
        assert_eq!(j["cat_64mib_s"], json!(1.23));
    }
}
