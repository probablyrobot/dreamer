//! `cargo xtask deps prepare [--target <triple>]...`: fetch, verify, and build
//! pinned native dependencies (Zig and `libghostty-vt`). `cargo xtask deps
//! web` fetches the web engine's (CEF, cmake, Ninja), which only builds with
//! the desktop's `web` feature need.
//!
//! `cargo xtask deps prebuilt` packs the library for every shipped target
//! into the archives a crates.io build of `ghostty-sys` downloads, and pins
//! their SHA-256 in `crates/ghostty-sys/prebuilt.toml`; `--check` (also
//! run by `prepare`) fails when those pins name another libghostty-vt.
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
        .ok_or(
            "usage: cargo xtask deps <prepare|path|tmux|fonts|web|prebuilt [--check]> [--target <triple>]...",
        )?;
    let mut targets = Vec::new();
    let mut check = false;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Pins are never updated by this command; accepted for clarity.
            "--locked" => {}
            "--check" => check = true,
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
            check_prebuilt_pins(&manifest)?;
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
        "web" => {
            let web = ensure_web(&manifest)?;
            println!(
                "deps: CEF {} at {}",
                manifest.get("cef", "version")?,
                web.cef_dir.display()
            );
            println!("CEF_PATH={}", web.cef_path.display());
            for dir in &web.tools {
                println!("tools: {}", dir.display());
            }
            Ok(())
        }
        "prebuilt" if check => check_prebuilt_pins(&manifest),
        "prebuilt" => prebuilt(&manifest),
        other => Err(format!(
            "unknown deps command `{other}` (prepare, path, tmux, fonts, web, prebuilt)"
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

/// The web engine's build inputs: the CEF distribution, unpacked where the
/// `cef-dll-sys` build script looks (`CEF_PATH/<version>/<dir>`), and the
/// cmake and Ninja that build CEF's C++ wrapper.
#[derive(Debug)]
pub struct WebDeps {
    /// `CEF_PATH` for the build.
    pub cef_path: PathBuf,
    /// The unpacked distribution (holds the framework).
    pub cef_dir: PathBuf,
    /// The directories holding `cmake` and `ninja`, put first on `PATH`.
    pub tools: Vec<PathBuf>,
}

impl WebDeps {
    /// Environment for a cargo build with the `web` feature: the pinned
    /// distribution and tools.
    pub fn env(&self) -> Vec<(&'static str, std::ffi::OsString)> {
        let mut paths = self.tools.clone();
        if let Some(p) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&p));
        }
        vec![
            ("CEF_PATH", self.cef_path.clone().into_os_string()),
            ("PATH", std::env::join_paths(paths).unwrap_or_default()),
        ]
    }

    /// The CEF framework bundle.
    pub fn framework(&self) -> PathBuf {
        self.cef_dir.join("Chromium Embedded Framework.framework")
    }
}

/// The host the web engine is pinned for, or why there is none.
fn web_host() -> Result<&'static str, String> {
    match host_triple()? {
        t @ "aarch64-apple-darwin" => Ok(t),
        other => Err(format!(
            "web panes build on macOS (arm64) only so far, not on {other}"
        )),
    }
}

fn web_paths(manifest: &Manifest, host: &str) -> Result<WebDeps, String> {
    let deps = deps_dir()?;
    let cef_path = deps.join("cef");
    let cef_dir = cef_path
        .join(manifest.get("cef", "version")?)
        .join(manifest.get("cef", &format!("{host}-dir"))?);
    let cmake = deps
        .join("cmake")
        .join(manifest.get("cmake", "version")?)
        .join(host)
        .join(manifest.get("cmake", &format!("{host}-bin"))?);
    let ninja = deps
        .join("ninja")
        .join(manifest.get("ninja", "version")?)
        .join(host);
    Ok(WebDeps {
        cef_path,
        cef_dir,
        tools: vec![cmake, ninja],
    })
}

/// The web engine's dependencies if `cargo xtask deps web` has provisioned
/// them on this host; never downloads anything.
pub fn web_deps(manifest: &Manifest) -> Result<Option<WebDeps>, String> {
    let web = web_paths(manifest, web_host()?)?;
    let complete = web.cef_dir.join("archive.json").is_file()
        && web.framework().is_dir()
        && web.tools[0].join("cmake").is_file()
        && web.tools[1].join("ninja").is_file();
    Ok(complete.then_some(web))
}

/// Fetch (checksum-verified) and unpack the CEF distribution, cmake, and
/// Ninja for this host.
pub fn ensure_web(manifest: &Manifest) -> Result<WebDeps, String> {
    let host = web_host()?;
    let web = web_paths(manifest, host)?;
    let deps = deps_dir()?;
    // cmake: the release's universal tarball, its one top directory dropped.
    if !web.tools[0].join("cmake").is_file() {
        let version = manifest.get("cmake", "version")?;
        let archive = download(
            manifest.get("cmake", host)?,
            &format!("cmake-{version}-macos-universal.tar.gz"),
            manifest.get("cmake", &format!("{host}-sha256"))?,
        )?;
        extract(&archive, &deps.join("cmake").join(version).join(host), true)?;
    }
    // Ninja: a zip holding the one executable.
    if !web.tools[1].join("ninja").is_file() {
        let version = manifest.get("ninja", "version")?;
        let archive = download(
            manifest.get("ninja", host)?,
            &format!("ninja-{version}-mac.zip"),
            manifest.get("ninja", &format!("{host}-sha256"))?,
        )?;
        extract(&archive, &web.tools[1], false)?;
    }
    // CEF: laid out as `<version>/<dir>` the way the cef crate's own
    // download does it (the `Release` directory, with the headers, wrapper
    // sources, and cmake files beside it), then the `archive.json` that the
    // cef-dll-sys build script checks the version against, written last.
    if !web.cef_dir.join("archive.json").is_file() {
        let name = manifest.get("cef", &format!("{host}-name"))?;
        let archive = download(
            manifest.get("cef", host)?,
            name,
            manifest.get("cef", &format!("{host}-sha256"))?,
        )?;
        let staging = web.cef_dir.with_file_name("unpacked");
        extract(&archive, &staging, true)?;
        if web.cef_dir.exists() {
            std::fs::remove_dir_all(&web.cef_dir).map_err(|e| e.to_string())?;
        }
        std::fs::rename(staging.join("Release"), &web.cef_dir)
            .map_err(|e| format!("{name} has no Release directory: {e}"))?;
        for item in [
            "CMakeLists.txt",
            "cmake",
            "include",
            "libcef_dll",
            "CREDITS.html",
            "LICENSE.txt",
        ] {
            std::fs::rename(staging.join(item), web.cef_dir.join(item))
                .map_err(|e| format!("{name}: {item}: {e}"))?;
        }
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
        let sha1 = manifest.get("cef", &format!("{host}-sha1"))?;
        let json = format!(
            "{{\n  \"type\": \"minimal\",\n  \"name\": \"{name}\",\n  \"sha1\": \"{sha1}\"\n}}\n"
        );
        std::fs::write(web.cef_dir.join("archive.json"), json).map_err(|e| e.to_string())?;
    }
    web_deps(manifest)?.ok_or_else(|| {
        format!(
            "the web dependencies are incomplete after unpacking; remove {} and run again",
            web.cef_dir.display()
        )
    })
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
    let dir = deps_dir()?.join("fonts").join("nerdfonts").join(version);
    // (section, archive name, files to copy as (from, to)).
    let mut sets = vec![(
        "nerdfonts",
        "JetBrainsMono".to_owned(),
        manifest
            .get("nerdfonts", "files")?
            .split_whitespace()
            .map(|f| (f.to_owned(), f.to_owned()))
            .collect::<Vec<_>>(),
    )];
    for section in manifest.get("nerdfonts", "extra")?.split_whitespace() {
        let mut files: Vec<(String, String)> = manifest
            .get(section, "files")?
            .split_whitespace()
            .map(|f| (f.to_owned(), f.to_owned()))
            .collect();
        files.push((
            manifest.get(section, "license")?.to_owned(),
            manifest.get(section, "license_as")?.to_owned(),
        ));
        sets.push((section, section.to_owned(), files));
    }
    for (section, label, files) in sets {
        if files.iter().all(|(_, to)| dir.join(to).is_file()) {
            continue;
        }
        let name = format!("{label}-nerdfonts-{version}.tar.xz");
        let archive = download(
            manifest.get(section, "url")?,
            &name,
            manifest.get(section, "sha256")?,
        )?;
        let all = dir.with_extension(format!("{label}.all"));
        extract(&archive, &all, false)?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for (from, to) in &files {
            std::fs::copy(all.join(from), dir.join(to))
                .map_err(|e| format!("{from} missing from {name}: {e}"))?;
        }
        let _ = std::fs::remove_dir_all(&all);
    }
    // Settings find a font by its family name: every shipped file must
    // carry the family its manifest section says.
    let mut sections = vec!["nerdfonts"];
    sections.extend(manifest.get("nerdfonts", "extra")?.split_whitespace());
    for section in sections {
        let family = manifest.get(section, "family")?;
        for f in manifest.get(section, "files")?.split_whitespace() {
            if !f.ends_with(".ttf") {
                continue;
            }
            let bytes = std::fs::read(dir.join(f)).map_err(|e| format!("{f}: {e}"))?;
            let names = font_families(&bytes).map_err(|e| format!("{f}: {e}"))?;
            if !names.iter().any(|n| n == family) {
                return Err(format!(
                    "{f} names its family {names:?}, not `{family}` (deps/baseline.toml [{section}])"
                ));
            }
        }
    }
    Ok(dir)
}

/// The family names in a TrueType file's `name` table (IDs 1 and 16, the
/// Windows Unicode and Mac Roman records).
pub fn font_families(font: &[u8]) -> Result<Vec<String>, String> {
    let u16_at = |o: usize| -> Result<usize, String> {
        font.get(o..o + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .ok_or_else(|| "truncated font".to_string())
    };
    let u32_at = |o: usize| -> Result<usize, String> {
        font.get(o..o + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            .ok_or_else(|| "truncated font".to_string())
    };
    let tables = u16_at(4)?;
    let mut name_at = None;
    for i in 0..tables {
        let rec = 12 + 16 * i;
        if font.get(rec..rec + 4) == Some(b"name") {
            name_at = Some(u32_at(rec + 8)?);
        }
    }
    let base = name_at.ok_or("no name table")?;
    let count = u16_at(base + 2)?;
    let strings = base + u16_at(base + 4)?;
    let mut out = Vec::new();
    for i in 0..count {
        let r = base + 6 + 12 * i;
        let (platform, encoding, id) = (u16_at(r)?, u16_at(r + 2)?, u16_at(r + 6)?);
        if id != 1 && id != 16 {
            continue;
        }
        let (len, off) = (u16_at(r + 8)?, u16_at(r + 10)?);
        let raw = font
            .get(strings + off..strings + off + len)
            .ok_or("truncated name")?;
        let name = match (platform, encoding) {
            (3, 1) | (0, _) => {
                let units: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                String::from_utf16_lossy(&units)
            }
            (1, 0) => raw.iter().map(|b| char::from(*b)).collect(),
            _ => continue,
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out)
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

/// Targets the prebuilt archives cover: every target Dreamer ships.
const PREBUILT_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
];

/// The GitHub release that holds one libghostty-vt build's archives, named
/// for the build so the pins move only when the library does.
fn prebuilt_tag(manifest: &Manifest) -> Result<String, String> {
    let version = manifest.get("ghostty", "version")?;
    let commit = manifest.get("ghostty", "commit")?;
    Ok(format!("libghostty-vt-{version}-{}", &commit[..12]))
}

fn prebuilt_url_base(manifest: &Manifest) -> Result<String, String> {
    Ok(format!(
        "https://github.com/probablyrobot/dreamer/releases/download/{}",
        prebuilt_tag(manifest)?
    ))
}

fn prebuilt_pins_path() -> PathBuf {
    crate::util::repo_root().join("crates/ghostty-sys/prebuilt.toml")
}

/// `crates/ghostty-sys/prebuilt.toml` names the pinned libghostty-vt and
/// has an archive for every shipped target.
fn check_prebuilt_pins(manifest: &Manifest) -> Result<(), String> {
    let path = prebuilt_pins_path();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pins = Manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let stale = |what: &str| {
        format!(
            "{} pins another libghostty-vt ({what} differs from deps/baseline.toml); \
             run `cargo xtask deps prebuilt` and upload the archives",
            path.display()
        )
    };
    for key in ["version", "commit"] {
        if pins.get("ghostty", key)? != manifest.get("ghostty", key)? {
            return Err(stale(key));
        }
    }
    if pins.get("ghostty", "url_base")? != prebuilt_url_base(manifest)? {
        return Err(stale("url_base"));
    }
    for target in PREBUILT_TARGETS {
        let sha = pins.get(target, "sha256")?;
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "{}: [{target}] sha256 is not a SHA-256",
                path.display()
            ));
        }
        pins.get(target, "file")?;
    }
    Ok(())
}

/// Build libghostty-vt for every shipped target, pack each prefix with the
/// licenses into `target/ghostty-prebuilt/`, and write the pins.
fn prebuilt(manifest: &Manifest) -> Result<(), String> {
    let root = crate::util::repo_root();
    let zig = ensure_zig(manifest)?;
    let src = ensure_ghostty_source(manifest)?;
    let version = manifest.get("ghostty", "version")?;
    let commit = manifest.get("ghostty", "commit")?;
    let out = root.join("target/ghostty-prebuilt");
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut pins = format!(
        "# Written by `cargo xtask deps prebuilt`; checked by `cargo xtask deps prepare`.\n\
         # A crates.io build of this crate downloads its target's archive from\n\
         # url_base and refuses it unless the SHA-256 matches.\n\
         [ghostty]\nversion = \"{version}\"\ncommit = \"{commit}\"\nurl_base = \"{}\"\n",
        prebuilt_url_base(manifest)?
    );
    for target in PREBUILT_TARGETS {
        let prefix = ensure_ghostty_build(manifest, &zig, &src, target)?;
        let stage = out.join("stage").join(target);
        std::fs::create_dir_all(stage.join("lib")).map_err(|e| e.to_string())?;
        for (from, to) in [
            (prefix.join(".dreamer-build"), ".dreamer-build"),
            (prefix.join("lib/libghostty-vt.a"), "lib/libghostty-vt.a"),
            (src.src.join("LICENSE"), "LICENSE-ghostty"),
            (
                root.join("THIRD_PARTY_NOTICES.md"),
                "THIRD_PARTY_NOTICES.md",
            ),
        ] {
            std::fs::copy(&from, stage.join(to)).map_err(|e| format!("{}: {e}", from.display()))?;
        }
        crate::package::run_checked(
            Command::new("cp")
                .arg("-R")
                .arg(prefix.join("include"))
                .arg(&stage),
            "cp include",
        )?;
        let file = format!("libghostty-vt-{version}-{}-{target}.tar.gz", &commit[..12]);
        let archive = out.join(&file);
        let owner: &[&str] = if cfg!(target_os = "macos") {
            &["--uid", "0", "--gid", "0", "--numeric-owner"]
        } else {
            &["--owner=0", "--group=0", "--numeric-owner"]
        };
        crate::package::run_checked(
            Command::new("tar")
                .env("COPYFILE_DISABLE", "1")
                .args(owner)
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&stage)
                .args([
                    ".dreamer-build",
                    "LICENSE-ghostty",
                    "THIRD_PARTY_NOTICES.md",
                    "lib",
                    "include",
                ]),
            "tar",
        )?;
        let sha = crate::util::sha256_file(&archive)?;
        std::fs::write(
            out.join(format!("{file}.sha256")),
            format!("{sha}  {file}\n"),
        )
        .map_err(|e| e.to_string())?;
        println!("deps: {} {sha}", archive.display());
        pins.push_str(&format!(
            "\n[{target}]\nfile = \"{file}\"\nsha256 = \"{sha}\"\n"
        ));
    }
    std::fs::remove_dir_all(out.join("stage")).map_err(|e| e.to_string())?;
    let path = prebuilt_pins_path();
    std::fs::write(&path, pins).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("deps: pins written to {}", path.display());
    println!(
        "deps: upload them as release {}: gh release create {0} --prerelease --latest=false target/ghostty-prebuilt/*",
        prebuilt_tag(manifest)?
    );
    Ok(())
}
