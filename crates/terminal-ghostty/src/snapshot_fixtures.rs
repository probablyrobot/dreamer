//! Cross-architecture snapshot fixtures (spec section 11.5).
//!
//! Each source stream in `tests/fixtures/snapshot-src/*.vt` is fed to an
//! engine and snapshotted on every supported architecture; the snapshots are
//! checked in under `tests/fixtures/snapshots/<target-triple>/`. Every lane
//! decodes every architecture's snapshots and compares the result with a
//! native replay of the same stream. An optional `*.cont` stream is then fed
//! to both, which proves unfinished parser input survived the transfer.

use std::path::{Path, PathBuf};

use terminal_core::GridSize;

use crate::{Engine, EngineConfig, EngineError};

/// Grid used for every fixture.
pub const SIZE: GridSize = GridSize { cols: 40, rows: 8 };
/// Continuation tracking limit used for fixtures.
pub const CONTINUATION: usize = 1024;

/// Engine configuration shared by fixture producers and consumers.
pub fn config() -> EngineConfig {
    let mut c = EngineConfig::new(SIZE);
    c.scrollback_bytes = Some(usize::MAX);
    c.continuation_max_bytes = Some(CONTINUATION);
    c
}

/// The Rust target triple this binary was built for.
pub fn this_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => "unsupported",
    }
}

/// Every architecture that must have checked-in snapshots.
pub const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

pub fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Names of the source streams, sorted.
pub fn sources() -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(fixture_root().join("snapshot-src"))?
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".vt"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    Ok(names)
}

pub fn source_bytes(name: &str, ext: &str) -> std::io::Result<Option<Vec<u8>>> {
    let path = fixture_root()
        .join("snapshot-src")
        .join(format!("{name}.{ext}"));
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Engine with the source stream applied natively.
pub fn replay(name: &str) -> Result<Engine, String> {
    let bytes = source_bytes(name, "vt")
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("missing source {name}.vt"))?;
    let mut e = Engine::new(config()).map_err(|e| e.to_string())?;
    e.feed(&bytes).map_err(|e| e.to_string())?;
    Ok(e)
}

/// Everything the fixtures compare: visible cells with styles, cursor,
/// full text including scrollback, scrollback size, and a set of modes.
/// Process-local identifiers (image generations, row ids) are excluded.
pub fn semantic_state(e: &mut Engine) -> Result<String, EngineError> {
    let mut out = e.frame()?.style_dump();
    out.push_str("-- screen\n");
    out.push_str(&e.screen_text()?);
    out.push_str(&format!("\n-- scrollback {}\n", e.scrollback_rows()?));
    for (n, ansi) in [
        (1, false),
        (4, true),
        (6, false),
        (7, false),
        (25, false),
        (47, false),
        (1049, false),
        (2004, false),
    ] {
        out.push_str(&format!(
            "mode {}{n} {}\n",
            if ansi { "" } else { "?" },
            e.mode(n, ansi)?
        ));
    }
    Ok(out)
}

/// Write this architecture's snapshots into `dir`.
pub fn generate(dir: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut written = Vec::new();
    for name in sources().map_err(|e| e.to_string())? {
        let e = replay(&name)?;
        let snap = e.snapshot().map_err(|e| format!("{name}: {e}"))?;
        let path = dir.join(format!("{name}.snap"));
        std::fs::write(&path, snap).map_err(|e| e.to_string())?;
        written.push(path);
    }
    Ok(written)
}
