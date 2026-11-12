//! `cargo xtask headless [--cargo-args|--check]`: the packages that make up
//! the headless build (server, CLI, pane-app runtime, and their libraries).
//!
//! These must build without GPUI, a display server, a GPU API, or font
//! libraries. The list is the single source for CI, the gate, and the
//! boundary check; `--check` fails if any of them can pull in a GUI crate on
//! any target platform, or if a workspace crate is in neither list.

use std::collections::BTreeSet;
use std::process::Command;

use crate::util::repo_root;

/// Packages built and tested for every headless target, including ARM64
/// Linux. Add each new UI-free crate here.
pub const PACKAGES: &[&str] = &[
    "api",
    "dreamer-server",
    "dreamer-cli",
    "dreamer-termapp",
    "domain",
    "ghostty-sys",
    "script-runtime",
    "terminal-core",
    "terminal-ghostty",
    "tmux-backend",
    "tmux-protocol",
    "wire",
    "workspace-config",
    "workspace-host",
];

/// Headless packages whose integration tests drive a real tmux. Where no
/// tmux baseline binary exists for the target (the emulated ARM64 job),
/// only their unit tests run.
pub const NEEDS_TMUX: &[&str] = &["tmux-backend", "workspace-host"];

/// Workspace packages that are allowed to depend on GUI crates, or are
/// build tooling outside the shipped graph.
const NOT_HEADLESS: &[&str] = &[
    "api-gen",
    "dreamer-app",
    "ghostty-bindgen",
    "terminal-render",
    "xtask",
];

/// Crate name prefixes that must never appear in a headless dependency
/// graph: the UI framework, windowing, GPU, and font stacks.
const FORBIDDEN: &[&str] = &[
    "gpui",
    "wgpu",
    "naga",
    "blade-",
    "metal",
    "ash",
    "glow",
    "khronos-egl",
    "font-kit",
    "cosmic-text",
    "freetype",
    "fontconfig",
    "yeslogic-fontconfig",
    "core-text",
    "core-graphics",
    "objc",
    "cocoa",
    "wayland-",
    "x11",
    "xcb",
    "xkbcommon",
    "xkeysym",
    "winit",
    "calloop-wayland",
];

pub fn run(args: &[String]) -> Result<(), String> {
    match args {
        [] => {
            for p in PACKAGES {
                println!("{p}");
            }
            Ok(())
        }
        [flag] if flag == "--cargo-args" => {
            let parts: Vec<String> = PACKAGES.iter().map(|p| format!("-p {p}")).collect();
            println!("{}", parts.join(" "));
            Ok(())
        }
        [flag] if flag == "--cargo-args-without-tmux" => {
            let parts: Vec<String> = PACKAGES
                .iter()
                .filter(|p| !NEEDS_TMUX.contains(p))
                .map(|p| format!("-p {p}"))
                .collect();
            println!("{}", parts.join(" "));
            Ok(())
        }
        [flag] if flag == "--needs-tmux" => {
            let parts: Vec<String> = NEEDS_TMUX.iter().map(|p| format!("-p {p}")).collect();
            println!("{}", parts.join(" "));
            Ok(())
        }
        [flag] if flag == "--check" => check(),
        _ => Err(
            "usage: cargo xtask headless [--cargo-args|--cargo-args-without-tmux|--needs-tmux|--check]"
                .into(),
        ),
    }
}

fn check() -> Result<(), String> {
    let mut problems = Vec::new();

    // Every workspace package must be classified.
    for name in workspace_packages()? {
        if !PACKAGES.contains(&name.as_str()) && !NOT_HEADLESS.contains(&name.as_str()) {
            problems.push(format!(
                "workspace package `{name}` is not classified; add it to PACKAGES or NOT_HEADLESS in xtask/src/headless.rs"
            ));
        }
    }

    for package in PACKAGES {
        for dep in dependency_names(package)? {
            if is_forbidden(&dep) {
                problems.push(format!("`{package}` depends on `{dep}`"));
            }
        }
    }

    if problems.is_empty() {
        println!(
            "headless: ok ({} packages, no GUI, windowing, GPU, or font crates on any target)",
            PACKAGES.len()
        );
        Ok(())
    } else {
        for p in &problems {
            eprintln!("headless: {p}");
        }
        Err(format!("{} headless boundary violation(s)", problems.len()))
    }
}

fn is_forbidden(name: &str) -> bool {
    FORBIDDEN
        .iter()
        .any(|f| name == f.trim_end_matches('-') || name.starts_with(f))
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(repo_root());
    cmd
}

/// Normal and build dependencies of `package` for every target platform.
fn dependency_names(package: &str) -> Result<BTreeSet<String>, String> {
    let out = cargo()
        .args([
            "tree",
            "--locked",
            "--package",
            package,
            "--edges",
            "normal,build",
            "--target",
            "all",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .output()
        .map_err(|e| format!("could not run cargo tree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo tree -p {package} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

fn workspace_packages() -> Result<Vec<String>, String> {
    let out = cargo()
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|e| format!("could not run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".into());
    }
    // Avoid a JSON dependency: package names appear as `"name":"..."` entries
    // directly inside each package object, before its `"version"`.
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    for chunk in text.split("{\"name\":\"").skip(1) {
        if let Some(end) = chunk.find('"')
            && chunk[end..].starts_with("\",\"version\"")
        {
            names.push(chunk[..end].to_owned());
        }
    }
    if names.is_empty() {
        return Err("no workspace packages found in cargo metadata".into());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::is_forbidden;

    #[test]
    fn forbidden_names_match_whole_families() {
        for name in [
            "gpui",
            "gpui_platform",
            "wgpu-core",
            "x11rb",
            "xcb",
            "wayland-client",
            "font-kit",
            "objc2",
        ] {
            assert!(is_forbidden(name), "{name}");
        }
        for name in [
            "tokio",
            "mlua",
            "ghostty-sys",
            "metadata",
            "xtask",
            "proptest",
        ] {
            assert!(!is_forbidden(name), "{name}");
        }
    }
}
