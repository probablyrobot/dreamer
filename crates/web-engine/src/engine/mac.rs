//! CEF on macOS: the framework loaded from Dreamer Web, CEF's protocol on
//! the application object, and each page's own `NSView`, a child of the
//! window's view.

use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};

use cef_sys::cef::*;
use cef_sys::mac as appkit;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};

use crate::Rect;

/// A key event as the platform delivered it (unused).
pub(super) type OsEvent<'a> = *mut u8;

/// Chromium's switches for this platform: its cookie encryption key would
/// otherwise go into the login keychain, with a prompt.
pub(super) fn switches(cl: &mut CommandLine, _browser: bool) {
    cl.append_switch(Some(&CefString::from("use-mock-keychain")));
}

/// Load the framework library at `path` (once per process; it stays
/// loaded: unloading it would pull the framework out from under CEF).
fn load_framework(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{}: not a usable path", path.display()))?;
    // The binding takes the string as a reference to its first character.
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let first = unsafe { &*name.as_ptr() };
    if load_library(Some(first)) != 1 {
        return Err(format!(
            "the Chromium Embedded Framework at {} did not load",
            path.display()
        ));
    }
    // Declares the API version the wrapper was built for.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    Ok(())
}

/// The app bundle the running executable is in (`Dreamer.app` for
/// `Dreamer.app/Contents/MacOS/dreamer`), if it is in one.
fn app_bundle(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Dreamer Web, installed.
pub(super) struct Chromium {
    component: PathBuf,
}

impl Chromium {
    /// The installed component, if it is this build's and complete.
    pub(super) fn find(_window: &impl HasWindowHandle) -> Result<Chromium, String> {
        let installed = web_component::installed_dir();
        if let Some(problem) = web_component::check(&installed).problem(&installed) {
            return Err(problem);
        }
        // A copy used in place is linked to: Chromium is told where it
        // really is, so the helpers find the framework beside themselves.
        Ok(Chromium {
            component: installed.canonicalize().unwrap_or(installed),
        })
    }

    /// Load the framework and make the application object answer CEF's
    /// protocol.
    pub(super) fn load(&self) -> Result<(), String> {
        load_framework(&self.component.join(web_component::FRAMEWORK_BINARY))?;
        // SAFETY: main thread, framework loaded, NSApp exists (inside the
        // toolkit's run loop), before initialize.
        unsafe { appkit::adopt_application_protocol() }
    }

    /// The framework and the helper apps are outside the app's bundle, so
    /// CEF is told where. It passes these on to the helpers; the main
    /// bundle is named too, so they take the app's rather than their own.
    pub(super) fn settings(&self, settings: Settings) -> Result<Settings, String> {
        let path = |p: &Path| CefString::from(p.to_string_lossy().as_ref());
        let exe =
            std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
        Ok(Settings {
            framework_dir_path: path(&self.component.join(web_component::FRAMEWORK)),
            browser_subprocess_path: path(&web_component::helper_executable(&self.component, "")),
            main_bundle_path: app_bundle(&exe).as_deref().map(path).unwrap_or_default(),
            ..settings
        })
    }
}

/// In a helper: load the framework. The helper apps sit beside it in the
/// component's directory: from `dreamer Helper.app/Contents/MacOS/dreamer
/// Helper`, the framework is three directories up.
pub(super) fn load_in_helper() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let dir = exe
        .ancestors()
        .nth(4)
        .ok_or("not inside a helper app")?
        .to_path_buf();
    let path = dir.join(web_component::FRAMEWORK_BINARY);
    if !path.is_file() {
        return Err(format!("{} is missing", path.display()));
    }
    load_framework(&path)
}

/// A closing browser: its view is taken out of the window, which
/// completes the close (CEF would otherwise ask the window to close,
/// `performClose:`).
pub(super) fn close(browser: &mut Browser) -> i32 {
    if let Some(h) = browser.host() {
        // SAFETY: CEF's UI thread is the main thread; the view is the
        // closing browser's live view, or null.
        unsafe { appkit::remove_from_superview(h.window_handle().cast()) };
    }
    1
}

fn frame_of(r: Rect) -> appkit::Frame {
    appkit::Frame {
        x: f64::from(r.x),
        y: f64::from(r.y),
        width: f64::from(r.width.max(1.0)),
        height: f64::from(r.height.max(1.0)),
    }
}

/// The page's surface: CEF's view, a child of the window's view.
pub(super) struct Surface {
    /// The window's content view.
    parent: *mut c_void,
}

/// CEF's view of `browser`, or null.
fn view(browser: &Browser) -> *mut c_void {
    browser
        .host()
        .map_or(std::ptr::null_mut(), |h| h.window_handle().cast())
}

impl Surface {
    /// The `NSView` of a window, from its handle.
    pub(super) fn new(
        window: &(impl HasWindowHandle + HasDisplayHandle),
        _rect: Rect,
    ) -> Result<Surface, String> {
        let handle = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?;
        match handle.as_raw() {
            RawWindowHandle::AppKit(h) => Ok(Surface {
                parent: h.ns_view.as_ptr(),
            }),
            _ => Err("not an AppKit window".into()),
        }
    }

    /// The view is drawn by Chromium itself.
    pub(super) fn render_handler(&self) -> Option<RenderHandler> {
        None
    }

    pub(super) fn window_info(&self, rect: Rect) -> WindowInfo {
        // SAFETY: `parent` is the window's live content view; main thread.
        let frame = unsafe { appkit::from_top_left(self.parent, frame_of(rect)) };
        WindowInfo {
            parent_view: self.parent,
            bounds: cef_sys::cef::Rect {
                x: frame.x as i32,
                y: frame.y as i32,
                width: frame.width as i32,
                height: frame.height as i32,
            },
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        }
    }

    pub(super) fn opened(&self, _browser: &Browser) {}

    pub(super) fn place(&self, browser: &Browser, rect: Rect) {
        // SAFETY: the parent is the window's live content view and the
        // view is CEF's live child of it (or null); main thread.
        unsafe {
            let frame = appkit::from_top_left(self.parent, frame_of(rect));
            appkit::set_frame(view(browser), frame);
        }
    }

    pub(super) fn set_visible(&self, browser: &Browser, visible: bool) {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::set_hidden(view(browser), !visible) };
    }

    pub(super) fn take_focus(&self, browser: &Browser) {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::make_first_responder(view(browser)) };
    }

    /// Whether the page's view is first responder.
    pub(super) fn has_focus(&self, browser: &Browser) -> Option<bool> {
        // SAFETY: CEF's live view or null; main thread.
        Some(unsafe { appkit::has_first_responder(view(browser)) })
    }

    pub(super) fn give_focus_back(&self, _browser: &Browser) {
        // SAFETY: the window's live content view; main thread.
        unsafe { appkit::make_first_responder(self.parent) };
    }

    pub(super) fn click_side_button(
        &self,
        browser: &Browser,
        x: f32,
        y: f32,
        back: bool,
    ) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe {
            appkit::click_button(
                view(browser),
                f64::from(x),
                f64::from(y),
                if back { 3 } else { 4 },
            )
        }
    }

    pub(super) fn press_key(
        &self,
        browser: &Browser,
        key_code: u16,
        chars: &str,
        command: bool,
    ) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::press_key(view(browser), key_code, chars, command) }
    }

    pub(super) fn click(&self, browser: &Browser, x: f32, y: f32) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::click(view(browser), f64::from(x), f64::from(y)) }
    }

    pub(super) fn compose(
        &self,
        browser: &Browser,
        marked: &str,
        committed: &str,
    ) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::compose(view(browser), marked, committed) }
    }

    pub(super) fn window_number(&self) -> Option<isize> {
        // SAFETY: the window's live content view; main thread.
        unsafe { appkit::window_number(self.parent) }
    }

    pub(super) fn screen_color_space(&self) -> Option<crate::ColorSpace> {
        // SAFETY: the window's live content view; main thread.
        unsafe { appkit::screen_color_space(self.parent) }
    }
}
