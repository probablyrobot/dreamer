//! Web views for the desktop: Chromium, through the Chromium Embedded
//! Framework, drawing one page into a rectangle of a window.
//!
//! Chromium draws to the screen itself: on macOS each view is CEF's own
//! `NSView`, a child of the window's view, kept on the pane's rectangle. It
//! sits above everything the toolkit draws, so the caller hides it while an
//! overlay covers the pane, and can draw the page's capture instead.
//!
//! CEF's browser process is this process; its renderer, GPU, and utility
//! processes run the helper executable (`dreamer-web-helper`) from the app
//! bundle's helper apps. CEF's work is pumped from the caller's event loop:
//! [`start`] takes a function CEF calls, from any thread, when it wants
//! [`do_work`] called on the main thread.
//!
//! A view can show a page of an app's own ([`AppPage`], see [`site`]): it
//! then has a browser profile of the app's, may navigate only to the app's
//! origin and the origins it allows, and pages on the app's origin get
//! `window.dreamer.call`, passed on to the caller.
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

/// One of Chromium's own edit commands, which the desktop's edit shortcuts
/// become in a page: the clipboard, including rich and HTML contents,
/// works as in Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

pub mod site;
pub use site::{AppPage, BridgeCall};

#[cfg(all(feature = "engine", target_os = "macos"))]
mod engine;
#[cfg(all(feature = "engine", target_os = "macos"))]
pub use engine::{WebView, do_work, profile_ready, run_subprocess, shutdown, start, started};
