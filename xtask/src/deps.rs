//! `cargo xtask deps prepare [--target <triple>]...`: fetch, verify, and build
//! pinned native dependencies (Zig and `libghostty-vt`).
//!
//! Everything lands in a per-user cache (`DREAMER_DEPS_DIR`, default
//! `~/Library/Caches/dreamer/deps` or `$XDG_CACHE_HOME/dreamer/deps`) keyed by
//! version and target, so the gate worktree, CI, and every checkout share one
//! build. Downloads are verified against the SHA-256 pins in
//! `deps/baseline.toml`; nothing unpinned is ever fetched.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::Manifest;

pub fn run(args: &[String]) -> Result<(), String> {
    let (cmd, rest) = args
        .split_first()
        .ok_or("usage: cargo xtask deps <prepare|path|tmux> [--target <triple>]...")?;
    let mut targets = Vec::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Pins are never updated by this command; accepted for clarity.
            "--locked" => {}
            "--target" => targets.push(iter.next().ok_or("--target needs a value")?.clone()),
            other => return Err(format!("unknown deps argument `{other}`")),
        }
    }
    if targets.is_empty() {
        targets.push(host_triple()?.to_owned());
    }
    let manifest = Manifest::load()?;
    match cmd.as_str() {
        "prepare" => {
            let fonts = ensure_fonts(&manifest)?;
            println!("deps: fonts at {}", fonts.display());
            let zig = ensure_zig(&manifest)?;
            let src = ensure_ghostty_source(&manifest)?;
            for target in &targets {
                let prefix = ensure_ghostty_build(&manifest, &zig, &src, target)?;
                println!("deps: libghostty-vt for {target} at {}", prefix.display());
            }
            Ok(())
        }
        "tmux" => {
            let tmux = ensure_tmux(&manifest)?;
            println!("{}", tmux.display());
            Ok(())
        }
        "fonts" => {
            println!("{}", ensure_fonts(&manifest)?.display());
            Ok(())
        }
        "path" => {
            for target in &targets {
                println!("{}", ghostty_prefix(&manifest, target)?.display());
            }
            Ok(())
        }
        other => Err(format!(
            "unknown deps command `{other}` (prepare, path, tmux, fonts)"
        )),
    }
}

pub fn deps_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("DREAMER_DEPS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library").join("Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".cache"))
    };
    Ok(base.join("dreamer").join("deps"))
}

pub fn host_triple() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => Err(format!("unsupported host {os}/{arch}")),
    }
}

/// Install prefix of `libghostty-vt` for a Rust target triple.
pub fn ghostty_prefix(manifest: &Manifest, target: &str) -> Result<PathBuf, String> {
    manifest.get("targets", target)?;
    Ok(deps_dir()?
        .join("ghostty")
        .join(manifest.get("ghostty", "version")?)
        .join("install")
        .join(target))
}

/// Build the pinned tmux into the cache and return the executable path.
/// Needs a C compiler, make, pkg-config, bison, and libevent/ncurses headers.
fn ensure_tmux(manifest: &Manifest) -> Result<PathBuf, String> {
    let version = manifest.get("tmux", "version")?;
    let root = deps_dir()?.join("tmux").join(version);
    let prefix = root.join("install");
    let bin = prefix.join("bin").join("tmux");
    if prefix.join(".complete").is_file() && bin.is_file() {
        return Ok(bin);
    }
    let url = manifest.get("tmux", "url")?;
    let sha = manifest.get("tmux", "sha256")?;
    let name = format!("tmux-{version}.tar.gz");
    let archive = download(url, &name, sha)?;
    let src = root.join("src");
    extract(&archive, &src, true)?;
    println!("deps: building tmux {version}");
    let run = |program: &str, args: &[&str]| -> Result<(), String> {
        let status = Command::new(program)
            .args(args)
            .current_dir(&src)
            .status()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`{program} {}` failed", args.join(" ")))
        }
    };
    let prefix_arg = format!("--prefix={}", prefix.display());
    run("./configure", &[&prefix_arg])?;
    let jobs = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .to_string();
    run("make", &["-j", &jobs])?;
    run("make", &["install"])?;
    std::fs::write(prefix.join(".complete"), sha).map_err(|e| e.to_string())?;
    Ok(bin)
}

fn zig_host_key() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-macos"),
        ("linux", "x86_64") => Ok("x86_64-linux"),
        ("linux", "aarch64") => Ok("aarch64-linux"),
        (os, arch) => Err(format!("no pinned Zig for host {os}/{arch}")),
    }
}

/// The pinned default font files (checksum-verified archive, then only the
/// files listed in the baseline). Returns the directory holding them.
pub fn ensure_fonts(manifest: &Manifest) -> Result<PathBuf, String> {
    let version = manifest.get("nerdfonts", "version")?;
    let files: Vec<&str> = manifest
        .get("nerdfonts", "files")?
        .split_whitespace()
        .collect();
    let dir = deps_dir()?.join("fonts").join("nerdfonts").join(version);
    if files.iter().all(|f| dir.join(f).is_file()) {
        return Ok(dir);
    }
    let name = format!("JetBrainsMono-nerdfonts-{version}.tar.xz");
    let archive = download(
        manifest.get("nerdfonts", "url")?,
        &name,
        manifest.get("nerdfonts", "sha256")?,
    )?;
    let all = dir.with_extension("all");
    extract(&archive, &all, false)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for f in &files {
        std::fs::copy(all.join(f), dir.join(f))
            .map_err(|e| format!("{f} missing from {name}: {e}"))?;
    }
    let _ = std::fs::remove_dir_all(&all);
    Ok(dir)
}

fn ensure_zig(manifest: &Manifest) -> Result<PathBuf, String> {
    let version = manifest.get("zig", "version")?;
    let key = zig_host_key()?;
    let sha = manifest.get("zig", key)?;
    let dir = deps_dir()?.join("zig").join(version).join(key);
    let zig = dir.join("zig");
    if zig.is_file() {
        return Ok(zig);
    }
    let name = format!("zig-{key}-{version}.tar.xz");
    let url = format!("{}/{name}", manifest.get("zig", "url_base")?);
    let archive = download(&url, &name, sha)?;
    extract(&archive, &dir, true)?;
    if !zig.is_file() {
        return Err(format!("{} missing after extracting {name}", zig.display()));
    }
    Ok(zig)
}

struct GhosttySource {
    src: PathBuf,
    deps: PathBuf,
}

fn ensure_ghostty_source(manifest: &Manifest) -> Result<GhosttySource, String> {
    let version = manifest.get("ghostty", "version")?;
    let base = manifest.get("ghostty", "url_base")?;
    let root = deps_dir()?.join("ghostty").join(version);
    let src = root.join("src");
    let deps = root.join("zig-packages");
    for (key, dest) in [("source", &src), ("deps", &deps)] {
        if dest.join(".complete").is_file() {
            continue;
        }
        let name = manifest.get("ghostty", key)?;
        let sha = manifest.get("ghostty", &format!("{key}_sha256"))?;
        let archive = download(&format!("{base}/{name}"), name, sha)?;
        extract(&archive, dest, true)?;
        std::fs::write(dest.join(".complete"), sha).map_err(|e| e.to_string())?;
    }
    let lib_version = std::fs::read_to_string(src.join("LIB_VERSION"))
        .map_err(|e| format!("source tree lacks LIB_VERSION: {e}"))?;
    if lib_version.trim() != version {
        return Err(format!(
            "source LIB_VERSION is {}, baseline pins {version}",
            lib_version.trim()
        ));
    }
    Ok(GhosttySource { src, deps })
}

fn ensure_ghostty_build(
    manifest: &Manifest,
    zig: &Path,
    source: &GhosttySource,
    target: &str,
) -> Result<PathBuf, String> {
    let prefix = ghostty_prefix(manifest, target)?;
    let zig_target = manifest.get("targets", target)?;
    let optimize = manifest.get("ghostty", "optimize")?;
    let cpu = manifest.get("ghostty", "cpu")?;
    let features = manifest.get("ghostty", "vt_features")?;
    let fingerprint = format!(
        "ghostty={} commit={} zig={} target={zig_target} optimize={optimize} cpu={cpu} features={features}\n",
        manifest.get("ghostty", "version")?,
        manifest.get("ghostty", "commit")?,
        manifest.get("zig", "version")?,
    );
    let marker = prefix.join(".dreamer-build");
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(fingerprint.as_str()) {
        return Ok(prefix);
    }

    println!("deps: building libghostty-vt for {target} ({zig_target}, {optimize})");
    // Install straight into the final prefix (installed pkg-config and CMake
    // files record it); the marker written last makes the build complete.
    let cache = prefix.with_extension("zig-cache");
    for dir in [&prefix, &cache] {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    let mut cmd = Command::new(zig);
    cmd.arg("build")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--system")
        .arg(&source.deps)
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dcpu={cpu}"))
        .arg(format!("-Dtarget={zig_target}"))
        .current_dir(&source.src)
        // Must be fresh: a populated Zig 0.17 cache can crash offline builds,
        // and an empty one proves only pinned packages were used.
        .env("ZIG_GLOBAL_CACHE_DIR", &cache)
        .env("ZIG_LOCAL_CACHE_DIR", cache.join("local"));
    if features != "default" {
        cmd.arg(format!("-Dvt-features={features}"));
    }
    let status = cmd
        .status()
        .map_err(|e| format!("could not run {}: {e}", zig.display()))?;
    if !status.success() {
        return Err(format!("zig build failed for {target}"));
    }
    if !prefix.join("lib").join("libghostty-vt.a").is_file() {
        return Err(format!("zig build produced no static library for {target}"));
    }
    std::fs::write(&marker, &fingerprint).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&cache);
    Ok(prefix)
}

/// Download `url` into the cache and verify its SHA-256. A cached file with
/// the right checksum is reused; a wrong one is replaced.
fn download(url: &str, name: &str, sha256: &str) -> Result<PathBuf, String> {
    let dir = deps_dir()?.join("downloads");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(name);
    if path.is_file() && file_sha256(&path)? == sha256 {
        return Ok(path);
    }
    println!("deps: downloading {url}");
    let partial = path.with_extension("part");
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
        ])
        .arg("--output")
        .arg(&partial)
        .arg(url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        return Err(format!("download failed: {url}"));
    }
    let actual = file_sha256(&partial)?;
    if actual != sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "checksum mismatch for {name}: expected {sha256}, got {actual}"
        ));
    }
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

fn file_sha256(path: &Path) -> Result<String, String> {
    crate::util::sha256_file(path)
}

/// Extract an archive into `dest`, atomically. With `strip`, the archive's
/// single top-level directory becomes `dest`.
fn extract(archive: &Path, dest: &Path, strip: bool) -> Result<(), String> {
    let staging = dest.with_extension("extracting");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut cmd = Command::new("tar");
    cmd.arg("-xf").arg(archive).arg("-C").arg(&staging);
    if strip {
        cmd.arg("--strip-components=1");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("could not run tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar failed on {}", archive.display()));
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| e.to_string())?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&staging, dest).map_err(|e| e.to_string())
}
