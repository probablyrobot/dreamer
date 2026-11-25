//! Linux: check that a Dreamer Web component draws a page, with no display.
//!
//! `offscreen COMPONENT_DIR` loads `libcef.so` from the component at run
//! time (as the desktop does), starts Chromium with no display server
//! (`--ozone-platform=headless`, the GPU process on the component's
//! SwiftShader) and the component's helper, opens a page
//! of one colour in a browser drawn offscreen into memory, and reads the
//! centre pixel of what Chromium paints. It exits 0 when that pixel is the
//! page's colour. `cargo xtask web bundle` and `cargo xtask package
//! --platform web` run it on the component they make.
#![allow(unsafe_code)]

#[cfg(target_os = "linux")]
mod check {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use cef_sys::cef::*;

    /// The page's colour, and the page.
    const COLOR: [u8; 3] = [18, 52, 86];
    const PAGE: &str = "data:text/html,<body style='margin:0;background:rgb(18,52,86)'></body>";
    const WIDTH: i32 = 320;
    const HEIGHT: i32 = 200;

    /// The centre pixel (RGB) of the last paint, and its size.
    static PAINTED: Mutex<Option<([u8; 3], i32, i32)>> = Mutex::new(None);

    wrap_app! {
        struct Headless;
        impl App {
            fn on_before_command_line_processing(
                &self,
                process_type: Option<&CefString>,
                command_line: Option<&mut CommandLine>,
            ) {
                let browser = process_type.is_none_or(|t| t.to_string().is_empty());
                if let (true, Some(cl)) = (browser, command_line) {
                    cl.append_switch_with_value(
                        Some(&CefString::from("ozone-platform")),
                        Some(&CefString::from("headless")),
                    );
                    cl.append_switch_with_value(
                        Some(&CefString::from("password-store")),
                        Some(&CefString::from("basic")),
                    );
                    // No display, so no GPU driver: the GPU process draws
                    // with the component's SwiftShader.
                    cl.append_switch_with_value(
                        Some(&CefString::from("use-angle")),
                        Some(&CefString::from("swiftshader")),
                    );
                }
            }
        }
    }

    wrap_render_handler! {
        struct Paint;
        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
                if let Some(r) = rect {
                    *r = Rect { x: 0, y: 0, width: WIDTH, height: HEIGHT };
                }
            }

            fn on_paint(
                &self,
                _browser: Option<&mut Browser>,
                kind: PaintElementType,
                _dirty: Option<&[Rect]>,
                buffer: *const u8,
                width: i32,
                height: i32,
            ) {
                if kind != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                    return;
                }
                let len = width as usize * height as usize * 4;
                // SAFETY: CEF lends a BGRA buffer of `width` × `height`
                // pixels for the length of this call.
                let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
                let at = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
                let rgb = [pixels[at + 2], pixels[at + 1], pixels[at]];
                if let Ok(mut p) = PAINTED.lock() {
                    *p = Some((rgb, width, height));
                }
            }
        }
    }

    wrap_client! {
        struct Offscreen;
        impl Client {
            fn render_handler(&self) -> Option<RenderHandler> {
                Some(Paint::new())
            }
        }
    }

    fn path(p: &Path) -> CefString {
        CefString::from(p.to_string_lossy().as_ref())
    }

    pub fn run() -> Result<String, String> {
        let component = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .ok_or("usage: offscreen COMPONENT_DIR")?;
        let component = component
            .canonicalize()
            .map_err(|e| format!("{}: {e}", component.display()))?;
        cef_sys::loader::load(&component.join(web_component::LIBRARY))?;
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let cache =
            std::env::temp_dir().join(format!("dreamer-web-offscreen-{}", std::process::id()));
        let settings = Settings {
            no_sandbox: 1,
            external_message_pump: 1,
            windowless_rendering_enabled: 1,
            root_cache_path: path(&cache),
            cache_path: path(&cache.join("default")),
            resources_dir_path: path(&component),
            locales_dir_path: path(&component.join("locales")),
            browser_subprocess_path: path(&web_component::helper(&component)),
            log_file: path(&cache.join("chromium.log")),
            log_severity: LogSeverity::WARNING,
            ..Default::default()
        };
        let args = args::Args::new();
        let mut app = Headless::new();
        if initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 1
        {
            return Err(format!(
                "Chromium did not start (see {})",
                cache.join("chromium.log").display()
            ));
        }
        let info = WindowInfo {
            windowless_rendering_enabled: 1,
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };
        let mut client = Offscreen::new();
        let browser = browser_host_create_browser_sync(
            Some(&info),
            Some(&mut client),
            Some(&CefString::from(PAGE)),
            Some(&BrowserSettings::default()),
            None,
            None,
        )
        .ok_or("no browser was made")?;
        let started = Instant::now();
        let mut seen = None;
        while started.elapsed() < Duration::from_secs(30) {
            do_message_loop_work();
            seen = PAINTED.lock().ok().and_then(|p| *p);
            if seen.is_some_and(|(rgb, _, _)| rgb == COLOR) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let took = started.elapsed();
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        let closing = Instant::now();
        while closing.elapsed() < Duration::from_secs(2) {
            do_message_loop_work();
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(browser);
        drop(client);
        shutdown();
        let _ = std::fs::remove_dir_all(&cache);
        match seen {
            Some((rgb, w, h)) if rgb == COLOR => Ok(format!(
                "Chromium {} from {} painted the page offscreen ({w}x{h}): centre pixel \
                 rgb{rgb:?} after {} ms",
                web_component::CEF_VERSION,
                component.display(),
                took.as_millis()
            )),
            Some((rgb, _, _)) => Err(format!(
                "the page painted rgb{rgb:?} at its centre, not rgb{COLOR:?}"
            )),
            None => Err("Chromium painted nothing in 30 s".into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    match check::run() {
        Ok(line) => println!("offscreen: {line}"),
        Err(e) => {
            eprintln!("offscreen: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("offscreen: Linux only");
    std::process::exit(2);
}
