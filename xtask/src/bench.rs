//! `cargo xtask bench`: the benchmark harness (spec 16.5, 16.6). Builds the
//! daemon and `dreamer-bench` in release mode and runs a workload against
//! a private tmux server and daemon, writing the results with the machine
//! they ran on to `target/bench/`.
//!
//! ```text
//! cargo xtask bench headless [--panes N] [--linux]
//! ```
//!
//! `--linux` runs the same workload in an ARM64 Linux container (the one
//! the gate's `arm64` stage uses), which has the ptys for 1,000 panes:
//! macOS allows 511 for the whole system (`kern.tty.ptmx_max`).

use std::process::Command;

pub fn run(args: &[String]) -> Result<(), String> {
    let root = crate::util::repo_root();
    let (workload, rest) = match args {
        [w, rest @ ..] if w == "headless" => (w.as_str(), rest),
        _ => return Err("usage: cargo xtask bench headless [--panes N] [--linux]".into()),
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
