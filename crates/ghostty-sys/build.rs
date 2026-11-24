//! Link the pinned static libghostty-vt.
//!
//! In the repository, this script never downloads or builds anything. It
//! reads the pins from `deps/baseline.toml`, finds the matching install
//! prefix that `cargo xtask deps prepare` made in the dependency cache,
//! checks its build fingerprint, and links the static library.
//!
//! Built from crates.io (no `deps/baseline.toml` beside the crate), it uses
//! the prebuilt archive pinned in `prebuilt.toml`: the file named by
//! `DREAMER_GHOSTTY_ARCHIVE`, or one downloaded from Dreamer's GitHub
//! release. Either way the archive is refused unless its SHA-256 matches
//! the pin.
//!
//! `DREAMER_GHOSTTY_PREFIX` points at a prepared prefix directly, and
//! `DREAMER_DEPS_DIR` moves the dependency cache.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=DREAMER_DEPS_DIR");
    println!("cargo:rerun-if-env-changed=DREAMER_GHOSTTY_PREFIX");
    println!("cargo:rerun-if-env-changed=DREAMER_GHOSTTY_ARCHIVE");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    // docs.rs builds documentation only, offline: nothing to link.
    if std::env::var_os("DOCS_RS").is_some() {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let target = std::env::var("TARGET").expect("TARGET");
    let baseline = manifest_dir.join("../../deps/baseline.toml");
    let pins = manifest_dir.join("prebuilt.toml");

    let in_repo = baseline.is_file();
    let pin_file = if in_repo { &baseline } else { &pins };
    println!("cargo:rerun-if-changed={}", pin_file.display());
    let text = std::fs::read_to_string(pin_file)
        .unwrap_or_else(|e| panic!("read {}: {e}", pin_file.display()));
    let version = value(&text, "ghostty", "version");
    let commit = value(&text, "ghostty", "commit");

    let prefix = if let Some(p) = std::env::var_os("DREAMER_GHOSTTY_PREFIX") {
        PathBuf::from(p)
    } else if in_repo {
        deps_dir()
            .join("ghostty")
            .join(&version)
            .join("install")
            .join(&target)
    } else {
        prebuilt(&text, &target, &out_dir)
    };

    let marker = prefix.join(".dreamer-build");
    let fingerprint = std::fs::read_to_string(&marker).unwrap_or_default();
    let lib = prefix.join("lib").join("libghostty-vt.a");
    if !lib.is_file() || !fingerprint.contains(&format!("ghostty={version} commit={commit} ")) {
        if in_repo {
            panic!(
                "\n\nlibghostty-vt {version} for {target} is not prepared at {}.\n\
                 Run: cargo xtask deps prepare --target {target}\n\n",
                prefix.display()
            );
        }
        panic!(
            "\n\nlibghostty-vt {version} at {} is not the pinned build \
             (commit {commit}).\n\n",
            prefix.display()
        );
    }
    println!("cargo:rerun-if-changed={}", marker.display());

    // Link from a directory that holds only the static archive: the macOS
    // linker prefers a shared library found beside it, and Apple's ld has no
    // way to name the archive file verbatim.
    let out = out_dir.join("ghostty-static");
    std::fs::create_dir_all(&out).expect("create link dir");
    let dest = out.join("libghostty-vt.a");
    let _ = std::fs::remove_file(&dest);
    if std::fs::hard_link(&lib, &dest).is_err() {
        std::fs::copy(&lib, &dest).expect("copy libghostty-vt.a");
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    // Exposed to dependents as DEP_GHOSTTY_VT_INCLUDE / _VERSION / _COMMIT.
    println!("cargo:include={}", prefix.join("include").display());
    println!("cargo:version={version}");
    println!("cargo:commit={commit}");
    println!("cargo:rustc-env=DREAMER_GHOSTTY_VERSION={version}");
    println!("cargo:rustc-env=DREAMER_GHOSTTY_COMMIT={commit}");
}

/// The pinned prebuilt archive for `target`, checked and unpacked under
/// `out_dir`; returns the prefix it unpacked to.
fn prebuilt(pins: &str, target: &str, out_dir: &Path) -> PathBuf {
    let Some(file) = try_value(pins, target, "file") else {
        panic!(
            "\n\nno prebuilt libghostty-vt for {target}. Build it with Dreamer's \
             `cargo xtask deps prepare --target {target}` and set \
             DREAMER_GHOSTTY_PREFIX to the prefix it prints.\n\n"
        );
    };
    let sha256 = value(pins, target, "sha256");
    let dir = out_dir.join("ghostty-prebuilt");
    let prefix = dir.join(target);
    let done = prefix.join(".dreamer-sha256");
    if std::fs::read_to_string(&done).is_ok_and(|s| s.trim() == sha256) {
        return prefix;
    }
    std::fs::create_dir_all(&dir).expect("create prebuilt dir");

    let archive = match std::env::var_os("DREAMER_GHOSTTY_ARCHIVE") {
        Some(p) => PathBuf::from(p),
        None => {
            let url = format!("{}/{file}", value(pins, "ghostty", "url_base"));
            let dest = dir.join(&file);
            let status = Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "--location"])
                .args(["--retry", "3", "--output"])
                .arg(&dest)
                .arg(&url)
                .status();
            match status {
                Ok(s) if s.success() => dest,
                Ok(s) => panic!("\n\ncurl {url}: {s}\n\n"),
                Err(e) => panic!(
                    "\n\ncould not run curl to fetch {url}: {e}\n\
                     Download it yourself and set DREAMER_GHOSTTY_ARCHIVE to the file.\n\n"
                ),
            }
        }
    };
    let bytes =
        std::fs::read(&archive).unwrap_or_else(|e| panic!("read {}: {e}", archive.display()));
    let got = hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    if got != sha256 {
        panic!(
            "\n\n{} has SHA-256 {got}, but the pin for {target} is {sha256}; refusing it.\n\n",
            archive.display()
        );
    }

    let _ = std::fs::remove_dir_all(&prefix);
    std::fs::create_dir_all(&prefix).expect("create prefix");
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&prefix)
        .status()
        .unwrap_or_else(|e| panic!("run tar: {e}"));
    assert!(status.success(), "tar -xzf {}: {status}", archive.display());
    std::fs::write(&done, &sha256).expect("record unpacked archive");
    prefix
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Same lookup as `xtask::deps::deps_dir`.
fn deps_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DREAMER_DEPS_DIR") {
        return PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"));
    // Build scripts run on the host, which is where the cache lives.
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
    };
    base.join("dreamer").join("deps")
}

fn value(text: &str, section: &str, key: &str) -> String {
    try_value(text, section, key).unwrap_or_else(|| panic!("missing [{section}] {key}"))
}

/// Minimal reader for the `key = "value"` subset the pin files use.
fn try_value(text: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == header;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().trim_matches('"').to_owned());
        }
    }
    None
}
