//! Raw bindings to the pinned `libghostty-vt` C API.
//!
//! Everything here is `unsafe` to use. The rest of the workspace goes through
//! the safe wrapper in `terminal-ghostty`; no other crate may depend on this
//! one directly.

#![allow(unsafe_code)]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(missing_debug_implementations, clippy::all)]

mod bindings {
    include!("bindings.rs");
}

pub use bindings::*;

/// Release version this crate was linked against, from `deps/baseline.toml`.
pub const LINKED_VERSION: &str = env!("DREAMER_GHOSTTY_VERSION");
/// Upstream source commit of that release.
pub const LINKED_COMMIT: &str = env!("DREAMER_GHOSTTY_COMMIT");
