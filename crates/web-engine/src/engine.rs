//! CEF: starting it, the external message pump, and browsers drawn over a
//! rectangle of a window. What differs by platform (where Chromium is
//! loaded from, and the surface a page is drawn in) is in [`mac`] and
//! [`linux`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cef_sys::cef::{self, *};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::Rect;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use mac as platform;

// The component's CEF release must be the one these bindings are for: a
// component checked against it has a framework they can call.
const _: () = assert!(
    same_version(web_component::CEF_VERSION.as_bytes(), sys::CEF_VERSION),
    "web_component::CEF_VERSION is not the CEF release the cef crate binds"
);

/// Whether `ours` is `cef`'s version string (which ends in a NUL).
const fn same_version(ours: &[u8], cef: &[u8]) -> bool {
    if ours.len() + 1 != cef.len() {
        return false;
    }
    let mut i = 0;
    while i < ours.len() {
        if ours[i] != cef[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Called by CEF, from any thread, when it wants [`do_work`] after the
/// given delay in milliseconds.
static WAKE: OnceLock<Box<dyn Fn(i64) + Send + Sync>> = OnceLock::new();
/// Browsers created and not yet closed.
static LIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The outcome of [`start`], once tried.
    static STARTED: RefCell<Option<Result<(), String>>> = const { RefCell::new(None) };
}

/// Whether Chromium draws pages in the display's color space
/// (`web.color: display`), as [`start`] was told; read as CEF initializes
/// and as it launches each helper process.
static DISPLAY_COLOR: AtomicBool = AtomicBool::new(false);

/// Pages rasterized in sRGB, as the rest of the window is drawn, unless
/// `web.color` is `display`: wide-gamut colors are clamped to sRGB, and a
/// page's capture (its snapshot under an overlay) has the colors the live
/// page shows. Chromium still matches its output to the screen, as the
/// toolkit does. (`force-color-profile` would instead tell Chromium the
/// screen is sRGB, so its output reaches a wide-gamut screen unmatched and
/// oversaturated.) Without the switch Chromium draws in the display's
/// color space.
fn force_srgb(cl: &mut CommandLine) {
    if !DISPLAY_COLOR.load(Ordering::Relaxed) {
        cl.append_switch_with_value(
            Some(&CefString::from("force-raster-color-profile")),
            Some(&CefString::from("srgb")),
        );
    }
}

/// The color space Chromium was started with ([`start`]); `Srgb` before
/// it starts.
pub fn color() -> crate::Color {
    if started() && DISPLAY_COLOR.load(Ordering::Relaxed) {
        crate::Color::Display
    } else {
        crate::Color::Srgb
    }
}

/// The process's arguments as CEF reads them, kept for the process: only
/// the executable, so the desktop's own options never reach Chromium.
struct MainArgv {
    _strings: Vec<CString>,
    pointers: Vec<*const c_char>,
}

impl MainArgv {
    fn executable() -> Self {
        let exe = std::env::args().next().unwrap_or_else(|| "dreamer".into());
        let strings = vec![CString::new(exe).unwrap_or_default()];
        let pointers = strings.iter().map(|s| s.as_ptr()).collect();
        Self {
            _strings: strings,
            pointers,
        }
    }

    fn main_args(&mut self) -> MainArgs {
        MainArgs {
            argc: self.pointers.len() as i32,
            argv: self.pointers.as_mut_ptr().cast(),
        }
    }
}

mod handlers {
    #![allow(clippy::too_many_arguments)]
    use super::*;

    wrap_app! {
        pub struct DreamerApp;
        impl App {
            fn on_before_command_line_processing(
                &self,
                process_type: Option<&CefString>,
                command_line: Option<&mut CommandLine>,
            ) {
                let Some(cl) = command_line else { return };
                let browser = process_type.is_none_or(|t| t.to_string().is_empty());
                // Chromium's cookie encryption key would otherwise go into
                // the system's keychain or keyring, with a prompt; and the
                // platform's display server.
                platform::switches(cl, browser);
                // The browser process (no process type) draws pages in
                // sRGB unless `web.color` is `display`; its helpers are
                // told the same as they launch (below), so a helper never
                // decides this itself.
                if browser {
                    force_srgb(cl);
                }
            }
            fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
                Some(DreamerBrowserProcess::new())
            }
            // In every process: the scheme apps' packaged pages are served
            // on is a standard, secure one that `fetch` may use, so a page
            // there has an origin of its own.
            fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
                if let Some(r) = registrar {
                    let options = SchemeOptions::STANDARD.get_raw()
                        | SchemeOptions::SECURE.get_raw()
                        | SchemeOptions::FETCH_ENABLED.get_raw();
                    r.add_custom_scheme(
                        Some(&CefString::from(crate::site::SCHEME)),
                        options as i32,
                    );
                }
            }
        }
    }

    wrap_browser_process_handler! {
        pub struct DreamerBrowserProcess;
        impl BrowserProcessHandler {
            fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
                if let Some(cl) = command_line {
                    force_srgb(cl);
                }
            }
            fn on_schedule_message_pump_work(&self, delay_ms: i64) {
                if let Some(wake) = WAKE.get() {
                    wake(delay_ms);
                }
            }
        }
    }

    wrap_client! {
        pub struct DreamerClient {
            shared: Arc<Shared>,
        }
        impl Client {
            fn display_handler(&self) -> Option<DisplayHandler> {
                Some(DreamerDisplay::new(self.shared.clone()))
            }
            fn load_handler(&self) -> Option<LoadHandler> {
                Some(DreamerLoad::new(self.shared.clone()))
            }
            fn focus_handler(&self) -> Option<FocusHandler> {
                Some(DreamerFocus::new(self.shared.clone()))
            }
            fn life_span_handler(&self) -> Option<LifeSpanHandler> {
                Some(DreamerLifeSpan::new(self.shared.clone()))
            }
            fn request_handler(&self) -> Option<RequestHandler> {
                self.shared
                    .app
                    .is_some()
                    .then(|| DreamerRequests::new(self.shared.clone()))
            }
            fn download_handler(&self) -> Option<DownloadHandler> {
                Some(DreamerDownloads::new(self.shared.clone()))
            }
            fn find_handler(&self) -> Option<FindHandler> {
                Some(DreamerFind::new(self.shared.clone()))
            }
            fn keyboard_handler(&self) -> Option<KeyboardHandler> {
                self.shared
                    .app
                    .is_none()
                    .then(|| DreamerKeys::new(self.shared.clone()))
            }
            // Pages drawn offscreen (Wayland) paint through the surface.
            fn render_handler(&self) -> Option<RenderHandler> {
                self.shared.render.clone()
            }
        }
    }

    // Downloads (see `crate::browser`): a page that may not save them is
    // refused, with a line in the pane's status; any other download waits
    // for the pane's answer (`WebView::save_download`), its callback kept
    // until then.
    wrap_download_handler! {
        pub struct DreamerDownloads {
            shared: Arc<Shared>,
        }
        impl DownloadHandler {
            fn can_download(
                &self,
                _browser: Option<&mut Browser>,
                url: Option<&CefString>,
                _request_method: Option<&CefString>,
            ) -> i32 {
                if self.shared.may_download {
                    return 1;
                }
                let url = url.map(|u| u.to_string()).unwrap_or_default();
                self.shared
                    .event(crate::Event::Status(crate::browser::download_refused(&url)));
                0
            }
            fn on_before_download(
                &self,
                _browser: Option<&mut Browser>,
                download_item: Option<&mut DownloadItem>,
                suggested_name: Option<&CefString>,
                callback: Option<&mut BeforeDownloadCallback>,
            ) -> i32 {
                let name = suggested_name
                    .map(|n| n.to_string())
                    .filter(|n| !n.is_empty())
                    .or_else(|| {
                        download_item
                            .as_ref()
                            .map(|d| CefString::from(&d.url()).to_string())
                    })
                    .unwrap_or_default();
                // Returning 0 cancels it (CEF's default for this kind of
                // browser).
                let (true, Some(item), Some(callback)) =
                    (self.shared.may_download, download_item, callback)
                else {
                    self.shared
                        .event(crate::Event::Status(crate::browser::download_refused(&name)));
                    return 0;
                };
                let id = item.id();
                let size = u64::try_from(item.total_bytes()).ok().filter(|n| *n > 0);
                if let Ok(mut d) = self.shared.downloads.lock() {
                    d.entry(id).or_default().asked = Some(callback.clone());
                }
                self.shared
                    .event(crate::Event::DownloadAsked { id, name, size });
                1
            }
            fn on_download_updated(
                &self,
                _browser: Option<&mut Browser>,
                download_item: Option<&mut DownloadItem>,
                callback: Option<&mut DownloadItemCallback>,
            ) {
                let Some(item) = download_item else { return };
                let id = item.id();
                let path = || {
                    let p = CefString::from(&item.full_path()).to_string();
                    (!p.is_empty()).then(|| PathBuf::from(p))
                };
                let event = {
                    let Ok(mut downloads) = self.shared.downloads.lock() else {
                        return;
                    };
                    let d = downloads.entry(id).or_default();
                    if let Some(cb) = callback {
                        d.item = Some(cb.clone());
                    }
                    let saving = d.saving_to.is_some();
                    if item.is_complete() == 1 {
                        downloads.remove(&id);
                        saving.then(|| crate::Event::DownloadDone {
                            id,
                            path: path().unwrap_or_default(),
                        })
                    } else if item.is_canceled() == 1 || item.is_interrupted() == 1 {
                        downloads.remove(&id);
                        let error = (item.is_interrupted() == 1)
                            .then(|| interrupted(item.interrupt_reason().get_raw()));
                        Some(crate::Event::DownloadEnded {
                            id,
                            error,
                            partial: path(),
                        })
                    } else if saving {
                        Some(crate::Event::DownloadProgress {
                            id,
                            received: u64::try_from(item.received_bytes()).unwrap_or(0),
                            total: u64::try_from(item.total_bytes()).ok().filter(|n| *n > 0),
                        })
                    } else {
                        None
                    }
                };
                if let Some(e) = event {
                    self.shared.event(e);
                }
            }
        }
    }

    wrap_find_handler! {
        pub struct DreamerFind {
            shared: Arc<Shared>,
        }
        impl FindHandler {
            fn on_find_result(
                &self,
                _browser: Option<&mut Browser>,
                _identifier: i32,
                count: i32,
                _selection_rect: Option<&cef::Rect>,
                active_match_ordinal: i32,
                final_update: i32,
            ) {
                self.shared.event(crate::Event::Found {
                    count: count.max(0) as u32,
                    current: active_match_ordinal.max(0) as u32,
                    last: final_update == 1,
                });
            }
        }
    }

    // The browser's Esc stops a load; any other key is the page's.
    wrap_keyboard_handler! {
        pub struct DreamerKeys {
            shared: Arc<Shared>,
        }
        impl KeyboardHandler {
            fn on_pre_key_event(
                &self,
                browser: Option<&mut Browser>,
                event: Option<&KeyEvent>,
                _os_event: platform::OsEvent<'_>,
                _is_keyboard_shortcut: Option<&mut i32>,
            ) -> i32 {
                let (Some(b), Some(e)) = (browser, event) else {
                    return 0;
                };
                // Shift, Control, Alt, Command.
                let modified = e.modifiers & 0b1000_1110 != 0;
                if e.type_ == KeyEventType::RAWKEYDOWN
                    && e.windows_key_code == 0x1b
                    && !modified
                    && b.is_loading() == 1
                {
                    b.stop_load();
                    return 1;
                }
                0
            }
        }
    }

    // An app's page: where it may navigate, and what its own origin
    // serves (its package's files, its bridge).
    wrap_request_handler! {
        pub struct DreamerRequests {
            shared: Arc<Shared>,
        }
        impl RequestHandler {
            fn on_before_browse(
                &self,
                _browser: Option<&mut Browser>,
                _frame: Option<&mut Frame>,
                request: Option<&mut Request>,
                _user_gesture: i32,
                _is_redirect: i32,
            ) -> i32 {
                let (Some(app), Some(request)) = (self.shared.app.as_ref(), request) else {
                    return 0;
                };
                let url = CefString::from(&request.url()).to_string();
                if app.may_navigate(&url) {
                    return 0;
                }
                if let Ok(mut b) = self.shared.blocked.lock() {
                    b.push(url);
                }
                1
            }
            fn resource_request_handler(
                &self,
                _browser: Option<&mut Browser>,
                frame: Option<&mut Frame>,
                request: Option<&mut Request>,
                _is_navigation: i32,
                _is_download: i32,
                request_initiator: Option<&CefString>,
                _disable_default_handling: Option<&mut i32>,
            ) -> Option<ResourceRequestHandler> {
                let app = self.shared.app.as_ref()?;
                let url = CefString::from(&request?.url()).to_string();
                let serve = if app.is_bridge_url(&url) {
                    let frame_url = frame
                        .map(|f| CefString::from(&f.url()).to_string())
                        .unwrap_or_default();
                    let initiator = request_initiator.map(|i| i.to_string()).unwrap_or_default();
                    if app.is_bridge(&url, &frame_url, &initiator) {
                        Serve::Bridge
                    } else {
                        Serve::Refused
                    }
                } else {
                    match app.file(&url) {
                        Some(path) => Serve::File(path),
                        None if crate::site::origin(&url)
                            .is_some_and(|o| o.starts_with(crate::site::SCHEME)) =>
                        {
                            Serve::Missing
                        }
                        None => return None,
                    }
                };
                Some(DreamerResources::new(self.shared.clone(), Arc::new(serve)))
            }
        }
    }

    wrap_resource_request_handler! {
        pub struct DreamerResources {
            shared: Arc<Shared>,
            serve: Arc<Serve>,
        }
        impl ResourceRequestHandler {
            // The binding's default cancels every request: go on.
            fn on_before_resource_load(
                &self,
                _browser: Option<&mut Browser>,
                _frame: Option<&mut Frame>,
                _request: Option<&mut Request>,
                _callback: Option<&mut Callback>,
            ) -> ReturnValue {
                ReturnValue::CONTINUE
            }
            fn resource_handler(
                &self,
                _browser: Option<&mut Browser>,
                _frame: Option<&mut Frame>,
                _request: Option<&mut Request>,
            ) -> Option<ResourceHandler> {
                Some(Served::new(
                    self.shared.clone(),
                    self.serve.clone(),
                    Arc::new(Mutex::new(Body::default())),
                ))
            }
        }
    }

    wrap_resource_handler! {
        pub struct Served {
            shared: Arc<Shared>,
            serve: Arc<Serve>,
            body: Arc<Mutex<Body>>,
        }
        impl ResourceHandler {
            fn open(
                &self,
                request: Option<&mut Request>,
                handle_request: Option<&mut i32>,
                callback: Option<&mut Callback>,
            ) -> i32 {
                let Some(handle) = handle_request else { return 0 };
                *handle = 1;
                let answer = |status, mime, bytes: Vec<u8>| {
                    if let Ok(mut b) = self.body.lock() {
                        *b = Body { status, mime, bytes, at: 0 };
                    }
                    1
                };
                let refuse = |status, code, why: &str| {
                    let reply = crate::site::bridge_reply(&Err((code, why.to_owned())));
                    answer(status, "application/json", reply.into_bytes())
                };
                match &*self.serve {
                    Serve::File(path) => match std::fs::read(path) {
                        Ok(bytes) => answer(200, crate::site::mime(path), bytes),
                        Err(_) => answer(404, "text/plain", b"not found".to_vec()),
                    },
                    Serve::Missing => answer(404, "text/plain", b"not found".to_vec()),
                    // A frame of another origin: no bridge.
                    Serve::Refused => refuse(403, -32005, "no bridge for this origin"),
                    Serve::Bridge => {
                        let body = request.map(|r| post_body(r)).unwrap_or_default();
                        let (method, params) = match crate::site::bridge_request(&body) {
                            Ok(call) => call,
                            Err(why) => return refuse(400, -32600, &why),
                        };
                        let (Some(app), Some(cb)) = (self.shared.app.as_ref(), callback.cloned())
                        else {
                            return 0;
                        };
                        // Answered later, when the call comes back.
                        *handle = 0;
                        let out = self.body.clone();
                        (app.bridge)(crate::site::BridgeCall {
                            method,
                            params,
                            reply: Box::new(move |a| {
                                if let Ok(mut b) = out.lock() {
                                    *b = Body {
                                        status: 200,
                                        mime: "application/json",
                                        bytes: crate::site::bridge_reply(&a).into_bytes(),
                                        at: 0,
                                    };
                                }
                                cb.cont();
                            }),
                        });
                        1
                    }
                }
            }
            fn response_headers(
                &self,
                response: Option<&mut Response>,
                response_length: Option<&mut i64>,
                _redirect_url: Option<&mut CefString>,
            ) {
                let Ok(b) = self.body.lock() else { return };
                if let Some(r) = response {
                    r.set_status(b.status);
                    r.set_mime_type(Some(&CefString::from(b.mime)));
                    r.set_header_by_name(
                        Some(&CefString::from("Cache-Control")),
                        Some(&CefString::from("no-store")),
                        1,
                    );
                }
                if let Some(l) = response_length {
                    *l = b.bytes.len() as i64;
                }
            }
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            fn read(
                &self,
                data_out: *mut u8,
                bytes_to_read: i32,
                bytes_read: Option<&mut i32>,
                _callback: Option<&mut ResourceReadCallback>,
            ) -> i32 {
                let (Some(read), Ok(mut b)) = (bytes_read, self.body.lock()) else {
                    return 0;
                };
                let n = b.bytes.len().saturating_sub(b.at).min(bytes_to_read.max(0) as usize);
                *read = n as i32;
                if n == 0 || data_out.is_null() {
                    return 0;
                }
                // SAFETY: CEF hands a buffer of `bytes_to_read` bytes, and
                // `n` is at most that.
                unsafe { std::ptr::copy_nonoverlapping(b.bytes[b.at..].as_ptr(), data_out, n) };
                b.at += n;
                1
            }
        }
    }

    wrap_display_handler! {
        pub struct DreamerDisplay {
            shared: Arc<Shared>,
        }
        impl DisplayHandler {
            fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
                if let Ok(mut t) = self.shared.title.lock() {
                    *t = title.map(|t| t.to_string()).unwrap_or_default();
                }
            }
            fn on_address_change(
                &self,
                _browser: Option<&mut Browser>,
                frame: Option<&mut Frame>,
                url: Option<&CefString>,
            ) {
                if frame.is_some_and(|f| f.is_main() == 1)
                    && let Ok(mut u) = self.shared.url.lock()
                {
                    *u = url.map(|u| u.to_string()).unwrap_or_default();
                }
            }
        }
    }

    wrap_load_handler! {
        pub struct DreamerLoad {
            shared: Arc<Shared>,
        }
        impl LoadHandler {
            fn on_load_end(
                &self,
                _browser: Option<&mut Browser>,
                frame: Option<&mut Frame>,
                http_status_code: i32,
            ) {
                if let Some(f) = frame.filter(|f| f.is_main() == 1) {
                    self.shared.loaded.store(http_status_code, Ordering::Relaxed);
                    let script = self.shared.on_load.lock().ok().and_then(|mut s| s.take());
                    if let Some(script) = script {
                        f.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                    }
                }
            }
            fn on_load_error(
                &self,
                _browser: Option<&mut Browser>,
                frame: Option<&mut Frame>,
                error_code: Errorcode,
                error_text: Option<&CefString>,
                failed_url: Option<&CefString>,
            ) {
                // A load stopped or turned into a download is not a failure.
                if frame.is_some_and(|f| f.is_main() == 1) && error_code != Errorcode::ABORTED {
                    self.shared.event(crate::Event::LoadFailed {
                        url: failed_url.map(|u| u.to_string()).unwrap_or_default(),
                        error: error_text.map(|t| t.to_string()).unwrap_or_default(),
                    });
                }
            }
        }
    }

    wrap_focus_handler! {
        pub struct DreamerFocus {
            shared: Arc<Shared>,
        }
        impl FocusHandler {
            fn on_got_focus(&self, _browser: Option<&mut Browser>) {
                self.shared.focused.store(true, Ordering::Relaxed);
                self.shared.event(crate::Event::Focus);
            }
        }
    }

    // A profile Chromium made or loaded: pages may open in it now.
    wrap_request_context_handler! {
        pub struct DreamerProfile {
            ready: Arc<ProfileReady>,
        }
        impl RequestContextHandler {
            fn on_request_context_initialized(&self, _context: Option<&mut RequestContext>) {
                let waiting = match self.ready.waiting.lock() {
                    Ok(mut w) => {
                        self.ready.done.store(true, Ordering::Release);
                        std::mem::take(&mut *w)
                    }
                    Err(_) => return,
                };
                for f in waiting {
                    f();
                }
            }
        }
    }

    // A profile's cookies are on disk.
    wrap_completion_callback! {
        pub struct Flushed {
            done: Arc<AtomicBool>,
        }
        impl CompletionCallback {
            fn on_complete(&self) {
                self.done.store(true, Ordering::Release);
            }
        }
    }

    // Load `url` in `browser`'s main frame, as a task of its own.
    wrap_task! {
        pub struct LoadLater {
            browser: Browser,
            url: String,
        }
        impl Task {
            fn execute(&self) {
                if let Some(f) = self.browser.main_frame() {
                    f.load_url(Some(&CefString::from(self.url.as_str())));
                }
            }
        }
    }

    wrap_dev_tools_message_observer! {
        pub struct DreamerDevTools {
            shared: Arc<Shared>,
        }
        impl DevToolsMessageObserver {
            fn on_dev_tools_method_result(
                &self,
                _browser: Option<&mut Browser>,
                message_id: i32,
                success: i32,
                result: Option<&[u8]>,
            ) {
                let done = self
                    .shared
                    .replies
                    .lock()
                    .ok()
                    .and_then(|mut r| r.remove(&message_id));
                if let Some(done) = done {
                    let reply = result.unwrap_or_default().to_vec();
                    done(if success == 1 { Ok(reply) } else { Err(reply) });
                }
            }
        }
    }

    wrap_life_span_handler! {
        pub struct DreamerLifeSpan {
            shared: Arc<Shared>,
        }
        impl LifeSpanHandler {
            // One page per pane: no new windows. In the browser, a link
            // that wants one opens in the same pane.
            fn on_before_popup(
                &self,
                browser: Option<&mut Browser>,
                _frame: Option<&mut Frame>,
                _popup_id: i32,
                target_url: Option<&CefString>,
                _target_frame_name: Option<&CefString>,
                _target_disposition: WindowOpenDisposition,
                _user_gesture: i32,
                _popup_features: Option<&PopupFeatures>,
                _window_info: Option<&mut WindowInfo>,
                _client: Option<&mut Option<Client>>,
                _settings: Option<&mut BrowserSettings>,
                _extra_info: Option<&mut Option<DictionaryValue>>,
                _no_javascript_access: Option<&mut i32>,
            ) -> i32 {
                if self.shared.app.is_none()
                    && let (Some(b), Some(url)) = (browser, target_url)
                    && let Some(f) = b.main_frame()
                {
                    f.load_url(Some(url));
                }
                1
            }
            // The page's surface goes, not the window: by default CEF asks
            // the window holding a closing browser to close.
            fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
                browser.map_or(0, platform::close)
            }
            fn on_before_close(&self, _browser: Option<&mut Browser>) {
                if !self.shared.closed.swap(true, Ordering::Relaxed) {
                    LIVE.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Where a page's events go.
type OnEvent = Box<dyn Fn(crate::Event) + Send + Sync>;

/// A DevTools reply's handler: the result's JSON, or the error's.
type Reply = Box<dyn FnOnce(Result<Vec<u8>, Vec<u8>>) + Send>;

/// What a browser's handlers report, read from the main thread.
#[derive(Default)]
struct Shared {
    title: Mutex<String>,
    /// The main frame's address now.
    url: Mutex<String>,
    /// HTTP status of the main frame's last finished load; -1 before one.
    loaded: AtomicI32,
    closed: AtomicBool,
    on_event: Mutex<Option<OnEvent>>,
    /// A script for the main frame when its load finishes, once.
    on_load: Mutex<Option<String>>,
    /// DevTools calls waiting for their replies, by message id.
    replies: Mutex<HashMap<i32, Reply>>,
    /// The app whose page this is, if it is one.
    app: Option<crate::AppPage>,
    /// Navigations refused (an app's page, outside its origins), in order.
    blocked: Mutex<Vec<String>>,
    /// Whether the page may ask to save downloads.
    may_download: bool,
    /// Whether the page has the keyboard focus, as last given or taken
    /// (where the platform cannot be asked).
    focused: AtomicBool,
    /// How a page drawn offscreen paints (Wayland).
    render: Option<RenderHandler>,
    /// Downloads the page started and Chromium still has, by id.
    downloads: Mutex<HashMap<u32, Download>>,
}

/// A download Chromium has: its callbacks, and whether it is being saved.
#[derive(Default)]
struct Download {
    /// Until the pane answers: save (continue) it, or drop it (cancel).
    asked: Option<BeforeDownloadCallback>,
    /// Cancels it once it runs.
    item: Option<DownloadItemCallback>,
    /// Where it is being saved, once the pane said where.
    saving_to: Option<PathBuf>,
}

impl Shared {
    /// Tell the pane, if it still listens.
    fn event(&self, e: crate::Event) {
        if let Ok(f) = self.on_event.lock()
            && let Some(f) = f.as_ref()
        {
            f(e);
        }
    }
}

/// What an app's origin answers a request with.
enum Serve {
    /// A file of the package's `web/` directory.
    File(PathBuf),
    /// No such file on the package's origin.
    Missing,
    /// The bridge, for a frame of the app's own origin.
    Bridge,
    /// The bridge's address, from another origin.
    Refused,
}

/// A response as decided, read out as Chromium asks.
#[derive(Default)]
struct Body {
    status: i32,
    mime: &'static str,
    bytes: Vec<u8>,
    at: usize,
}

/// Why a download stopped, from Chromium's interrupt reason (its ranges:
/// file 1 to 19, network 20 to 29, server 30 to 39, the user 40 on).
fn interrupted(reason: u32) -> String {
    match reason {
        3 => "the disk is full".into(),
        2 => "the folder cannot be written".into(),
        1..=19 => format!("the file could not be written (reason {reason})"),
        20..=29 => "the network failed".into(),
        30..=39 => "the server failed".into(),
        _ => format!("interrupted (reason {reason})"),
    }
}

/// The bytes a request posts.
fn post_body(request: &Request) -> Vec<u8> {
    let Some(post) = request.post_data() else {
        return Vec::new();
    };
    let mut elements: Vec<Option<PostDataElement>> = vec![None; post.element_count()];
    post.elements(Some(&mut elements));
    let mut out = Vec::new();
    for e in elements.into_iter().flatten() {
        let mut bytes = vec![0u8; e.bytes_count()];
        let n = e.bytes(bytes.len(), bytes.as_mut_ptr());
        bytes.truncate(n);
        out.extend(bytes);
    }
    out
}

/// Chromium's profile directory, once started: each app's own profile is
/// a directory of it.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// DevTools message ids, unique in the process.
static MESSAGE: AtomicI32 = AtomicI32::new(1);

/// Start CEF in this process, once, and initialize it with an external
/// message pump. On macOS the framework is loaded from Dreamer Web (the
/// component installed apart from the app, `web/` in Dreamer's data
/// directory; see [`web_component`]), the application object is made to
/// answer CEF's protocol, and Chromium runs its helper processes from the
/// component's helper apps. On Linux `libcef.so` is loaded from the same
/// component, which holds Chromium's resources and the helper too, and
/// `window` (any of the app's windows) says which display server pages
/// are drawn for: X11 or Wayland.
/// `cache_dir` holds Chromium's profile. `color` is the color space pages
/// are drawn in, for as long as the process runs: Chromium takes it as it
/// initializes, so a later call's is ignored. `wake` is called from any thread
/// with a delay in milliseconds whenever CEF wants [`do_work`]; the caller
/// should also call it at least every 33 ms or so while browsers exist.
///
/// Main thread only, inside the toolkit's running application. A second
/// call returns the first one's outcome, except while the component is
/// not installed (or not this build's): it is looked for again each time,
/// so adding it needs no restart.
pub fn start(
    cache_dir: &Path,
    color: crate::Color,
    window: &impl HasWindowHandle,
    wake: impl Fn(i64) + Send + Sync + 'static,
) -> Result<(), String> {
    if let Some(done) = STARTED.with(|s| s.borrow().clone()) {
        return done;
    }
    // The platform's Chromium, loaded; or why not (a missing component is
    // looked for again on the next call).
    let chromium = platform::Chromium::find(window)?;
    let result = (|| {
        chromium.load()?;
        let _ = WAKE.set(Box::new(wake));
        DISPLAY_COLOR.store(color == crate::Color::Display, Ordering::Relaxed);
        std::fs::create_dir_all(cache_dir).map_err(|e| format!("{}: {e}", cache_dir.display()))?;
        // Chromium compares profile paths with links resolved (`/tmp` is
        // `/private/tmp`): an app's profile must be inside this one.
        let cache_dir = &cache_dir
            .canonicalize()
            .unwrap_or_else(|_| cache_dir.to_path_buf());
        let _ = ROOT.set(cache_dir.clone());
        // Where the framework (or library), its resources, and the helpers
        // are, as the platform has them.
        let settings = chromium.settings(Settings {
            // Chromium's sandbox is not set up for these helpers yet.
            no_sandbox: 1,
            external_message_pump: 1,
            multi_threaded_message_loop: 0,
            root_cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
            cache_path: CefString::from(cache_dir.join("default").to_string_lossy().as_ref()),
            log_file: CefString::from(cache_dir.join("chromium.log").to_string_lossy().as_ref()),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        })?;
        // Kept for the process: CEF may read its arguments later.
        let argv = Box::leak(Box::new(MainArgv::executable()));
        let main_args = argv.main_args();
        let mut app = handlers::DreamerApp::new();
        if initialize(
            Some(&main_args),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 1
        {
            return Err(format!(
                "Chromium did not start (see {})",
                cache_dir.join("chromium.log").display()
            ));
        }
        Ok(())
    })();
    STARTED.with(|s| *s.borrow_mut() = Some(result.clone()));
    result
}

/// Whether [`start`] succeeded in this process.
pub fn started() -> bool {
    STARTED.with(|s| matches!(*s.borrow(), Some(Ok(()))))
}

/// Run CEF's pending work. Call it on the main thread when the `wake`
/// given to [`start`] asks, and periodically while browsers exist.
pub fn do_work() {
    if started() {
        do_message_loop_work();
        #[cfg(target_os = "linux")]
        platform::native_events();
    }
}

/// Close CEF after the last browser has closed, pumping its work until
/// then, at most for `timeout`. Every [`WebView`] must have been dropped.
pub fn shutdown(timeout: Duration) -> Result<(), String> {
    if !started() {
        return Ok(());
    }
    let until = Instant::now() + timeout;
    // Cookies first (logins survive a restart), whether or not every
    // browser closes in time: Chromium writes them to disk only now and then.
    let flushing: Vec<Arc<AtomicBool>> = PROFILES.with(|p| {
        p.borrow()
            .values()
            .filter_map(|p| {
                let done = Arc::new(AtomicBool::new(false));
                let mut flushed = handlers::Flushed::new(done.clone());
                let cookies = p.context.cookie_manager(None)?;
                (cookies.flush_store(Some(&mut flushed)) != 0).then_some(done)
            })
            .collect()
    });
    while flushing.iter().any(|d| !d.load(Ordering::Acquire)) && Instant::now() < until {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    while LIVE.load(Ordering::Relaxed) > 0 {
        if Instant::now() > until {
            return Err(format!(
                "{} browser(s) still open; Chromium not shut down",
                LIVE.load(Ordering::Relaxed)
            ));
        }
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    // Chromium shuts down with no profile still held.
    PROFILES.with(|p| p.borrow_mut().clear());
    cef::shutdown();
    STARTED.with(|s| *s.borrow_mut() = Some(Err("Chromium was shut down".into())));
    Ok(())
}

/// The entry point of the helper executable (CEF's renderer, GPU, and
/// utility processes): returns its exit code.
pub fn run_subprocess() -> i32 {
    if let Err(e) = platform::load_in_helper() {
        eprintln!("dreamer-web-helper: {e}");
        return 1;
    }
    let args = args::Args::new();
    // The renderer must know apps' scheme too.
    let mut app = handlers::DreamerApp::new();
    execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    )
}

/// The directory, in Chromium's profile directory, of the profile a page
/// opens in: app `app`'s own (`app-<app>`), or the browser's. Chromium
/// makes a profile only directly inside its root (any deeper directory
/// is refused), so the profiles sit side by side there.
fn profile_name(app: Option<&str>) -> Result<String, String> {
    let Some(app) = app else {
        return Ok(crate::browser::PROFILE.to_owned());
    };
    let plain = !app.is_empty()
        && app
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !app.starts_with('.');
    if !plain {
        return Err(format!("`{app}` is not an app id"));
    }
    Ok(format!("app-{app}"))
}

/// A profile's request context, asked of Chromium once per process and
/// kept: Chromium makes (or loads) a profile on disk asynchronously, and
/// a browser can be created in it only once it is initialized.
struct Profile {
    context: RequestContext,
    ready: Arc<ProfileReady>,
}

#[derive(Default)]
struct ProfileReady {
    done: AtomicBool,
    /// What runs when it is.
    waiting: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

thread_local! {
    /// The profiles asked for so far, by directory name.
    static PROFILES: RefCell<HashMap<String, Profile>> = RefCell::new(HashMap::new());
}

/// The request context of profile `name` (see [`profile_name`]), asked of
/// Chromium if it was not yet, and whether it is ready for a browser.
fn profile(name: &str) -> Result<(RequestContext, Arc<ProfileReady>), String> {
    if let Some(p) = PROFILES.with(|p| {
        p.borrow()
            .get(name)
            .map(|p| (p.context.clone(), p.ready.clone()))
    }) {
        return Ok(p);
    }
    let root = ROOT.get().ok_or("Chromium is not running")?;
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let settings = RequestContextSettings {
        cache_path: CefString::from(dir.to_string_lossy().as_ref()),
        // Session cookies kept, so logins survive restarts.
        persist_session_cookies: 1,
        ..Default::default()
    };
    let ready = Arc::new(ProfileReady::default());
    let mut handler = handlers::DreamerProfile::new(ready.clone());
    let context = request_context_create_context(Some(&settings), Some(&mut handler))
        .ok_or_else(|| format!("Chromium made no profile at {}", dir.display()))?;
    PROFILES.with(|p| {
        p.borrow_mut().insert(
            name.to_owned(),
            Profile {
                context: context.clone(),
                ready: ready.clone(),
            },
        )
    });
    Ok((context, ready))
}

/// Whether the profile a page of app `app` (or, without one, a browser
/// page) opens in is ready for it. If not, Chromium is making it now, and
/// `ready` runs (on the main thread, from [`do_work`]) once it is: open the
/// page then. [`WebView::open`] refuses a page whose profile is not ready.
pub fn profile_ready(
    app: Option<&str>,
    ready: impl FnOnce() + Send + 'static,
) -> Result<bool, String> {
    if !started() {
        return Err("Chromium is not running".into());
    }
    let (_, r) = profile(&profile_name(app)?)?;
    let mut waiting = r.waiting.lock().map_err(|_| "a profile's lock")?;
    // Checked under the lock the initializer takes, so `ready` is never
    // left behind.
    if r.done.load(Ordering::Acquire) {
        return Ok(true);
    }
    waiting.push(Box::new(ready));
    Ok(false)
}

/// One page, drawn by Chromium in a surface of its own over a rectangle of
/// a window. Closed when dropped.
pub struct WebView {
    browser: Browser,
    shared: Arc<Shared>,
    surface: platform::Surface,
    placed: Cell<Option<Rect>>,
    visible: Cell<bool>,
    /// Keeps the DevTools observer registered.
    _devtools: Option<Registration>,
}

impl std::fmt::Debug for WebView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebView")
            .field("placed", &self.placed.get())
            .field("visible", &self.visible.get())
            .finish_non_exhaustive()
    }
}

impl WebView {
    /// Open `url` in a new view over `rect` of `window`, which keeps
    /// keyboard focus. `on_event` gets what the page reports (on CEF's UI
    /// thread, the main thread): that it took focus, for example on a
    /// click, a status line, find results. With `app`, the page is the
    /// app's: in its own profile, held to its origins, and with the bridge
    /// on its own origin. Without, it is the browser ([`crate::browser`]).
    pub fn open(
        window: &(impl HasWindowHandle + HasDisplayHandle),
        rect: Rect,
        url: &str,
        app: Option<crate::AppPage>,
        on_event: impl Fn(crate::Event) + Send + Sync + 'static,
    ) -> Result<WebView, String> {
        if !started() {
            return Err("Chromium is not running".into());
        }
        let surface = platform::Surface::new(window, rect)?;
        let (context, ready) = profile(&profile_name(app.as_ref().map(|a| a.app.as_str()))?)?;
        if !ready.done.load(Ordering::Acquire) {
            return Err("the page's browser profile is not ready yet".into());
        }
        let mut context = Some(context);
        let script = app.as_ref().map(crate::AppPage::bridge_script);
        let shared = Arc::new(Shared {
            loaded: AtomicI32::new(-1),
            on_event: Mutex::new(Some(Box::new(on_event))),
            may_download: crate::browser::downloads_allowed(app.as_ref()),
            app,
            render: surface.render_handler(),
            ..Shared::default()
        });
        let info = surface.window_info(rect);
        let mut client = handlers::DreamerClient::new(shared.clone());
        // An app's page loads once its bridge script is in place.
        let first = if script.is_some() { "about:blank" } else { url };
        let browser = browser_host_create_browser_sync(
            Some(&info),
            Some(&mut client),
            Some(&CefString::from(first)),
            // A page drawn offscreen (Wayland) paints at up to 60 frames
            // a second, not CEF's default 30; others ignore it.
            Some(&BrowserSettings {
                windowless_frame_rate: 60,
                ..Default::default()
            }),
            None,
            context.as_mut(),
        )
        .ok_or("Chromium could not create a browser")?;
        LIVE.fetch_add(1, Ordering::Relaxed);
        let devtools = browser.host().and_then(|h| {
            h.add_dev_tools_message_observer(Some(&mut handlers::DreamerDevTools::new(
                shared.clone(),
            )))
        });
        surface.opened(&browser);
        let view = WebView {
            browser,
            shared,
            surface,
            placed: Cell::new(None),
            visible: Cell::new(true),
            _devtools: devtools,
        };
        view.place(rect);
        // Scripts added for new documents run only while the page domain
        // is enabled.
        view.devtools("Page.enable", "{}", Box::new(|_| {}));
        if script.is_none() {
            // The browser's back and forward by mouse and swipe, in an
            // isolated world, in this document and every later one.
            let params = serde_json::json!({
                "source": crate::browser::NAVIGATION_SCRIPT,
                "worldName": crate::browser::WORLD,
                "runImmediately": true,
            });
            view.devtools(
                "Page.addScriptToEvaluateOnNewDocument",
                &params.to_string(),
                Box::new(|_| {}),
            );
        }
        if let Some(script) = script {
            let params = serde_json::json!({ "source": script }).to_string();
            let (browser, url) = (view.browser.clone(), url.to_owned());
            view.devtools(
                "Page.addScriptToEvaluateOnNewDocument",
                &params,
                // Loaded after Chromium's call into the DevTools observer
                // returns: a navigation started inside it changes what
                // Chromium is iterating over.
                Box::new(move |_| {
                    post_task(
                        ThreadId::UI,
                        Some(&mut handlers::LoadLater::new(browser, url)),
                    );
                }),
            );
        }
        // Creating the browser may give its surface the keys; the pane
        // takes focus only when Dreamer gives it.
        view.surface.give_focus_back(&view.browser);
        view.shared.focused.store(false, Ordering::Relaxed);
        Ok(view)
    }

    fn host(&self) -> Option<BrowserHost> {
        self.browser.host()
    }

    /// Save download `id`, which the page asked for
    /// ([`crate::Event::DownloadAsked`]), to `path`: a file that need not
    /// exist (an existing one is replaced when the download completes).
    /// Its progress and end come as events. An error when it is no longer
    /// waiting (canceled meanwhile).
    pub fn save_download(&self, id: u32, path: &Path) -> Result<(), String> {
        let asked = self.shared.downloads.lock().ok().and_then(|mut d| {
            let d = d.get_mut(&id)?;
            let asked = d.asked.take()?;
            d.saving_to = Some(path.to_path_buf());
            Some(asked)
        });
        let asked = asked.ok_or("the download is no longer waiting")?;
        // Outside the lock: Chromium may report on the download at once.
        asked.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
        Ok(())
    }

    /// Cancel download `id`: one waiting for an answer, or one being saved
    /// (Chromium removes its partial file, and reports the end).
    pub fn cancel_download(&self, id: u32) {
        let (asked, item) = match self.shared.downloads.lock() {
            Ok(mut d) => match d.get_mut(&id) {
                Some(d) => (d.asked.take(), d.item.clone()),
                None => (None, None),
            },
            Err(_) => (None, None),
        };
        // Outside the lock: a callback dropped unanswered cancels the
        // download, and Chromium may report that at once.
        drop(asked);
        if let Some(item) = item {
            item.cancel();
        }
    }

    /// The navigations refused so far (an app's page, to an origin it may
    /// not go to), oldest first.
    pub fn blocked(&self) -> Vec<String> {
        self.shared
            .blocked
            .lock()
            .map(|b| b.clone())
            .unwrap_or_default()
    }

    /// Keep the view on `rect` (window content coordinates from the top
    /// left). Cheap when nothing changed.
    pub fn place(&self, rect: Rect) {
        if self.placed.get() == Some(rect) {
            return;
        }
        self.placed.set(Some(rect));
        self.surface.place(&self.browser, rect);
        if let Some(h) = self.host() {
            h.was_resized();
        }
    }

    /// The rectangle the view was last placed on.
    pub fn placed(&self) -> Option<Rect> {
        self.placed.get()
    }

    /// Show or hide the view; a hidden page stops drawing.
    pub fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) == visible {
            return;
        }
        self.surface.set_visible(&self.browser, visible);
        if let Some(h) = self.host() {
            h.was_hidden(i32::from(!visible));
        }
    }

    pub fn visible(&self) -> bool {
        self.visible.get()
    }

    /// Give the page keyboard focus. A page that has it (a click in it)
    /// keeps it where it is.
    pub fn focus(&self) {
        if self.has_focus() {
            return;
        }
        if let Some(h) = self.host() {
            h.set_focus(1);
        }
        self.shared.focused.store(true, Ordering::Relaxed);
        if !self.has_focus() {
            self.surface.take_focus(&self.browser);
        }
    }

    /// Whether the page's surface has the window's keyboard focus now.
    pub fn has_focus(&self) -> bool {
        self.surface
            .has_focus(&self.browser)
            .unwrap_or_else(|| self.shared.focused.load(Ordering::Relaxed))
    }

    /// Take keyboard focus back to the window's own view, if the page has
    /// it (another page may have taken it already).
    pub fn release_focus(&self) {
        if !self.has_focus() {
            return;
        }
        if let Some(h) = self.host() {
            h.set_focus(0);
        }
        self.shared.focused.store(false, Ordering::Relaxed);
        self.surface.give_focus_back(&self.browser);
    }

    /// The main frame's address now.
    pub fn url(&self) -> String {
        self.shared
            .url
            .lock()
            .map(|u| u.clone())
            .unwrap_or_default()
    }

    /// Back in the page's history, if it can.
    pub fn back(&self) {
        if self.browser.can_go_back() == 1 {
            self.browser.go_back();
        }
    }

    /// Forward in the page's history, if it can.
    pub fn forward(&self) {
        if self.browser.can_go_forward() == 1 {
            self.browser.go_forward();
        }
    }

    pub fn reload(&self) {
        self.browser.reload();
    }

    pub fn stop(&self) {
        self.browser.stop_load();
    }

    pub fn is_loading(&self) -> bool {
        self.browser.is_loading() == 1
    }

    /// Find `text` in the page, highlighting every match: a new search,
    /// or (`next`) the next match after the current one, or before it
    /// (`!forward`). Results come as [`crate::Event::Found`].
    pub fn find(&self, text: &str, forward: bool, next: bool) {
        if let Some(h) = self.host() {
            h.find(
                Some(&CefString::from(text)),
                i32::from(forward),
                0,
                i32::from(next),
            );
            // A new search from a page without the keys (the find field
            // has them) counts the matches but makes none current; going
            // on to the next one makes the first current.
            if !next {
                h.find(Some(&CefString::from(text)), i32::from(forward), 0, 1);
            }
        }
    }

    /// End find in page, its highlights gone.
    pub fn stop_finding(&self) {
        if let Some(h) = self.host() {
            h.stop_finding(1);
        }
    }

    /// Zoom the page in (`Some(true)`), out, or back to 100% (`None`), in
    /// Chromium's steps.
    pub fn zoom(&self, step: Option<bool>) {
        if let Some(h) = self.host() {
            h.zoom(match step {
                Some(true) => ZoomCommand::IN,
                Some(false) => ZoomCommand::OUT,
                None => ZoomCommand::RESET,
            });
        }
    }

    /// Press and release mouse button 4 (`back`) or 5 at (`x`, `y`) in the
    /// page, from its top left, through the window. For smoke checks.
    pub fn click_side_button(&self, x: f32, y: f32, back: bool) -> Result<(), String> {
        self.surface.click_side_button(&self.browser, x, y, back)
    }

    /// Whether the page is drawn offscreen (Wayland), so it gets mouse and
    /// key events only through [`WebView::send_mouse`] and
    /// [`WebView::send_key`]; elsewhere its own surface takes them.
    pub fn takes_forwarded_input(&self) -> bool {
        self.shared.render.is_some()
    }

    /// A mouse event at (`x`, `y`) in the page, in logical pixels from its
    /// top left. `modifiers` are CEF's event flags.
    pub fn send_mouse(&self, mouse: crate::Mouse, x: f32, y: f32, modifiers: u32) {
        let Some(h) = self.host() else { return };
        let e = MouseEvent {
            x: x.round() as i32,
            y: y.round() as i32,
            modifiers,
        };
        match mouse {
            crate::Mouse::Move => h.send_mouse_move_event(Some(&e), 0),
            crate::Mouse::Leave => h.send_mouse_move_event(Some(&e), 1),
            crate::Mouse::Down { button, clicks } | crate::Mouse::Up { button, clicks } => {
                let up = matches!(mouse, crate::Mouse::Up { .. });
                let button = match button {
                    crate::Button::Left => MouseButtonType::LEFT,
                    crate::Button::Middle => MouseButtonType::MIDDLE,
                    crate::Button::Right => MouseButtonType::RIGHT,
                };
                h.send_mouse_click_event(Some(&e), button, i32::from(up), clicks.max(1) as i32);
            }
            crate::Mouse::Wheel { dx, dy } => {
                h.send_mouse_wheel_event(Some(&e), dx.round() as i32, dy.round() as i32);
            }
        }
    }

    /// A key event for the page (it should have the focus).
    pub fn send_key(&self, key: &crate::Key) {
        let Some(h) = self.host() else { return };
        let event = |type_, character: u16| KeyEvent {
            type_,
            modifiers: key.modifiers,
            windows_key_code: key.windows_key_code,
            native_key_code: key.native_key_code,
            character,
            unmodified_character: character,
            ..Default::default()
        };
        if key.down {
            h.send_key_event(Some(&event(KeyEventType::RAWKEYDOWN, 0)));
            for unit in key.text.encode_utf16() {
                h.send_key_event(Some(&event(KeyEventType::CHAR, unit)));
            }
        } else {
            h.send_key_event(Some(&event(KeyEventType::KEYUP, 0)));
        }
    }

    /// Navigate to `url`.
    pub fn load(&self, url: &str) {
        if let Some(f) = self.browser.main_frame() {
            f.load_url(Some(&CefString::from(url)));
        }
    }

    /// Run `script` in the page's main frame.
    pub fn run_script(&self, script: &str) {
        if let Some(f) = self.browser.main_frame() {
            f.execute_java_script(Some(&CefString::from(script)), None, 0);
        }
    }

    /// A mouse wheel turn at (`x`, `y`) in the view, `dy` pixels (negative
    /// scrolls down).
    pub fn wheel(&self, x: i32, y: i32, dy: i32) {
        if let Some(h) = self.host() {
            h.send_mouse_wheel_event(Some(&MouseEvent { x, y, modifiers: 0 }), 0, dy);
        }
    }

    /// Call DevTools `method` with `params` (a JSON object); `done` gets
    /// the result's JSON, or the error's, on the main thread. Dropped
    /// unanswered if the page closes first.
    fn devtools(&self, method: &str, params: &str, done: Reply) {
        let Some(h) = self.host() else { return };
        let id = MESSAGE.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut r) = self.shared.replies.lock() {
            r.insert(id, done);
        }
        let message = format!(r#"{{"id":{id},"method":"{method}","params":{params}}}"#);
        if h.send_dev_tools_message(Some(message.as_bytes())) != 1
            && let Ok(mut r) = self.shared.replies.lock()
        {
            r.remove(&id);
        }
    }

    /// Call DevTools `method` with `params` (an object); `done` gets the
    /// result, or the error's message, on the main thread. Dropped
    /// unanswered if the page closes first.
    pub fn call(
        &self,
        method: &str,
        params: &serde_json::Value,
        done: impl FnOnce(Result<serde_json::Value, String>) + Send + 'static,
    ) {
        self.devtools(
            method,
            &params.to_string(),
            Box::new(move |r| {
                done(match r {
                    Ok(json) => serde_json::from_slice(&json).map_err(|e| e.to_string()),
                    Err(json) => Err(serde_json::from_slice::<serde_json::Value>(&json)
                        .ok()
                        .and_then(|v| Some(v.get("message")?.as_str()?.to_owned()))
                        .unwrap_or_else(|| String::from_utf8_lossy(&json).into_owned())),
                });
            }),
        );
    }

    /// Ask for an image of the page as drawn now (DevTools
    /// `Page.captureScreenshot`, a PNG at the display's scale). `done` gets
    /// the reply's JSON, with the PNG base64 in `data`, on the main thread:
    /// decode it elsewhere.
    pub fn capture(&self, done: impl FnOnce(Result<Vec<u8>, String>) + Send + 'static) {
        self.devtools(
            "Page.captureScreenshot",
            r#"{"format":"png"}"#,
            Box::new(move |r| done(r.map_err(|e| String::from_utf8_lossy(&e).into_owned()))),
        );
    }

    /// Evaluate `expression` in the page (DevTools `Runtime.evaluate`);
    /// `done` gets its value on the main thread, or `None` if the page did
    /// not give one.
    pub fn evaluate(
        &self,
        expression: &str,
        done: impl FnOnce(Option<serde_json::Value>) + Send + 'static,
    ) {
        let params = serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        });
        self.devtools(
            "Runtime.evaluate",
            &params.to_string(),
            Box::new(move |r| {
                done(r.ok().and_then(|json| {
                    let mut v: serde_json::Value = serde_json::from_slice(&json).ok()?;
                    v.pointer_mut("/result/value").map(serde_json::Value::take)
                }));
            }),
        );
    }

    /// Ask for the page's scroll position in CSS pixels; `done` gets it on
    /// the main thread, or `None` if the page did not say.
    pub fn scroll_position(&self, done: impl FnOnce(Option<(i32, i32)>) + Send + 'static) {
        self.evaluate("[scrollX,scrollY]", move |v| {
            done(v.and_then(|v| {
                let xy = v.as_array()?;
                let n = |i: usize| Some(xy.get(i)?.as_f64()?.round() as i32);
                Some((n(0)?, n(1)?))
            }));
        });
    }

    /// Run one of Chromium's edit commands in the page's focused frame.
    pub fn edit(&self, edit: crate::Edit) {
        let Some(f) = self
            .browser
            .focused_frame()
            .or_else(|| self.browser.main_frame())
        else {
            return;
        };
        match edit {
            crate::Edit::Cut => f.cut(),
            crate::Edit::Copy => f.copy(),
            crate::Edit::Paste => f.paste(),
            crate::Edit::SelectAll => f.select_all(),
        }
    }

    /// Press and release a key as a person types it, queued on the
    /// application's events and delivered through the window by its event
    /// loop (the desktop's shortcuts see it first): `key_code` is the macOS
    /// virtual key code, `chars` what it types. The window must be the key
    /// window. For smoke checks.
    pub fn press_key(&self, key_code: u16, chars: &str, command: bool) -> Result<(), String> {
        self.surface
            .press_key(&self.browser, key_code, chars, command)
    }

    /// Click at (`x`, `y`) in the page, from its top left, through the
    /// window. The window must be the key window. For smoke checks.
    pub fn click(&self, x: f32, y: f32) -> Result<(), String> {
        self.surface.click(&self.browser, x, y)
    }

    /// An input method's composition in the focused page: `marked` as
    /// marked text, then `committed` in its place. For smoke checks.
    pub fn compose(&self, marked: &str, committed: &str) -> Result<(), String> {
        self.surface.compose(&self.browser, marked, committed)
    }

    /// Scroll the page to (`x`, `y`) in CSS pixels: now if it has loaded,
    /// else when it has.
    pub fn restore_scroll(&self, x: i32, y: i32) {
        let script = format!("window.scrollTo({x}, {y})");
        if self.load_status().is_some() {
            self.run_script(&script);
        } else if let Ok(mut s) = self.shared.on_load.lock() {
            *s = Some(script);
        }
    }

    /// The page's title as last reported.
    pub fn title(&self) -> String {
        self.shared
            .title
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default()
    }

    /// The HTTP status of the last finished load of the page (0 for
    /// `data:` and `file:` URLs), or `None` before one finished.
    pub fn load_status(&self) -> Option<i32> {
        let s = self.shared.loaded.load(Ordering::Relaxed);
        (s >= 0).then_some(s)
    }

    /// The window server's number for the window holding the view.
    pub fn window_number(&self) -> Option<isize> {
        self.surface.window_number()
    }

    /// The color space of the screen the view's window is on: what
    /// Chromium draws in with `web.color: display`.
    pub fn screen_color_space(&self) -> Option<crate::ColorSpace> {
        self.surface.screen_color_space()
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        if let Ok(mut f) = self.shared.on_event.lock() {
            *f = None;
        }
        // Its downloads end with the page: canceled, and the file each was
        // being saved to (reserved by the caller, empty until complete)
        // removed, off this thread.
        let downloads: Vec<Download> = self
            .shared
            .downloads
            .lock()
            .map(|mut d| d.drain().map(|(_, d)| d).collect())
            .unwrap_or_default();
        let mut leftover = Vec::new();
        for d in downloads {
            if let Some(item) = &d.item {
                item.cancel();
            }
            leftover.extend(d.saving_to);
        }
        if !leftover.is_empty() {
            std::thread::spawn(move || {
                for p in leftover {
                    let _ = std::fs::remove_file(p);
                }
            });
        }
        if let Ok(mut r) = self.shared.replies.lock() {
            r.clear();
        }
        // Gone from the screen at once; CEF removes its surface as it
        // closes.
        self.surface.set_visible(&self.browser, false);
        if let Some(h) = self.host() {
            h.close_browser(1);
        }
    }
}
