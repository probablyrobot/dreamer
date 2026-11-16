//! `cargo xtask bench`: the benchmark harness (spec 16.5, 16.6). Builds the
//! daemon and `dreamer-bench` in release mode and runs a workload against
//! a private tmux server and daemon, writing the results with the machine
//! they ran on to `target/bench/`.
//!
//! ```text
//! cargo xtask bench headless|busy [OPTIONS] [--linux]
//! ```
//!
//! `--linux` runs the same workload in an ARM64 Linux container (the one
//! the gate's `arm64` stage uses), which has the ptys for 1,000 panes:
//! macOS allows 511 for the whole system (`kern.tty.ptmx_max`).

use std::process::Command;

pub fn run(args: &[String]) -> Result<(), String> {
    let root = crate::util::repo_root();
    let (workload, rest) = match args {
        [w, rest @ ..] if w == "gui" => return gui(&root, rest),
        [w, rest @ ..] if w == "headless" || w == "busy" => (w.as_str(), rest),
        _ => {
            return Err(
                "usage: cargo xtask bench headless|busy [OPTIONS] [--linux]\n       cargo xtask bench gui [--panes N]"
                    .into(),
            );
        }
    };
    if rest.iter().any(|a| a == "--linux") {
        let rest: Vec<String> = rest.iter().filter(|a| *a != "--linux").cloned().collect();
        return linux(&root, workload, &rest);
    }
    let built = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "dreamer-server",
            "-p",
            "bench",
        ])
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
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let status = Command::new(target.join("release/dreamer-bench"))
        .arg(workload)
        .args(rest)
        .arg("--json")
        .arg(&out)
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
    let status = Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/arm64"])
        .arg("-v")
        .arg(format!("{}:/src", root.display()))
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
        ])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !built.success() {
        return Err("building dreamer and dreamerd failed".into());
    }
    let bin = crate::util::cargo_target(root).join("release");
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
        .env("DREAMER_BENCH_GUI", &out);
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
