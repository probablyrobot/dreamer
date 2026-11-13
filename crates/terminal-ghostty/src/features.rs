//! Runtime feature probe. A linked symbol does not prove the optional
//! implementation behind it was compiled in; the build reports it.

use ghostty_sys::GhosttyBuildInfo::*;
use ghostty_sys::GhosttyResult::GHOSTTY_SUCCESS;
use ghostty_sys::*;

/// What the linked library was built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Features {
    pub version: String,
    pub simd: bool,
    pub kitty_graphics: bool,
    pub snapshot: bool,
    pub formatter: bool,
    pub selection: bool,
    pub search: bool,
    pub render_state: bool,
    pub input_encode: bool,
    pub color: bool,
    pub grid_introspection: bool,
}

impl Features {
    /// Names of required features this build lacks. Dreamer needs all of
    /// them; see `docs/dependency-baseline.md`.
    pub fn missing_required(&self) -> Vec<&'static str> {
        [
            ("kitty_graphics", self.kitty_graphics),
            ("snapshot", self.snapshot),
            ("formatter", self.formatter),
            ("selection", self.selection),
            ("search", self.search),
            ("render_state", self.render_state),
            ("input_encode", self.input_encode),
            ("color", self.color),
            ("grid_introspection", self.grid_introspection),
        ]
        .into_iter()
        .filter_map(|(name, present)| (!present).then_some(name))
        .collect()
    }
}

fn flag(info: GhosttyBuildInfo::Type) -> bool {
    let mut out = false;
    // SAFETY: boolean build-info keys write one `bool`.
    let rc = unsafe { ghostty_build_info(info, (&raw mut out).cast()) };
    rc == GHOSTTY_SUCCESS && out
}

pub fn features() -> Features {
    crate::png_decode::register();
    let mut s = GhosttyString::default();
    // SAFETY: VERSION_STRING writes a GhosttyString pointing at static data.
    let rc = unsafe { ghostty_build_info(GHOSTTY_BUILD_INFO_VERSION_STRING, (&raw mut s).cast()) };
    let version = if rc == GHOSTTY_SUCCESS {
        crate::borrowed_str(s.ptr, s.len)
    } else {
        String::new()
    };
    Features {
        version,
        simd: flag(GHOSTTY_BUILD_INFO_SIMD),
        kitty_graphics: flag(GHOSTTY_BUILD_INFO_KITTY_GRAPHICS),
        snapshot: flag(GHOSTTY_BUILD_INFO_FEATURE_SNAPSHOT),
        formatter: flag(GHOSTTY_BUILD_INFO_FEATURE_FORMATTER),
        selection: flag(GHOSTTY_BUILD_INFO_FEATURE_SELECTION),
        search: flag(GHOSTTY_BUILD_INFO_FEATURE_SEARCH),
        render_state: flag(GHOSTTY_BUILD_INFO_FEATURE_RENDER_STATE),
        input_encode: flag(GHOSTTY_BUILD_INFO_FEATURE_INPUT_ENCODE),
        color: flag(GHOSTTY_BUILD_INFO_FEATURE_COLOR),
        grid_introspection: flag(GHOSTTY_BUILD_INFO_FEATURE_GRID_INTROSPECTION),
    }
}
