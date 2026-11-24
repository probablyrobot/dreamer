//! The engine build family: peers that exchange terminal
//! snapshots must have been built from the same terminal semantics.
//!
//! The family covers the Ghostty source revision, the features Dreamer
//! requires of it, and Dreamer's own terminal policy revision. It never
//! includes architecture or executable hashes; a macOS and a Linux build of
//! the same revision are one family. `terminal-ghostty` tests that the
//! linked library matches these constants.

/// The pinned libghostty-vt release (`deps/baseline.toml`).
pub const GHOSTTY_VERSION: &str = "1.0.1";
/// The pinned libghostty-vt source commit (`deps/baseline.toml`).
pub const GHOSTTY_COMMIT: &str = "108cacb17ed8990840cd14abfce43d8ed8503ca1";
/// Build features Dreamer requires; a build without one is refused.
pub const REQUIRED_FEATURES: &[&str] = &[
    "kitty_graphics",
    "snapshot",
    "formatter",
    "selection",
    "search",
    "render_state",
    "input_encode",
    "color",
    "grid_introspection",
];
/// Bump when Dreamer changes how it configures or interprets the engine
/// (parser limits, width or Unicode settings, reply policy, restore rules).
///
/// 2: Kitty image storage limit set explicitly on every engine and on
/// snapshot restore ([`IMAGE_STORAGE_BYTES`]).
pub const TERMINAL_POLICY_REVISION: u32 = 2;

/// Unfinished input a canonical engine keeps so a snapshot can be taken
/// mid-sequence; replicas restore with the same limit.
pub const CONTINUATION_MAX_BYTES: usize = 64 << 10;

/// Kitty graphics image storage per engine (64 MiB per pane).
/// Eviction follows it, so mirrors and replicas must agree; it is not
/// serialized in snapshots and is applied again on restore.
pub const IMAGE_STORAGE_BYTES: u64 = 64 << 20;

/// Cell size in pixels that daemon mirrors and replicas apply on resize
/// until clients negotiate canonical geometry.
pub const CANONICAL_CELL_PX: (u32, u32) = (10, 20);

/// A stable identifier such as `ghostty-1.0.1-108cacb17ed8-f1a2b3c4-p1`.
pub fn build_family() -> String {
    // FNV-1a over the feature list: stable across platforms and releases.
    let mut h: u32 = 0x811c_9dc5;
    for f in REQUIRED_FEATURES {
        for b in f.bytes().chain(std::iter::once(0)) {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    format!(
        "ghostty-{GHOSTTY_VERSION}-{}-{h:08x}-p{TERMINAL_POLICY_REVISION}",
        &GHOSTTY_COMMIT[..12]
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn family_is_stable() {
        // Changing this string splits peers into incompatible families;
        // update it only together with a deliberate engine or policy change.
        assert_eq!(
            super::build_family(),
            "ghostty-1.0.1-108cacb17ed8-9288da2a-p2"
        );
    }
}
