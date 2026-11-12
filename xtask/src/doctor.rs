//! `cargo xtask doctor`: report whether this machine can build and test Dreamer.

use crate::deps::{deps_dir, ghostty_prefix, host_triple};
use crate::manifest::Manifest;
use crate::util::{capture, parse_version, repo_root, which};

/// Minimum supported tmux release (spec section 2.3).
const TMUX_BASELINE: (u32, u32, u32) = (3, 8, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug)]
struct Check {
    level: Level,
    name: &'static str,
    detail: String,
}

impl Check {
    fn new(level: Level, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level,
            name,
            detail: detail.into(),
        }
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    if let Some(arg) = args.first() {
        return Err(format!("doctor takes no arguments, got `{arg}`"));
    }

    let mut checks = vec![platform(), rust_toolchain(), tmux(), zig(), git()];
    checks.push(clang());
    checks.extend(native_build_tools());
    checks.push(docker());

    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for check in &checks {
        let tag = match check.level {
            Level::Ok => " ok ",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        println!("[{tag}] {:<width$}  {}", check.name, check.detail);
    }

    let failures = checks.iter().filter(|c| c.level == Level::Fail).count();
    if failures > 0 {
        return Err(format!("{failures} required check(s) failed"));
    }
    Ok(())
}

fn platform() -> Check {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("macos", "aarch64") => Check::new(Level::Ok, "platform", "macOS on Apple Silicon"),
        ("macos", _) => Check::new(
            Level::Fail,
            "platform",
            format!("macOS on {arch}: only Apple Silicon is supported"),
        ),
        ("linux", "x86_64" | "aarch64") => {
            Check::new(Level::Ok, "platform", format!("Linux on {arch}"))
        }
        _ => Check::new(
            Level::Fail,
            "platform",
            format!("{os}/{arch} is not a supported build platform"),
        ),
    }
}

fn pinned_rust_channel() -> Option<String> {
    let text = std::fs::read_to_string(repo_root().join("rust-toolchain.toml")).ok()?;
    text.lines().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix("channel")?
            .trim()
            .strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_owned())
    })
}

fn rust_toolchain() -> Check {
    let Some(pinned) = pinned_rust_channel() else {
        return Check::new(Level::Fail, "rust", "rust-toolchain.toml has no channel");
    };
    let Some(version) = capture("rustc", &["--version"]) else {
        return Check::new(Level::Fail, "rust", "rustc not found");
    };
    let active = parse_version(&version);
    if active.is_some() && active == parse_version(&pinned) {
        Check::new(Level::Ok, "rust", version)
    } else {
        Check::new(
            Level::Fail,
            "rust",
            format!("{version}, but rust-toolchain.toml pins {pinned}"),
        )
    }
}

fn tmux() -> Check {
    let program = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".to_owned());
    let Some(version) = capture(&program, &["-V"]) else {
        return Check::new(
            Level::Fail,
            "tmux",
            format!("`{program}` not found (set DREAMER_TMUX to its path)"),
        );
    };
    match parse_version(&version) {
        Some(v) if v >= TMUX_BASELINE => Check::new(Level::Ok, "tmux", version),
        Some(_) => Check::new(
            Level::Fail,
            "tmux",
            format!(
                "{version} is older than the {}.{} baseline",
                TMUX_BASELINE.0, TMUX_BASELINE.1
            ),
        ),
        None => Check::new(
            Level::Warn,
            "tmux",
            format!("unrecognized version `{version}`"),
        ),
    }
}

/// Zig and libghostty-vt come from the dependency cache, not from PATH.
fn zig() -> Check {
    let pinned = Manifest::load().and_then(|m| {
        let version = m.get("zig", "version")?.to_owned();
        let prefix = ghostty_prefix(&m, host_triple()?)?;
        Ok((version, prefix, m.get("ghostty", "version")?.to_owned()))
    });
    let (version, prefix, ghostty) = match pinned {
        Ok(p) => p,
        Err(e) => return Check::new(Level::Fail, "ghostty", e),
    };
    let zig_ok = deps_dir()
        .map(|d| d.join("zig").join(&version).is_dir())
        .unwrap_or(false);
    let lib_ok = prefix.join(".dreamer-build").is_file();
    match (zig_ok, lib_ok) {
        (true, true) => Check::new(
            Level::Ok,
            "ghostty",
            format!(
                "libghostty-vt {ghostty} built with Zig {version} at {}",
                prefix.display()
            ),
        ),
        _ => Check::new(
            Level::Warn,
            "ghostty",
            format!("libghostty-vt {ghostty} not prepared; run `cargo xtask deps prepare`"),
        ),
    }
}

fn git() -> Check {
    match capture("git", &["--version"]) {
        Some(v) => Check::new(Level::Ok, "git", v),
        None => Check::new(Level::Fail, "git", "not found"),
    }
}

#[cfg(target_os = "macos")]
fn native_build_tools() -> Vec<Check> {
    let mut checks = Vec::new();
    match capture("xcode-select", &["-p"]) {
        Some(dir) if dir.contains("Xcode") => {
            let version = capture("xcodebuild", &["-version"])
                .and_then(|v| v.lines().next().map(str::to_owned))
                .unwrap_or_else(|| "version unknown".to_owned());
            checks.push(Check::new(
                Level::Ok,
                "xcode",
                format!("{version} at {dir}"),
            ));
        }
        Some(dir) => checks.push(Check::new(
            Level::Fail,
            "xcode",
            format!("developer dir is {dir}; full Xcode is required for Metal shaders"),
        )),
        None => checks.push(Check::new(Level::Fail, "xcode", "Xcode not installed")),
    }
    // `xcrun --find metal` succeeds even when the separately installed Metal
    // Toolchain component is missing; only running the compiler proves it.
    checks.push(
        match capture("xcrun", &["--sdk", "macosx", "metal", "--version"]) {
            Some(version) => Check::new(
                Level::Ok,
                "metal",
                version.lines().next().unwrap_or("installed").to_owned(),
            ),
            None => Check::new(
                Level::Fail,
                "metal",
                "Metal compiler not found (xcodebuild -downloadComponent MetalToolchain)",
            ),
        },
    );
    checks
}

#[cfg(target_os = "linux")]
fn native_build_tools() -> Vec<Check> {
    let mut checks = Vec::new();
    for tool in ["cc", "pkg-config"] {
        checks.push(match which(tool) {
            Some(path) => Check::new(Level::Ok, tool, path.display().to_string()),
            None => Check::new(Level::Fail, tool, "not found"),
        });
    }
    // The exact GUI package list is fixed when the GPUI baseline is pinned;
    // until then these are reported, not required.
    let modules = [
        "wayland-client",
        "xkbcommon",
        "x11-xcb",
        "fontconfig",
        "freetype2",
    ];
    let missing: Vec<_> = modules
        .iter()
        .filter(|m| capture("pkg-config", &["--exists", m]).is_none())
        .copied()
        .collect();
    checks.push(if missing.is_empty() {
        Check::new(Level::Ok, "gui libs", modules.join(", "))
    } else {
        Check::new(
            Level::Info,
            "gui libs",
            format!(
                "missing {} (only needed for the desktop app)",
                missing.join(", ")
            ),
        )
    });
    checks
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_build_tools() -> Vec<Check> {
    Vec::new()
}

/// bindgen loads libclang to regenerate or check the Ghostty bindings.
fn clang() -> Check {
    match capture("clang", &["--version"]).and_then(|v| v.lines().next().map(str::to_owned)) {
        Some(v) => Check::new(Level::Ok, "clang", v),
        None => Check::new(
            Level::Warn,
            "clang",
            "not found (needed by `cargo xtask bindings`)",
        ),
    }
}

fn docker() -> Check {
    if which("docker").is_none() {
        return Check::new(
            Level::Info,
            "docker",
            "not found (optional: native ARM64 Linux tests in the pre-push gate)",
        );
    }
    match capture("docker", &["info", "--format", "{{.ServerVersion}}"]) {
        Some(v) => Check::new(Level::Ok, "docker", format!("daemon {v}")),
        None => Check::new(
            Level::Info,
            "docker",
            "installed, daemon not running (optional: ARM64 Linux tests in pre-push)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_toolchain_file_has_a_channel() {
        let channel = pinned_rust_channel().expect("channel");
        assert!(parse_version(&channel).is_some(), "{channel}");
    }

    #[test]
    fn levels_order_by_severity() {
        assert!(Level::Fail > Level::Warn && Level::Warn > Level::Info && Level::Info > Level::Ok);
    }
}
