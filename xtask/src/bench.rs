//! `cargo xtask bench`: the benchmark harness (spec 16.5, 16.6). Builds the
//! daemon and `dreamer-bench` in release mode and runs a workload against
//! a private tmux server and daemon, writing the results with the machine
//! they ran on and the revision they were built from to `target/bench/`.
//!
//! ```text
//! cargo xtask bench headless|busy|streams|tiers [OPTIONS] [--debug] [--linux]
//! cargo xtask bench gui [--panes N]
//! cargo xtask bench report FILE...
//! ```
//!
//! `--linux` runs the same workload in an ARM64 Linux container (the one
//! the gate's `arm64` stage uses), which has the ptys for 1,000 panes:
//! macOS allows 511 for the whole system (`kern.tty.ptmx_max`).
//!
//! `--debug` builds and runs the debug binaries instead: a check that the
//! harness works (CI runs it), never a measurement. Its results say so.
//!
//! `report` prints result files as the Markdown that `docs/performance.md`
//! records, so numbers are copied, not retyped.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

const USAGE: &str =
    "usage: cargo xtask bench headless|busy|streams|tiers [OPTIONS] [--debug] [--linux]
       cargo xtask bench gui [--panes N]
       cargo xtask bench report FILE...";

pub fn run(args: &[String]) -> Result<(), String> {
    let root = crate::util::repo_root();
    let (workload, rest) = match args {
        [w, rest @ ..] if w == "gui" => return gui(&root, rest),
        [w, rest @ ..] if w == "report" => return report(rest),
        [w, rest @ ..] if ["headless", "busy", "streams", "tiers"].contains(&w.as_str()) => {
            (w.as_str(), rest)
        }
        _ => return Err(USAGE.into()),
    };
    if rest.iter().any(|a| a == "--linux") {
        let rest: Vec<String> = rest.iter().filter(|a| *a != "--linux").cloned().collect();
        return linux(&root, workload, &rest);
    }
    let debug = rest.iter().any(|a| a == "--debug");
    let rest: Vec<&String> = rest.iter().filter(|a| *a != "--debug").collect();
    let profile = if debug { "debug" } else { "release" };
    let mut build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    build.args(["build", "--locked"]);
    if !debug {
        build.arg("--release");
    }
    let built = build
        .args(["-p", "dreamer-server", "-p", "bench"])
        .current_dir(&root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !built.success() {
        return Err("building dreamerd and dreamer-bench failed".into());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let out = root.join(format!("target/bench/{workload}-{stamp}.json"));
    let bin = crate::util::cargo_target(&root).join(profile);
    let status = Command::new(bin.join("dreamer-bench"))
        .arg(workload)
        .args(rest)
        .arg("--json")
        .arg(&out)
        .env(
            "DREAMER_BENCH_REVISION",
            revision(&root, profile).to_string(),
        )
        .status()
        .map_err(|e| format!("dreamer-bench: {e}"))?;
    if !status.success() {
        return Err(format!("the {workload} workload failed"));
    }
    println!("results: {}", out.display());
    Ok(())
}

/// The workload in the gate's ARM64 Linux container, with the pinned tmux;
/// results land in this checkout's `target/bench/`.
fn linux(root: &std::path::Path, workload: &str, rest: &[String]) -> Result<(), String> {
    let script = format!(
        "set -e; ulimit -n 65536; export DEBIAN_FRONTEND=noninteractive; \
         apt-get update -qq && apt-get install -y -qq --no-install-recommends \
           build-essential pkg-config curl ca-certificates xz-utils git libclang-dev \
           libevent-dev libncurses-dev bison python3 procps >/dev/null; \
         command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
         . \"$HOME/.cargo/env\"; \
         cargo xtask deps prepare --locked; \
         export DREAMER_TMUX=$(cargo xtask deps tmux | tail -n 1); \
         cargo xtask bench {workload} {}",
        rest.join(" ")
    );
    let private = crate::private_git::docker_args(root)?;
    // The container sees the checkout but not as its owner; the host
    // records which commit it is.
    let git = git_state(root).to_string();
    let status = Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/arm64"])
        .arg("-e")
        .arg(format!("DREAMER_BENCH_GIT={git}"))
        .arg("-v")
        .arg(format!("{}:/src", root.display()))
        .args(&private)
        .args([
            "-v",
            "dreamer-arm64-cargo:/root/.cargo",
            "-v",
            "dreamer-arm64-rustup:/root/.rustup",
            "-v",
            "dreamer-arm64-cache:/root/.cache",
            "-e",
            "CARGO_TARGET_DIR=/root/.cache/dreamer-bench-target",
            "-w",
            "/src",
            "ubuntu:24.04",
            "bash",
            "-c",
            &script,
        ])
        .status()
        .map_err(|e| format!("docker run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the {workload} workload failed in the Linux container"
        ))
    }
}

/// Where the checkout stands: commit, branch (none when detached), and
/// whether tracked files have uncommitted changes. Untracked files are not
/// counted. Taken from `DREAMER_BENCH_GIT` when the host passed it in.
fn git_state(root: &Path) -> Value {
    if let Some(v) = std::env::var("DREAMER_BENCH_GIT")
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        return v;
    }
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    json!({
        "commit": git(&["rev-parse", "HEAD"]),
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD"),
        "dirty": git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()),
    })
}

/// The revision block every result file carries: the checkout, the build
/// profile, the toolchain, the tmux that ran, and the pinned tmux and
/// libghostty-vt.
fn revision(root: &Path, profile: &str) -> Value {
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(program)
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let manifest = crate::manifest::Manifest::load().ok();
    let pin = |section: &str, key: &str| {
        manifest
            .as_ref()
            .and_then(|m| m.get(section, key).ok())
            .map(str::to_owned)
    };
    let mut v = git_state(root);
    if let Some(o) = v.as_object_mut() {
        o.insert("profile".into(), json!(profile));
        o.insert("toolchain".into(), json!(run("rustc", &["-V"])));
        o.insert("tmux".into(), json!(run(&tmux, &["-V"])));
        o.insert(
            "pins".into(),
            json!({
                "tmux": pin("tmux", "version"),
                "libghostty_vt": pin("ghostty", "tag"),
                "libghostty_vt_commit": pin("ghostty", "commit"),
            }),
        );
    }
    v
}

/// `cargo xtask bench report FILE...`: the files' machine and revision,
/// then one table with a column per file and a row per measure, a p50 and
/// its p95 sharing a row.
fn report(files: &[String]) -> Result<(), String> {
    if files.is_empty() {
        return Err(USAGE.into());
    }
    let mut runs = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{f}: {e}"))?;
        if v["results"].as_array().is_none_or(Vec::is_empty) {
            return Err(format!("{f}: no results"));
        }
        let name = Path::new(f)
            .file_stem()
            .map_or_else(|| f.clone(), |s| s.to_string_lossy().into_owned());
        runs.push((name, v));
    }
    print!("{}", render(&runs));
    Ok(())
}

/// The column heading for one run.
fn label(name: &str, v: &Value) -> String {
    let n = |k: &str| v[k].as_u64();
    let mut l = match (v["workload"].as_str(), n("panes")) {
        (Some(w @ ("busy" | "streams")), Some(p)) => {
            format!("{w}, {} busy panes", group(&p.to_string()))
        }
        (Some(w), Some(p)) => format!(
            "{w}, {} pane{}",
            group(&p.to_string()),
            if p == 1 { "" } else { "s" }
        ),
        _ => name.to_owned(),
    };
    if let Some(h) = n("history").filter(|h| *h > 0) {
        l += &format!(", {} lines of history", group(&h.to_string()));
    }
    if let Some(i) = n("idle").filter(|i| *i > 0) {
        l += &format!(", {} idle", group(&i.to_string()));
    }
    if let Some(s) = v["stream"].as_str() {
        l += &match n("rate_kib") {
            Some(0) => format!(", {s} flat out"),
            Some(r) => format!(", {s} at {} KiB/s a pane", group(&r.to_string())),
            None => format!(", {s}"),
        };
    }
    l
}

fn machine_line(h: &Value) -> String {
    let s = |k: &str| h[k].as_str().unwrap_or("").trim();
    if h.is_null() {
        return "not recorded".into();
    }
    // macOS gives bytes, Linux `N kB`.
    let mem = s("memory");
    let gib = match mem.strip_suffix("kB") {
        Some(kb) => kb.trim().parse::<f64>().ok().map(|k| k / 1048576.0),
        None => mem.parse::<f64>().ok().map(|b| b / 1073741824.0),
    };
    let mut parts = vec![s("cpu").to_owned()];
    if let Some(c) = h["cores"].as_u64().filter(|c| *c > 0) {
        parts.push(format!("{c} cores"));
    }
    if let Some(g) = gib {
        parts.push(format!("{} GiB", number(g)));
    }
    parts.retain(|p| !p.is_empty());
    let what = match (s("model"), parts.is_empty()) {
        ("", _) => parts.join(", "),
        (m, true) => m.to_owned(),
        (m, false) => format!("{m} ({})", parts.join(", ")),
    };
    format!("{what}, {}", s("os"))
}

fn revision_line(r: &Value) -> String {
    if r.is_null() {
        return "not recorded".into();
    }
    let s = |k: &str| r[k].as_str();
    let mut l = match s("commit") {
        Some(c) => format!("`{}`", &c[..c.len().min(12)]),
        None => "unknown commit".into(),
    };
    if let Some(b) = s("branch") {
        l += &format!(" (branch `{b}`)");
    }
    l += match r["dirty"].as_bool() {
        Some(true) => ", with uncommitted changes",
        Some(false) => ", no uncommitted changes",
        None => "",
    };
    let mut built = Vec::new();
    if let Some(t) = s("toolchain") {
        built.push(t.to_owned());
    }
    if let Some(t) = s("tmux") {
        built.push(t.to_owned());
    }
    if let Some(g) = r["pins"]["libghostty_vt"].as_str() {
        built.push(format!("libghostty-vt `{g}`"));
    }
    if !built.is_empty() {
        l += &format!("; {}", built.join(", "));
    }
    match s("profile") {
        Some("release") | None => {}
        Some(p) => l += &format!(". **{p} build: not performance evidence**"),
    }
    l
}

/// Lines that are the same for every run once, else one per run.
fn per_run(out: &mut String, what: &str, lines: &[(String, String)]) {
    if lines.windows(2).all(|w| w[0].1 == w[1].1) {
        if let Some((_, l)) = lines.first() {
            out.push_str(&format!("- **{what}:** {l}.\n"));
        }
    } else {
        for (label, l) in lines {
            out.push_str(&format!("- **{what} ({label}):** {l}.\n"));
        }
    }
}

fn render(runs: &[(String, Value)]) -> String {
    let labels: Vec<String> = runs.iter().map(|(n, v)| label(n, v)).collect();
    let mut out = String::new();
    let lines = |f: fn(&Value) -> String, key: &str| -> Vec<(String, String)> {
        runs.iter()
            .zip(&labels)
            .map(|((_, v), l)| (l.clone(), f(&v[key])))
            .collect()
    };
    per_run(&mut out, "Machine", &lines(machine_line, "hardware"));
    per_run(&mut out, "Revision", &lines(revision_line, "revision"));
    out.push('\n');

    // Every measure, in the order first seen.
    let mut measures: Vec<String> = Vec::new();
    for (_, v) in runs {
        for r in v["results"].as_array().into_iter().flatten() {
            if let Some(m) = r["measure"].as_str()
                && !measures.iter().any(|x| x == m)
            {
                measures.push(m.to_owned());
            }
        }
    }
    let find = |v: &Value, m: &str| -> Option<(f64, String)> {
        v["results"].as_array()?.iter().find_map(|r| {
            (r["measure"].as_str() == Some(m)).then(|| {
                (
                    r["value"].as_f64().unwrap_or(f64::NAN),
                    r["unit"].as_str().unwrap_or("").to_owned(),
                )
            })
        })
    };
    out.push_str(&format!("| Measure | {} | Target |\n", labels.join(" | ")));
    out.push_str(&format!("|{}\n", " --- |".repeat(runs.len() + 2)));
    let mut done: Vec<&str> = Vec::new();
    for m in &measures {
        if done.contains(&m.as_str()) {
            continue;
        }
        let pair = m
            .strip_suffix(", p50")
            .map(|p| format!("{p}, p95"))
            .filter(|p95| measures.contains(p95));
        let mut row = vec![capitalize(match &pair {
            Some(_) => format!("{}, p50 / p95", &m[..m.len() - 5]),
            None => m.clone(),
        })];
        for (_, v) in runs {
            let a = find(v, m);
            let cell = match (&pair, a) {
                (_, None) => "—".to_owned(),
                (None, Some((x, u))) => cell(&number(x), &u),
                (Some(p95), Some((x, u))) => {
                    let y = find(v, p95).map_or("—".to_owned(), |(y, _)| number(y));
                    cell(&format!("{} / {y}", number(x)), &u)
                }
            };
            row.push(cell);
        }
        row.push(String::new());
        out.push_str(&format!("| {} |\n", row.join(" | ")));
        done.push(m);
        if let Some(p95) = &pair
            && let Some(p) = measures.iter().find(|x| *x == p95)
        {
            done.push(p);
        }
    }
    out
}

fn capitalize(s: String) -> String {
    let mut c = s.chars();
    c.next()
        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

fn cell(value: &str, unit: &str) -> String {
    if unit.is_empty() || value == "—" {
        value.to_owned()
    } else {
        format!("{value} {unit}")
    }
}

/// Three significant figures (whole numbers stay whole), with thousands
/// separated as the docs write them.
fn number(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    let s = if v.fract() == 0.0 || a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    let (sign, s) = s.strip_prefix('-').map_or(("", s.as_str()), |r| ("-", r));
    let (int, frac) = s.split_once('.').map_or((s, None), |(i, f)| (i, Some(f)));
    let mut out = format!("{sign}{}", group(int));
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

/// `1234567` as `1,234,567`.
fn group(digits: &str) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Each shell prints `READY`, which the GUI benchmark waits for.
const READY_SHELL: &str = "/bin/sh -c 'printf READY\\n; exec /bin/sh'";

/// `cargo xtask bench gui [--panes N]`: the desktop measuring itself
/// (`crates/app/src/bench_gui.rs`) against an isolated tmux server with N
/// panes, at most 16 to a window (tab), the first window visible, and a
/// daemon already running. Needs a display that is awake and unlocked,
/// with the app's window in front.
fn gui(root: &std::path::Path, rest: &[String]) -> Result<(), String> {
    let mut panes = 16usize;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--panes" => {
                panes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n >= 1)
                    .ok_or("--panes needs a number")?;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if crate::package::screen_locked() == Some(true) {
        return Err("the screen is locked: the window would not be drawn".into());
    }
    let built = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
            "-p",
            "bench",
        ])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !built.success() {
        return Err("building dreamer and dreamerd failed".into());
    }
    let bin = crate::util::cargo_target(root).join("release");
    let machine = Command::new(bin.join("dreamer-bench"))
        .arg("machine")
        .output()
        .map_err(|e| format!("dreamer-bench machine: {e}"))?;
    let machine = String::from_utf8_lossy(&machine.stdout).trim().to_owned();
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let socket = format!("dreamer-bench-gui-{}", std::process::id());
    let dir = std::path::PathBuf::from(format!("/tmp/drm-bgui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config")).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("config/dreamer.yaml"),
        "tiers:\n  cold_after: never\n  warm_limit: none\n",
    )
    .map_err(|e| e.to_string())?;
    // A large window, so 16 tiled panes keep a few lines each.
    std::fs::create_dir_all(dir.join("desktop")).map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("desktop/desktop.yaml"),
        "version: 1\nendpoint: ''\nincarnation: 0\nsidebar: false\nbounds: [40.0, 40.0, 1600.0, 1000.0]\n",
    )
    .map_err(|e| e.to_string())?;
    let t = |args: &[&str]| -> Result<(), String> {
        let ok = Command::new(&tmux)
            .args(["-L", &socket])
            .args(args)
            .status()
            .map_err(|e| format!("tmux: {e}"))?
            .success();
        if ok {
            Ok(())
        } else {
            Err(format!("tmux {args:?} failed"))
        }
    };
    // Windows of up to 16 panes, tiled.
    let mut left = panes;
    let mut window = 0usize;
    while left > 0 {
        let here = left.min(16);
        let target = format!("main:{window}");
        if window == 0 {
            t(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "main",
                "-x",
                "220",
                "-y",
                "64",
                READY_SHELL,
            ])?;
        } else {
            t(&["new-window", "-d", "-t", &target, READY_SHELL])?;
        }
        for _ in 1..here {
            t(&["split-window", "-d", "-t", &target, READY_SHELL])?;
            t(&["select-layout", "-t", &target, "tiled"])?;
        }
        left -= here;
        window += 1;
    }
    t(&["select-window", "-t", "main:0"])?;
    let env = |c: &mut Command| {
        c.env("DREAMER_RUNTIME_DIR", dir.join("rt"))
            .env("DREAMER_STATE_DIR", dir.join("st"))
            .env("DREAMER_DESKTOP_STATE_DIR", dir.join("desktop"))
            .env("DREAMER_CONFIG_DIR", dir.join("config"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1");
    };
    // The daemon first: launch is measured with it running.
    let mut d = Command::new(bin.join("dreamerd"));
    d.args(["--background", "--tmux-socket", &socket, "--tmux", &tmux]);
    env(&mut d);
    d.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut daemon = d.spawn().map_err(|e| format!("dreamerd: {e}"))?;
    std::thread::sleep(std::time::Duration::from_secs(3 + panes as u64 / 100));

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let out = root.join(format!("target/bench/gui-{panes}-{stamp}.json"));
    std::fs::create_dir_all(out.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
    let mut app = Command::new(bin.join("dreamer"));
    app.args(["--workspace", "-L", &socket, "--tmux", &tmux])
        .env("DREAMER_BENCH_GUI", &out)
        .env("DREAMER_BENCH_PANES", panes.to_string())
        .env("DREAMER_BENCH_MACHINE", &machine)
        .env(
            "DREAMER_BENCH_REVISION",
            revision(root, "release").to_string(),
        );
    env(&mut app);
    let status = app.status().map_err(|e| format!("dreamer: {e}"));

    // Clean up whatever happened.
    if let Some(pid) = std::fs::read_to_string(dir.join("rt/dreamerd.lock"))
        .ok()
        .map(|s| s.trim().to_owned())
    {
        let _ = Command::new("kill").arg(&pid).status();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = t(&["kill-server"]);
    let _ = std::fs::remove_dir_all(&dir);
    if !status?.success() {
        return Err("the GUI benchmark failed".into());
    }
    println!("results: {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_pairs_percentiles_and_names_machine_and_revision() {
        let run = |history: u64, p50: f64, p95: f64| {
            json!({
                "workload": "headless", "panes": 400, "history": history,
                "hardware": { "model": "Mac15,11", "cpu": "Apple M3 Max", "cores": 14,
                              "memory": "38654705664", "os": "Darwin 27.2.0 arm64" },
                "revision": { "commit": "4b08011aabbccddeeff", "branch": null, "dirty": false,
                              "profile": "release", "toolchain": "rustc 1.99.1",
                              "tmux": "tmux 3.8", "pins": { "libghostty_vt": "vt-1.0.1" } },
                "results": [
                    { "measure": "key to echo, 1 pane, p50", "value": p50, "unit": "ms" },
                    { "measure": "key to echo, 1 pane, p95", "value": p95, "unit": "ms" },
                    { "measure": "daemon RSS, 400 Warm panes", "value": 1888.4, "unit": "MiB" },
                ],
            })
        };
        let out = render(&[
            ("a".into(), run(0, 0.2234, 0.281)),
            ("b".into(), run(2000, 13.71, 14.62)),
        ]);
        assert_eq!(
            out,
            "- **Machine:** Mac15,11 (Apple M3 Max, 14 cores, 36 GiB), Darwin 27.2.0 arm64.\n\
             - **Revision:** `4b08011aabbc`, no uncommitted changes; rustc 1.99.1, tmux 3.8, libghostty-vt `vt-1.0.1`.\n\
             \n\
             | Measure | headless, 400 panes | headless, 400 panes, 2,000 lines of history | Target |\n\
             | --- | --- | --- | --- |\n\
             | Key to echo, 1 pane, p50 / p95 | 0.22 / 0.28 ms | 13.7 / 14.6 ms |  |\n\
             | Daemon RSS, 400 Warm panes | 1,888 MiB | 1,888 MiB |  |\n"
        );
    }

    #[test]
    fn a_debug_run_says_it_is_not_evidence() {
        let r = json!({ "commit": "abc", "dirty": true, "profile": "debug" });
        assert_eq!(
            revision_line(&r),
            "`abc`, with uncommitted changes. **debug build: not performance evidence**"
        );
        assert_eq!(revision_line(&Value::Null), "not recorded");
    }
}
