//! `cargo xtask gate linux-desktop`: real-display evidence on a Linux
//! desktop (spec 17.4). Run by a person on a machine with a graphical
//! session; not part of CI.
//!
//! Builds the tested revision, then for each display backend that the
//! session offers (native Wayland via `WAYLAND_DISPLAY`, X11 via `DISPLAY`
//! with `WAYLAND_DISPLAY` removed) launches the app's smoke run on the
//! built-in demo and on every renderer fixture. Writes a report to
//! `target/gate/linux-desktop/<commit>.json`. Both backends are required
//! unless `--allow-missing wayland|x11` is given for one of them.

use std::fmt::Write as _;
use std::process::Command;
use std::time::Instant;

use crate::util::{capture, repo_root};

#[derive(Debug)]
struct Run {
    backend: &'static str,
    input: String,
    passed: bool,
    millis: u128,
    detail: String,
}

pub fn run(args: &[String]) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("linux-desktop runs on a Linux desktop session".into());
    }
    let mut allow_missing: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--allow-missing" => allow_missing.push(
                iter.next()
                    .ok_or("--allow-missing needs a backend")?
                    .clone(),
            ),
            other => return Err(format!("unknown linux-desktop argument `{other}`")),
        }
    }
    let root = repo_root();
    let commit = capture("git", &["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let dirty = capture(
        "git",
        &["-C", &root.to_string_lossy(), "status", "--porcelain"],
    )
    .is_some_and(|s| !s.is_empty());

    for step in [
        vec!["xtask", "deps", "prepare", "--locked"],
        vec!["build", "--locked", "--package", "dreamer-app"],
    ] {
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(&step)
            .current_dir(&root)
            .status()
            .map_err(|e| format!("cargo {}: {e}", step.join(" ")))?;
        if !status.success() {
            return Err(format!("cargo {} failed", step.join(" ")));
        }
    }
    let exe = root.join("target/debug/dreamer");
    let fixtures_dir = root.join("crates/terminal-ghostty/tests/fixtures/snapshot-src");
    let mut inputs: Vec<Option<String>> = vec![None];
    let mut names: Vec<String> = std::fs::read_dir(&fixtures_dir)
        .map_err(|e| format!("{}: {e}", fixtures_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "vt"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    names.sort();
    inputs.extend(names.into_iter().map(Some));

    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let backends: [(&'static str, bool); 2] =
        [("wayland", set("WAYLAND_DISPLAY")), ("x11", set("DISPLAY"))];
    let mut runs = Vec::new();
    let mut problems = Vec::new();
    for (backend, available) in backends {
        if !available {
            let msg = format!("{backend}: not available in this session");
            if allow_missing.iter().any(|b| b == backend) {
                println!("linux-desktop: {msg} (allowed)");
            } else {
                problems.push(msg);
            }
            continue;
        }
        for input in &inputs {
            let mut cmd = Command::new(&exe);
            cmd.arg("--smoke");
            if let Some(path) = input {
                cmd.args(["--fixture", path]);
            }
            if backend == "x11" {
                cmd.env_remove("WAYLAND_DISPLAY");
            }
            let started = Instant::now();
            let out = cmd.output().map_err(|e| format!("launch: {e}"))?;
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let expected = if backend == "wayland" {
                "Wayland"
            } else {
                "X11"
            };
            let passed = out.status.success()
                && stdout.contains(&format!("smoke: backend {expected}"))
                && stdout.contains("smoke: frame presented");
            let label = input
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or("demo")
                .to_owned();
            println!(
                "linux-desktop: {backend:<7} {label:<18} {}",
                if passed { "passed" } else { "FAILED" }
            );
            if !passed {
                problems.push(format!("{backend} {label}: {}", out.status));
            }
            runs.push(Run {
                backend,
                input: label,
                passed,
                millis: started.elapsed().as_millis(),
                detail: format!(
                    "{}{}",
                    stdout.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
    }

    let report = write_report(&commit, dirty, &runs)?;
    println!("linux-desktop: report {report}");
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

fn json(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn write_report(commit: &str, dirty: bool, runs: &[Run]) -> Result<String, String> {
    let dir = repo_root().join("target/gate/linux-desktop");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let distro = os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .unwrap_or("unknown")
        .trim_matches('"')
        .to_owned();
    let env = |n: &str| std::env::var(n).unwrap_or_default();
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\n  \"commit\": \"{}\",\n  \"dirty\": {dirty},\n  \"distro\": \"{}\",\n  \
         \"kernel\": \"{}\",\n  \"arch\": \"{}\",\n  \"desktop\": \"{}\",\n  \
         \"session_type\": \"{}\",\n  \"runs\": [",
        json(commit),
        json(&distro),
        json(&capture("uname", &["-r"]).unwrap_or_default()),
        std::env::consts::ARCH,
        json(&env("XDG_CURRENT_DESKTOP")),
        json(&env("XDG_SESSION_TYPE")),
    );
    for (i, r) in runs.iter().enumerate() {
        let _ = write!(
            s,
            "{}\n    {{ \"backend\": \"{}\", \"input\": \"{}\", \"passed\": {}, \"millis\": {}, \"output\": \"{}\" }}",
            if i == 0 { "" } else { "," },
            r.backend,
            json(&r.input),
            r.passed,
            r.millis,
            json(&r.detail)
        );
    }
    s.push_str("\n  ]\n}\n");
    let path = dir.join(format!("{}.json", &commit[..12.min(commit.len())]));
    std::fs::write(&path, s).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}
