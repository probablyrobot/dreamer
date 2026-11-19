//! Web views for the desktop: Chromium, through the Chromium Embedded
//! Framework, drawing one page into a rectangle of a window.
//!
//! Chromium draws to the screen itself: on macOS each view is CEF's own
//! `NSView`, a child of the window's view, kept on the pane's rectangle. It
//! sits above everything the toolkit draws, so the caller hides it while an
//! overlay covers the pane, and can draw the page's capture instead.
//!
//! CEF's browser process is this process; its renderer, GPU, and utility
//! processes run the helper executable (`dreamer-web-helper`) from the
//! helper apps of Dreamer Web, the component that holds Chromium apart from
//! the app ([`web_component`]): the framework is loaded from there when the
//! first page opens, and a missing component is an error saying how to add
//! it. CEF's work is pumped from the caller's event loop:
//! [`start`] takes a function CEF calls, from any thread, when it wants
//! [`do_work`] called on the main thread.
//!
//! A plain page's view is the browser ([`browser`]): its own profile, no
//! Dreamer API, back and forward by mouse and swipe, and downloads saved
//! only after the pane's prompt. A view can instead show a page of an
//! app's own ([`AppPage`], see [`site`]): it then has a browser profile of
//! the app's, may navigate only to the app's origin and the origins it
//! allows, its downloads are refused unless the app may save them, and
//! pages on the app's origin get `window.dreamer.call`, passed on to the
//! caller.
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

/// What a page tells the pane drawing it, on the main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The page took the keyboard focus (a click in it).
    Focus,
    /// The main frame's load of `url` failed, and why.
    LoadFailed { url: String, error: String },
    /// A line for the pane's status (a refused download).
    Status(String),
    /// The page started download `id` of a file it names `name` (as the
    /// site suggests it; not yet safe as a file name), `size` bytes if
    /// known. It waits for the caller: `WebView::save_download` or
    /// `WebView::cancel_download`.
    DownloadAsked {
        id: u32,
        name: String,
        size: Option<u64>,
    },
    /// Download `id`, being saved, has `received` bytes of `total`.
    DownloadProgress {
        id: u32,
        received: u64,
        total: Option<u64>,
    },
    /// Download `id` is complete, at `path` (the path it was saved to).
    DownloadDone { id: u32, path: std::path::PathBuf },
    /// Download `id` ended unfinished: canceled, or failed (`error`).
    /// `partial` is the file Chromium was writing, if it named one.
    DownloadEnded {
        id: u32,
        error: Option<String>,
        partial: Option<std::path::PathBuf>,
    },
    /// Find in page: how many matches, which is current (from 1, or 0),
    /// and whether this is the last report for the search.
    Found {
        count: u32,
        current: u32,
        last: bool,
    },
}

/// The color space Chromium draws pages in (`web.color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// sRGB, as the rest of the window: wide-gamut colors are clamped.
    #[default]
    Srgb,
    /// The display's own color space. A page's capture is then in that
    /// space, and is converted to sRGB before the toolkit draws it
    /// ([`capture_to_srgb`]).
    Display,
}

/// A color space (Core Graphics), as a page's capture or a screen has.
#[cfg(target_os = "macos")]
pub use cef_sys::color::ColorSpace;

/// Convert a page's capture (`width` × `height` RGBA pixels, 8 bits per
/// channel) from `from`, the color space it was drawn in, to sRGB, in
/// place: colors beyond sRGB are clipped to it. Nothing changes when
/// `from` is sRGB. Takes a few milliseconds for a large page: never on
/// the UI thread.
#[cfg(target_os = "macos")]
pub fn capture_to_srgb(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    from: &ColorSpace,
) -> Result<(), String> {
    if from.is_srgb() {
        return Ok(());
    }
    let srgb = ColorSpace::srgb().ok_or("no sRGB color space")?;
    cef_sys::color::convert_rgba(rgba, width as usize, height as usize, from, &srgb)
}

/// Convert `rgba` from sRGB to `to` (the smoke's expected colors in a
/// screen capture).
#[cfg(target_os = "macos")]
pub fn srgb_to(rgba: &mut [u8], width: u32, height: u32, to: &ColorSpace) -> Result<(), String> {
    let srgb = ColorSpace::srgb().ok_or("no sRGB color space")?;
    cef_sys::color::convert_rgba(rgba, width as usize, height as usize, &srgb, to)
}

pub mod browser;
pub mod site;
pub use site::{AppPage, BridgeCall};

#[cfg(all(feature = "engine", target_os = "macos"))]
mod engine;
#[cfg(all(feature = "engine", target_os = "macos"))]
pub use engine::{
    WebView, color, do_work, profile_ready, run_subprocess, shutdown, start, started,
};
