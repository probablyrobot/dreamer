//! Link the pinned static libghostty-vt prepared by `cargo xtask deps prepare`.
//!
//! This script never downloads or builds anything. It reads the pins from
//! `deps/baseline.toml`, finds the matching install prefix in the dependency
//! cache, checks that its build fingerprint matches, and links the static
//! library. Override the cache with `DREAMER_DEPS_DIR`, or point at a prefix
//! directly with `DREAMER_GHOSTTY_PREFIX`.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let baseline = manifest_dir.join("../../deps/baseline.toml");
    println!("cargo:rerun-if-changed={}", baseline.display());
    println!("cargo:rerun-if-env-changed=DREAMER_DEPS_DIR");
    println!("cargo:rerun-if-env-changed=DREAMER_GHOSTTY_PREFIX");

    let text = std::fs::read_to_string(&baseline).expect("read deps/baseline.toml");
    let version = value(&text, "ghostty", "version");
    let commit = value(&text, "ghostty", "commit");
    let target = std::env::var("TARGET").expect("TARGET");

    let prefix = match std::env::var_os("DREAMER_GHOSTTY_PREFIX") {
        Some(p) => PathBuf::from(p),
        None => deps_dir()
            .join("ghostty")
            .join(&version)
            .join("install")
            .join(&target),
    };
    let marker = prefix.join(".dreamer-build");
    let fingerprint = std::fs::read_to_string(&marker).unwrap_or_default();
    let lib = prefix.join("lib").join("libghostty-vt.a");
    if !lib.is_file() || !fingerprint.contains(&format!("ghostty={version} commit={commit} ")) {
        panic!(
            "\n\nlibghostty-vt {version} for {target} is not prepared at {}.\n\
             Run: cargo xtask deps prepare --target {target}\n\n",
            prefix.display()
        );
    }
    println!("cargo:rerun-if-changed={}", marker.display());

    // Link from a directory that holds only the static archive: the macOS
    // linker prefers a shared library found beside it, and Apple's ld has no
    // way to name the archive file verbatim.
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("ghostty-static");
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

/// Minimal reader for the `key = "value"` subset used by deps/baseline.toml.
fn value(text: &str, section: &str, key: &str) -> String {
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
            return v.trim().trim_matches('"').to_owned();
        }
    }
    panic!("deps/baseline.toml: missing [{section}] {key}");
}
