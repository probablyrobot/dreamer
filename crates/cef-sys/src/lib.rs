//! The Chromium Embedded Framework at the pinned release, for the desktop's
//! web panes: the `cef` crate (generated bindings and CEF's reference
//! counting), re-exported, and the few platform calls embedding needs.
//!
//! Everything here is either `unsafe` or a thin binding. The rest of the
//! workspace goes through the safe `web-engine` crate. Only the desktop
//! may depend on this crate; the headless executables never link CEF
//! (`cargo xtask headless --check`).
//!
//! Without the `cef` feature the crate holds only the Core Graphics color
//! calls on macOS ([`color`]), so workspace builds that do not ask for web
//! panes need no CEF distribution.
#![allow(unsafe_code)]

#[cfg(feature = "cef")]
pub use cef;

#[cfg(all(feature = "cef", target_os = "macos"))]
pub mod mac;

#[cfg(target_os = "macos")]
pub mod color;
