//! CEF on macOS: the framework, the external message pump, and browsers
//! parented to a window's view.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cef_sys::cef::{self, *};
use cef_sys::mac as appkit;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::Rect;

/// Where the framework sits relative to an executable in `Contents/MacOS`.
const FRAMEWORK: &str = "Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// Called by CEF, from any thread, when it wants [`do_work`] after the
/// given delay in milliseconds.
static WAKE: OnceLock<Box<dyn Fn(i64) + Send + Sync>> = OnceLock::new();
/// Browsers created and not yet closed.
static LIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The outcome of [`start`], once tried.
    static STARTED: RefCell<Option<Result<(), String>>> = const { RefCell::new(None) };
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
                _process_type: Option<&CefString>,
                command_line: Option<&mut CommandLine>,
            ) {
                // Chromium's cookie encryption key would otherwise go into
                // the login keychain, with a prompt.
                if let Some(cl) = command_line {
                    cl.append_switch(Some(&CefString::from("use-mock-keychain")));
                }
            }
            fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
                Some(DreamerBrowserProcess::new())
            }
        }
    }

    wrap_browser_process_handler! {
        pub struct DreamerBrowserProcess;
        impl BrowserProcessHandler {
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
        }
    }

    wrap_focus_handler! {
        pub struct DreamerFocus {
            shared: Arc<Shared>,
        }
        impl FocusHandler {
            fn on_got_focus(&self, _browser: Option<&mut Browser>) {
                if let Ok(f) = self.shared.on_focus.lock()
                    && let Some(f) = f.as_ref()
                {
                    f();
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
            // One page per pane: no new windows.
            fn on_before_popup(
                &self,
                _browser: Option<&mut Browser>,
                _frame: Option<&mut Frame>,
                _popup_id: i32,
                _target_url: Option<&CefString>,
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
                1
            }
            fn on_before_close(&self, _browser: Option<&mut Browser>) {
                if !self.shared.closed.swap(true, Ordering::Relaxed) {
                    LIVE.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// A DevTools reply's handler: the result's JSON, or the error's.
type Reply = Box<dyn FnOnce(Result<Vec<u8>, Vec<u8>>) + Send>;

/// What a browser's handlers report, read from the main thread.
#[derive(Default)]
struct Shared {
    title: Mutex<String>,
    /// HTTP status of the main frame's last finished load; -1 before one.
    loaded: AtomicI32,
    closed: AtomicBool,
    on_focus: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// A script for the main frame when its load finishes, once.
    on_load: Mutex<Option<String>>,
    /// DevTools calls waiting for their replies, by message id.
    replies: Mutex<HashMap<i32, Reply>>,
}

/// DevTools message ids, unique in the process.
static MESSAGE: AtomicI32 = AtomicI32::new(1);

/// The path of the framework binary next to the running executable's
/// bundle, if it is there.
fn framework_beside(exe: &Path, helper: bool) -> Option<PathBuf> {
    let up = if helper { "../../.." } else { "../Frameworks" };
    let path = exe.parent()?.join(up).join(FRAMEWORK);
    path.is_file().then_some(path)
}

/// Load the framework from the app bundle (once per process; it stays
/// loaded).
fn load_framework(helper: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    if framework_beside(&exe, helper).is_none() {
        return Err(
            "the Chromium Embedded Framework is not in this app's bundle \
             (a build with web panes runs from a bundle made by `cargo xtask web bundle`)"
                .into(),
        );
    }
    let loader = library_loader::LibraryLoader::new(&exe, helper);
    if !loader.load() {
        return Err("the Chromium Embedded Framework did not load".into());
    }
    // Unloading on drop would pull the framework out from under CEF.
    std::mem::forget(loader);
    // Declares the API version the wrapper was built for.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    Ok(())
}

/// Start CEF in this process, once: load the framework from the app
/// bundle, make the application object answer CEF's protocol, and
/// initialize with an external message pump. `cache_dir` holds Chromium's
/// profile. `wake` is called from any thread with a delay in milliseconds
/// whenever CEF wants [`do_work`]; the caller should also call it at least
/// every 33 ms or so while browsers exist.
///
/// Main thread only, inside the toolkit's running application. A second
/// call returns the first one's outcome.
pub fn start(cache_dir: &Path, wake: impl Fn(i64) + Send + Sync + 'static) -> Result<(), String> {
    if let Some(done) = STARTED.with(|s| s.borrow().clone()) {
        return done;
    }
    let result = (|| {
        load_framework(false)?;
        // SAFETY: main thread, framework loaded, NSApp exists (inside the
        // toolkit's run loop), before initialize.
        unsafe { appkit::adopt_application_protocol()? };
        let _ = WAKE.set(Box::new(wake));
        std::fs::create_dir_all(cache_dir).map_err(|e| format!("{}: {e}", cache_dir.display()))?;
        let settings = Settings {
            // Chromium's sandbox is not set up for these helpers yet.
            no_sandbox: 1,
            external_message_pump: 1,
            multi_threaded_message_loop: 0,
            root_cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
            cache_path: CefString::from(cache_dir.join("default").to_string_lossy().as_ref()),
            log_file: CefString::from(cache_dir.join("chromium.log").to_string_lossy().as_ref()),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        };
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
    }
}

/// Close CEF after the last browser has closed, pumping its work until
/// then, at most for `timeout`. Every [`WebView`] must have been dropped.
pub fn shutdown(timeout: Duration) -> Result<(), String> {
    if !started() {
        return Ok(());
    }
    let until = Instant::now() + timeout;
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
    cef::shutdown();
    STARTED.with(|s| *s.borrow_mut() = Some(Err("Chromium was shut down".into())));
    Ok(())
}

/// The entry point of the helper executable (CEF's renderer, GPU, and
/// utility processes): returns its exit code.
pub fn run_subprocess() -> i32 {
    if let Err(e) = load_framework(true) {
        eprintln!("dreamer-web-helper: {e}");
        return 1;
    }
    let args = args::Args::new();
    execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    )
}

/// The `NSView` of a window, from its handle.
fn parent_view(window: &impl HasWindowHandle) -> Result<*mut c_void, String> {
    let handle = window
        .window_handle()
        .map_err(|e| format!("no window handle: {e}"))?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(h) => Ok(h.ns_view.as_ptr()),
        _ => Err("not an AppKit window".into()),
    }
}

fn frame_of(r: Rect) -> appkit::Frame {
    appkit::Frame {
        x: f64::from(r.x),
        y: f64::from(r.y),
        width: f64::from(r.width.max(1.0)),
        height: f64::from(r.height.max(1.0)),
    }
}

/// One page, drawn by Chromium in a child view of a window's view. Closed
/// when dropped.
pub struct WebView {
    browser: Browser,
    shared: Arc<Shared>,
    parent: *mut c_void,
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
    /// keyboard focus. `on_focus` runs (on CEF's UI thread, the main
    /// thread) whenever the page takes focus, for example on a click.
    pub fn open(
        window: &impl HasWindowHandle,
        rect: Rect,
        url: &str,
        on_focus: impl Fn() + Send + Sync + 'static,
    ) -> Result<WebView, String> {
        if !started() {
            return Err("Chromium is not running".into());
        }
        let parent = parent_view(window)?;
        // SAFETY: `parent` is the window's live content view; main thread.
        let frame = unsafe { appkit::from_top_left(parent, frame_of(rect)) };
        let shared = Arc::new(Shared {
            loaded: AtomicI32::new(-1),
            on_focus: Mutex::new(Some(Box::new(on_focus))),
            ..Shared::default()
        });
        let info = WindowInfo {
            parent_view: parent,
            bounds: cef::Rect {
                x: frame.x as i32,
                y: frame.y as i32,
                width: frame.width as i32,
                height: frame.height as i32,
            },
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };
        let mut client = handlers::DreamerClient::new(shared.clone());
        let browser = browser_host_create_browser_sync(
            Some(&info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&BrowserSettings::default()),
            None,
            None,
        )
        .ok_or("Chromium could not create a browser")?;
        LIVE.fetch_add(1, Ordering::Relaxed);
        let devtools = browser.host().and_then(|h| {
            h.add_dev_tools_message_observer(Some(&mut handlers::DreamerDevTools::new(
                shared.clone(),
            )))
        });
        let view = WebView {
            browser,
            shared,
            parent,
            placed: Cell::new(None),
            visible: Cell::new(true),
            _devtools: devtools,
        };
        view.place(rect);
        // Creating the browser makes its view first responder; the pane
        // takes focus only when Dreamer gives it.
        // SAFETY: live content view, main thread.
        unsafe { appkit::make_first_responder(parent) };
        Ok(view)
    }

    fn host(&self) -> Option<BrowserHost> {
        self.browser.host()
    }

    fn view(&self) -> *mut c_void {
        self.host()
            .map_or(std::ptr::null_mut(), |h| h.window_handle().cast())
    }

    /// Keep the view on `rect` (window content coordinates from the top
    /// left). Cheap when nothing changed.
    pub fn place(&self, rect: Rect) {
        if self.placed.get() == Some(rect) {
            return;
        }
        self.placed.set(Some(rect));
        // SAFETY: the parent is the window's live content view and the
        // view is CEF's live child of it (or null); main thread.
        unsafe {
            let frame = appkit::from_top_left(self.parent, frame_of(rect));
            appkit::set_frame(self.view(), frame);
        }
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
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::set_hidden(self.view(), !visible) };
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
        if !self.has_focus() {
            // SAFETY: CEF's live view or null; main thread.
            unsafe { appkit::make_first_responder(self.view()) };
        }
    }

    /// Whether the page's view has the window's keyboard focus now.
    pub fn has_focus(&self) -> bool {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::has_first_responder(self.view()) }
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
        // SAFETY: the window's live content view; main thread.
        unsafe { appkit::make_first_responder(self.parent) };
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
        let params = serde_json::json!({ "expression": expression, "returnByValue": true });
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

    /// Press and release a key as a person types it, through the window
    /// (the desktop's shortcuts see it first): `key_code` is the macOS
    /// virtual key code, `chars` what it types. The window must be the key
    /// window. For smoke checks.
    pub fn press_key(&self, key_code: u16, chars: &str, command: bool) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::press_key(self.view(), key_code, chars, command) }
    }

    /// Click at (`x`, `y`) in the page, from its top left, through the
    /// window. The window must be the key window. For smoke checks.
    pub fn click(&self, x: f32, y: f32) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::click(self.view(), f64::from(x), f64::from(y)) }
    }

    /// An input method's composition in the focused page: `marked` as
    /// marked text, then `committed` in its place. For smoke checks.
    pub fn compose(&self, marked: &str, committed: &str) -> Result<(), String> {
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::compose(self.view(), marked, committed) }
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
        // SAFETY: the window's live content view; main thread.
        unsafe { appkit::window_number(self.parent) }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        if let Ok(mut f) = self.shared.on_focus.lock() {
            *f = None;
        }
        if let Ok(mut r) = self.shared.replies.lock() {
            r.clear();
        }
        // Gone from the screen at once; CEF removes the view as it closes.
        // SAFETY: CEF's live view or null; main thread.
        unsafe { appkit::set_hidden(self.view(), true) };
        if let Some(h) = self.host() {
            h.close_browser(1);
        }
    }
}
