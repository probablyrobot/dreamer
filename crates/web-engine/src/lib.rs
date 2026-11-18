//! Web views for the desktop: Chromium, through the Chromium Embedded
//! Framework, drawing one page into a rectangle of a window.
//!
//! Chromium draws to the screen itself: on macOS each view is CEF's own
//! `NSView`, a child of the window's view, kept on the pane's rectangle. It
//! sits above everything the toolkit draws, so the caller hides it while an
//! overlay covers the pane.
//!
//! CEF's browser process is this process; its renderer, GPU, and utility
//! processes run the helper executable (`dreamer-web-helper`) from the app
//! bundle's helper apps. CEF's work is pumped from the caller's event loop:
//! [`start`] takes a function CEF calls, from any thread, when it wants
//! [`do_work`] called on the main thread.
//!
//! Everything here is main-thread only. Without the `engine` feature (and
//! on platforms not supported yet) the crate has no engine and
//! [`AVAILABLE`] is false.
#![allow(unsafe_code)]

/// Whether this build can show web pages.
pub const AVAILABLE: bool = cfg!(all(feature = "engine", target_os = "macos"));

/// A rectangle in a window's content, in logical pixels from its top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[cfg(all(feature = "engine", target_os = "macos"))]
mod engine;
#[cfg(all(feature = "engine", target_os = "macos"))]
pub use engine::{WebView, do_work, run_subprocess, shutdown, start, started};
