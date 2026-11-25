//! CEF on Linux: `libcef.so` loaded at run time from Dreamer Web (the
//! component installed apart from the app, with Chromium's resources and
//! the helper beside the library); a page's surface on either display
//! server.
//!
//! - **X11:** CEF's own X11 window, a child of the toolkit's window, moved
//!   and shown over the X connection of this module.
//! - **Wayland:** CEF cannot be parented to a Wayland surface, so it renders
//!   offscreen on the GPU (shared textures). Each frame it lends (a dmabuf)
//!   is copied on the GPU into a buffer of ours ([`cef_sys::dmabuf`]), and
//!   that buffer is attached to a subsurface of the toolkit's surface, on
//!   the toolkit's own Wayland connection with an event queue of ours. The
//!   subsurface takes no input: the toolkit gets the pointer and keys over
//!   the page, and the pane forwards them (`WebView::send_mouse`,
//!   `WebView::send_key`).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use cef_sys::cef::{self, *};
use cef_sys::dmabuf;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};

use crate::Rect;

/// Which display server pages are drawn for, once Chromium started: 0
/// before, then X11 or Wayland. Chromium takes it as it initializes.
static SERVER: AtomicU8 = AtomicU8::new(0);
const X11: u8 = 1;
const WAYLAND: u8 = 2;

/// A key event as the platform delivered it (unused).
pub(super) type OsEvent<'a> = Option<&'a mut cef_sys::cef::sys::_XEvent>;

/// Chromium's switches for this platform: cookies are encrypted with a key
/// of the profile's own, not one from the desktop's keyring (which could
/// prompt); and the browser process draws for the display server the
/// toolkit uses (Chromium passes it on to its helpers).
pub(super) fn switches(cl: &mut CommandLine, browser: bool) {
    cl.append_switch_with_value(
        Some(&CefString::from("password-store")),
        Some(&CefString::from("basic")),
    );
    if browser {
        let ozone = match SERVER.load(Ordering::Relaxed) {
            WAYLAND => "wayland",
            _ => "x11",
        };
        cl.append_switch_with_value(
            Some(&CefString::from("ozone-platform")),
            Some(&CefString::from(ozone)),
        );
    }
}

/// Load `libcef.so` at `path` and declare the API version the bindings
/// are for.
fn load_library(path: &Path) -> Result<(), String> {
    cef_sys::loader::load(path).map_err(|e| format!("Chromium did not load: {e}"))?;
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    Ok(())
}

/// Dreamer Web, installed, for a window of one display server.
pub(super) struct Chromium {
    component: PathBuf,
    wayland: bool,
}

impl Chromium {
    /// The installed component, if it is this build's and complete.
    pub(super) fn find(window: &impl HasWindowHandle) -> Result<Chromium, String> {
        let installed = web_component::installed_dir();
        if let Some(problem) = web_component::check(&installed).problem(&installed) {
            return Err(problem);
        }
        let handle = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?;
        let wayland = match handle.as_raw() {
            RawWindowHandle::Wayland(_) => true,
            RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_) => false,
            _ => return Err("not an X11 or Wayland window".into()),
        };
        // A copy used in place is linked to: Chromium is told where it
        // really is.
        Ok(Chromium {
            component: installed.canonicalize().unwrap_or(installed),
            wayland,
        })
    }

    /// Load the library from the component, and note the display server.
    pub(super) fn load(&self) -> Result<(), String> {
        load_library(&self.component.join(web_component::LIBRARY))?;
        SERVER.store(if self.wayland { WAYLAND } else { X11 }, Ordering::Relaxed);
        Ok(())
    }

    /// Resources, locales, and the helper in the component; on Wayland,
    /// pages are drawn offscreen.
    pub(super) fn settings(&self, settings: Settings) -> Result<Settings, String> {
        let path = |p: PathBuf| CefString::from(p.to_string_lossy().as_ref());
        Ok(Settings {
            resources_dir_path: path(self.component.clone()),
            locales_dir_path: path(self.component.join("locales")),
            browser_subprocess_path: path(web_component::helper(&self.component)),
            windowless_rendering_enabled: i32::from(self.wayland),
            ..settings
        })
    }
}

/// In a helper: load the library beside it, in the component.
pub(super) fn load_in_helper() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let dir = exe.parent().ok_or("this executable has no directory")?;
    let path = dir.join(web_component::LIBRARY);
    if !path.is_file() {
        return Err(format!("{} is missing", path.display()));
    }
    load_library(&path)
}

/// A closing browser. An offscreen page has no window: CEF closes it. A
/// windowed page's X11 window is destroyed here, which completes the close
/// (CEF would otherwise ask the toplevel window to close).
pub(super) fn close(browser: &mut Browser) -> i32 {
    let Some(window) = browser.host().map(|h| h.window_handle()) else {
        return 0;
    };
    if window == 0 {
        return 0;
    }
    x11::with(|c| {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let _ = c.destroy_window(window as u32);
        let _ = x11rb::connection::Connection::flush(c);
    });
    1
}

/// A page's surface.
pub(super) enum Surface {
    /// CEF's child window of the toolkit's X11 window (its id).
    X11 { parent: u32 },
    /// A subsurface of the toolkit's Wayland surface.
    Wayland(Arc<wayland::Page>),
}

const NOT_YET: &str = "not available on Linux yet";

impl Surface {
    pub(super) fn new(
        window: &(impl HasWindowHandle + HasDisplayHandle),
        rect: Rect,
    ) -> Result<Surface, String> {
        let handle = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?;
        match handle.as_raw() {
            RawWindowHandle::Xcb(h) => Ok(Surface::X11 {
                parent: h.window.get(),
            }),
            RawWindowHandle::Xlib(h) => Ok(Surface::X11 {
                parent: h.window as u32,
            }),
            RawWindowHandle::Wayland(h) => {
                let display = window
                    .display_handle()
                    .map_err(|e| format!("no display handle: {e}"))?;
                let RawDisplayHandle::Wayland(d) = display.as_raw() else {
                    return Err("a Wayland window without a Wayland display".into());
                };
                // SAFETY: both come from the toolkit's live window and its
                // connection, which outlive the page (the pane closes its
                // page before its window goes).
                let page =
                    unsafe { wayland::Page::new(d.display.as_ptr(), h.surface.as_ptr(), rect)? };
                Ok(Surface::Wayland(Arc::new(page)))
            }
            _ => Err("not an X11 or Wayland window".into()),
        }
    }

    /// How an offscreen page paints (Wayland).
    pub(super) fn render_handler(&self) -> Option<RenderHandler> {
        match self {
            Surface::X11 { .. } => None,
            Surface::Wayland(page) => Some(wayland::Paint::new(page.clone())),
        }
    }

    pub(super) fn window_info(&self, rect: Rect) -> WindowInfo {
        match self {
            Surface::X11 { parent } => WindowInfo {
                parent_window: std::ffi::c_ulong::from(*parent),
                bounds: device(rect),
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            },
            Surface::Wayland(_) => WindowInfo {
                windowless_rendering_enabled: 1,
                shared_texture_enabled: 1,
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            },
        }
    }

    pub(super) fn opened(&self, browser: &Browser) {
        if let Surface::Wayland(page) = self {
            page.set_host(browser.host());
        }
    }

    pub(super) fn place(&self, browser: &Browser, rect: Rect) {
        match self {
            Surface::X11 { .. } => {
                let Some(child) = child_window(browser) else {
                    return;
                };
                let r = device(rect);
                x11::with(|c| {
                    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _};
                    let aux = ConfigureWindowAux::new()
                        .x(r.x)
                        .y(r.y)
                        .width(r.width.max(1) as u32)
                        .height(r.height.max(1) as u32);
                    let _ = c.configure_window(child, &aux);
                    let _ = x11rb::connection::Connection::flush(c);
                });
            }
            Surface::Wayland(page) => {
                let scaled = page.place(rect);
                if scaled && let Some(h) = browser.host() {
                    h.notify_screen_info_changed();
                }
            }
        }
    }

    pub(super) fn set_visible(&self, browser: &Browser, visible: bool) {
        match self {
            Surface::X11 { .. } => {
                let Some(child) = child_window(browser) else {
                    return;
                };
                x11::with(|c| {
                    use x11rb::protocol::xproto::ConnectionExt as _;
                    let _ = if visible {
                        c.map_window(child)
                    } else {
                        c.unmap_window(child)
                    };
                    let _ = x11rb::connection::Connection::flush(c);
                });
            }
            Surface::Wayland(page) => {
                page.set_visible(visible);
                if visible && let Some(h) = browser.host() {
                    h.invalidate(PaintElementType::VIEW);
                }
            }
        }
    }

    /// CEF focuses its X11 window as it is told to take focus; nothing more
    /// to do here.
    pub(super) fn take_focus(&self, _browser: &Browser) {}

    /// Whether the X server's input focus is in the page's window; `None`
    /// for an offscreen page, which has the focus it was last given.
    pub(super) fn has_focus(&self, browser: &Browser) -> Option<bool> {
        match self {
            Surface::X11 { .. } => {
                let child = child_window(browser)?;
                Some(x11::with(|c| x11::focus_inside(c, child)).unwrap_or(false))
            }
            Surface::Wayland(_) => None,
        }
    }

    /// Give the keys back to the toolkit's window.
    pub(super) fn give_focus_back(&self, browser: &Browser) {
        if let Surface::X11 { parent } = self {
            let inside = child_window(browser)
                .and_then(|child| x11::with(|c| x11::focus_inside(c, child)))
                .unwrap_or(false);
            if inside {
                x11::with(|c| {
                    use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus};
                    let _ = c.set_input_focus(InputFocus::PARENT, *parent, x11rb::CURRENT_TIME);
                    let _ = x11rb::connection::Connection::flush(c);
                });
            }
        }
    }

    pub(super) fn click_side_button(
        &self,
        _browser: &Browser,
        _x: f32,
        _y: f32,
        _back: bool,
    ) -> Result<(), String> {
        Err(format!("side button clicks: {NOT_YET}"))
    }

    pub(super) fn press_key(
        &self,
        _browser: &Browser,
        _key_code: u16,
        _chars: &str,
        _command: bool,
    ) -> Result<(), String> {
        Err(format!("key presses: {NOT_YET}"))
    }

    pub(super) fn click(&self, _browser: &Browser, _x: f32, _y: f32) -> Result<(), String> {
        Err(format!("clicks: {NOT_YET}"))
    }

    pub(super) fn compose(
        &self,
        _browser: &Browser,
        _marked: &str,
        _committed: &str,
    ) -> Result<(), String> {
        Err(format!("input method compositions: {NOT_YET}"))
    }

    pub(super) fn window_number(&self) -> Option<isize> {
        None
    }

    pub(super) fn screen_color_space(&self) -> Option<crate::ColorSpace> {
        None
    }
}

/// `rect` in device pixels, as X11 places windows.
fn device(rect: Rect) -> cef::Rect {
    let s = if rect.scale > 0.0 { rect.scale } else { 1.0 };
    cef::Rect {
        x: (rect.x * s).round() as i32,
        y: (rect.y * s).round() as i32,
        width: ((rect.width * s).round() as i32).max(1),
        height: ((rect.height * s).round() as i32).max(1),
    }
}

/// CEF's X11 window for a windowed page.
fn child_window(browser: &Browser) -> Option<u32> {
    let w = browser.host()?.window_handle();
    (w != 0).then_some(w as u32)
}

mod x11 {
    //! This module's own connection to the X server, for CEF's windows.

    use std::cell::RefCell;

    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    thread_local! {
        static CONNECTION: RefCell<Option<RustConnection>> = const { RefCell::new(None) };
    }

    /// Run `f` with the connection, made on first use; `None` without an
    /// X server.
    pub(super) fn with<R>(f: impl FnOnce(&RustConnection) -> R) -> Option<R> {
        CONNECTION.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_none() {
                *c = x11rb::connect(None).ok().map(|(conn, _)| conn);
            }
            c.as_ref().map(f)
        })
    }

    /// Whether the input focus is `window` or inside it.
    pub(super) fn focus_inside(c: &RustConnection, window: u32) -> bool {
        let Some(mut at) = c
            .get_input_focus()
            .ok()
            .and_then(|r| r.reply().ok())
            .map(|r| r.focus)
        else {
            return false;
        };
        // Up the tree: CEF's window holds Chromium's own.
        for _ in 0..8 {
            if at == window {
                return true;
            }
            match c.query_tree(at).ok().and_then(|r| r.reply().ok()) {
                Some(t) if t.parent != 0 && t.parent != t.root => at = t.parent,
                _ => return false,
            }
        }
        false
    }
}

pub(super) mod wayland {
    //! A page drawn offscreen into a subsurface of the toolkit's surface.

    use super::*;

    use std::os::fd::BorrowedFd;

    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_region, wl_registry, wl_subcompositor, wl_subsurface,
        wl_surface,
    };
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
    };
    use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

    /// `DRM_FORMAT_ARGB8888`: Chromium's BGRA in memory.
    const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
    /// `DRM_FORMAT_MOD_LINEAR`.
    const LINEAR: u64 = 0;
    /// Buffers per page: one on screen, one the compositor may still read,
    /// one being filled.
    const BUFFERS: usize = 3;

    /// This module's state for its event queue.
    pub(crate) struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// A buffer the compositor gave back may be filled again.
    impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
        fn event(
            _: &mut Self,
            _: &wl_buffer::WlBuffer,
            event: wl_buffer::Event,
            busy: &Arc<AtomicBool>,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_buffer::Event::Release = event {
                busy.store(false, Ordering::Release);
            }
        }
    }

    delegate_noop!(State: ignore wl_compositor::WlCompositor);
    delegate_noop!(State: ignore wl_surface::WlSurface);
    delegate_noop!(State: ignore wl_region::WlRegion);
    delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
    delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
    delegate_noop!(State: ignore wp_viewporter::WpViewporter);
    delegate_noop!(State: ignore wp_viewport::WpViewport);
    delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
    delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

    /// One of a page's buffers: ours on the GPU, and the compositor's
    /// handle for it.
    struct Slot {
        target: dmabuf::Target,
        buffer: wl_buffer::WlBuffer,
        /// Attached and not yet released by the compositor.
        busy: Arc<AtomicBool>,
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            self.buffer.destroy();
        }
    }

    /// What changes as the page paints and moves.
    struct Live {
        rect: Rect,
        visible: bool,
        /// The GPU copying frames, once one has copied a frame.
        copier: Option<dmabuf::Copier>,
        /// Why no GPU could, so it is not tried again for every frame.
        failed: Option<String>,
        slots: Vec<Slot>,
        /// The slot on screen (or last on screen, while hidden).
        shown: Option<usize>,
        host: Option<BrowserHost>,
    }

    /// A page's subsurface, on the toolkit's connection.
    pub(crate) struct Page {
        conn: Connection,
        queue: Mutex<EventQueue<State>>,
        qh: QueueHandle<State>,
        surface: wl_surface::WlSurface,
        subsurface: wl_subsurface::WlSubsurface,
        viewport: Option<wp_viewport::WpViewport>,
        dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        live: Mutex<Live>,
    }

    // SAFETY (for the raw pointers below): see `Page::new`.
    impl Page {
        /// A subsurface of `parent` on the connection `display`, placed on
        /// `rect` and shown once the page paints.
        ///
        /// # Safety
        ///
        /// `display` must be the toolkit's live `wl_display` and `parent`
        /// its live `wl_surface` for the window; both must outlive the page.
        pub(crate) unsafe fn new(
            display: *mut c_void,
            parent: *mut c_void,
            rect: Rect,
        ) -> Result<Page, String> {
            // SAFETY: the caller's promise; a foreign display is never
            // closed by this backend.
            let backend = unsafe { Backend::from_foreign_display(display.cast()) };
            let conn = Connection::from_backend(backend);
            let (globals, queue) = registry_queue_init::<State>(&conn)
                .map_err(|e| format!("Wayland: the registry: {e}"))?;
            let qh = queue.handle();
            let compositor: wl_compositor::WlCompositor = globals
                .bind(&qh, 1..=4, ())
                .map_err(|e| format!("Wayland: wl_compositor: {e}"))?;
            let subcompositor: wl_subcompositor::WlSubcompositor = globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("Wayland: wl_subcompositor: {e}"))?;
            let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals
                .bind(&qh, 3..=3, ())
                .map_err(|e| format!("Wayland: zwp_linux_dmabuf_v1: {e}"))?;
            let viewporter: Option<wp_viewporter::WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
            // SAFETY: the caller's promise: a live wl_surface proxy.
            let id =
                unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), parent.cast()) }
                    .map_err(|_| "Wayland: the window's surface is not usable".to_string())?;
            let parent = wl_surface::WlSurface::from_id(&conn, id)
                .map_err(|_| "Wayland: the window's surface is not usable".to_string())?;
            let surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&surface, &parent, &qh, ());
            // Its own commits show at once; its position with the window's
            // next frame.
            subsurface.set_desync();
            subsurface.set_position(rect.x.round() as i32, rect.y.round() as i32);
            // The pointer and keys over the page stay the window's.
            let region = compositor.create_region(&qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            let viewport = viewporter
                .as_ref()
                .map(|v| v.get_viewport(&surface, &qh, ()));
            surface.commit();
            conn.flush().map_err(|e| format!("Wayland: {e}"))?;
            Ok(Page {
                conn,
                queue: Mutex::new(queue),
                qh,
                surface,
                subsurface,
                viewport,
                dmabuf,
                live: Mutex::new(Live {
                    rect,
                    visible: true,
                    copier: None,
                    failed: None,
                    slots: Vec::new(),
                    shown: None,
                    host: None,
                }),
            })
        }

        pub(crate) fn set_host(&self, host: Option<BrowserHost>) {
            if let Ok(mut l) = self.live.lock() {
                l.host = host;
            }
        }

        /// Run what the compositor said to this page (buffers released).
        fn dispatch(&self) {
            if let Ok(mut q) = self.queue.lock() {
                let _ = q.dispatch_pending(&mut State);
            }
        }

        /// Keep the subsurface on `rect`; whether the scale changed.
        pub(crate) fn place(&self, rect: Rect) -> bool {
            self.dispatch();
            let Ok(mut l) = self.live.lock() else {
                return false;
            };
            let scaled = l.rect.scale != rect.scale;
            self.subsurface
                .set_position(rect.x.round() as i32, rect.y.round() as i32);
            l.rect = rect;
            if l.visible
                && let Some(i) = l.shown
                && let Some(s) = l.slots.get(i)
            {
                // The buffer stays until a frame of the new size comes; it
                // is shown at the new size meanwhile.
                self.show(&s.target, &s.buffer, &rect, false);
            }
            let _ = self.conn.flush();
            scaled
        }

        pub(crate) fn set_visible(&self, visible: bool) {
            self.dispatch();
            let Ok(mut l) = self.live.lock() else { return };
            if l.visible == visible {
                return;
            }
            l.visible = visible;
            if !visible {
                self.surface.attach(None, 0, 0);
                self.surface.commit();
            }
            let _ = self.conn.flush();
        }

        /// Attach `buffer` to the subsurface, sized for `rect`.
        fn show(
            &self,
            target: &dmabuf::Target,
            buffer: &wl_buffer::WlBuffer,
            rect: &Rect,
            new: bool,
        ) {
            if new {
                self.surface.attach(Some(buffer), 0, 0);
                self.surface
                    .damage_buffer(0, 0, target.width as i32, target.height as i32);
            }
            match &self.viewport {
                Some(v) => v.set_destination(
                    (rect.width.round() as i32).max(1),
                    (rect.height.round() as i32).max(1),
                ),
                None => self
                    .surface
                    .set_buffer_scale(rect.scale.round().max(1.0) as i32),
            }
            self.surface.commit();
        }

        /// The page's size in logical pixels, as Chromium lays it out.
        fn view_size(&self) -> (i32, i32, f32) {
            let r = self.live.lock().map(|l| l.rect).unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                scale: 1.0,
            });
            (
                (r.width.round() as i32).max(1),
                (r.height.round() as i32).max(1),
                if r.scale > 0.0 { r.scale } else { 1.0 },
            )
        }

        /// A frame Chromium lent: copy it into a free buffer of ours and
        /// show that.
        fn paint(&self, info: &AcceleratedPaintInfo) {
            self.dispatch();
            let Ok(mut l) = self.live.lock() else { return };
            let planes: Vec<dmabuf::Plane> = info
                .planes
                .iter()
                .take(info.plane_count.max(0) as usize)
                .map(|p| dmabuf::Plane {
                    fd: p.fd,
                    stride: p.stride,
                    offset: p.offset,
                })
                .collect();
            let frame = dmabuf::Frame {
                planes: &planes,
                modifier: info.modifier,
                width: info.extra.coded_size.width.max(0) as u32,
                height: info.extra.coded_size.height.max(0) as u32,
            };
            if frame.width == 0 || frame.height == 0 || l.failed.is_some() {
                return;
            }
            // Buffers of another size go once the compositor let them go.
            let shown = l.shown;
            let mut i = 0;
            while i < l.slots.len() {
                let s = &l.slots[i];
                let stale = s.target.width != frame.width || s.target.height != frame.height;
                if stale && !s.busy.load(Ordering::Acquire) && Some(i) != shown {
                    l.slots.remove(i);
                    if let Some(sh) = l.shown.as_mut()
                        && *sh > i
                    {
                        *sh -= 1;
                    }
                } else {
                    i += 1;
                }
            }
            // Not the one on screen: it is shown again when the page is.
            let free = l.slots.iter().enumerate().position(|(i, s)| {
                s.target.width == frame.width
                    && s.target.height == frame.height
                    && !s.busy.load(Ordering::Acquire)
                    && Some(i) != l.shown
            });
            let index = match free {
                Some(i) => i,
                None if l.slots.len() < BUFFERS + 2 => match self.new_slot(&mut l, &frame) {
                    Ok(slot) => {
                        l.slots.push(slot);
                        l.slots.len() - 1
                    }
                    Err(e) => {
                        eprintln!("dreamer: web page: {e}");
                        l.failed = Some(e);
                        return;
                    }
                },
                // Every buffer is still the compositor's: this frame is
                // skipped, and asked for again.
                None => {
                    if let Some(h) = &l.host {
                        h.invalidate(PaintElementType::VIEW);
                    }
                    return;
                }
            };
            let Some(copier) = l.copier.as_ref() else {
                return;
            };
            if let Err(e) = copier.copy(&frame, &l.slots[index].target) {
                eprintln!("dreamer: web page: {e}");
                return;
            }
            l.shown = Some(index);
            if l.visible {
                let rect = l.rect;
                let s = &l.slots[index];
                s.busy.store(true, Ordering::Release);
                self.show(&s.target, &s.buffer, &rect, true);
            }
            let _ = self.conn.flush();
        }

        /// A new buffer for frames like `frame`: the first GPU that copies
        /// one becomes the page's.
        fn new_slot(&self, l: &mut Live, frame: &dmabuf::Frame<'_>) -> Result<Slot, String> {
            if l.copier.is_none() {
                let mut why = Vec::new();
                for c in dmabuf::Copier::all()? {
                    match Self::make_slot(&self.dmabuf, &self.qh, &c, frame) {
                        Ok(slot) => match c.copy(frame, &slot.target) {
                            Ok(()) => {
                                l.copier = Some(c);
                                return Ok(slot);
                            }
                            Err(e) => why.push(format!("{}: {e}", c.name())),
                        },
                        Err(e) => why.push(format!("{}: {e}", c.name())),
                    }
                }
                return Err(format!(
                    "no GPU copies Chromium's frames ({})",
                    why.join("; ")
                ));
            }
            let c = l.copier.as_ref().ok_or("no GPU")?;
            Self::make_slot(&self.dmabuf, &self.qh, c, frame)
        }

        fn make_slot(
            dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            qh: &QueueHandle<State>,
            copier: &dmabuf::Copier,
            frame: &dmabuf::Frame<'_>,
        ) -> Result<Slot, String> {
            // Chromium allocated its frame with a modifier the compositor
            // takes; linear is the fallback every compositor takes.
            let mut wanted = vec![frame.modifier, LINEAR];
            wanted.dedup();
            wanted.retain(|m| *m != dmabuf::MOD_INVALID);
            let modifiers = copier.modifiers(&wanted);
            let target = copier.target(frame.width, frame.height, &modifiers)?;
            let params = dmabuf.create_params(qh, ());
            // SAFETY: the target's file is open while the target lives;
            // the request copies it.
            let fd = unsafe { BorrowedFd::borrow_raw(target.fd()) };
            params.add(
                fd,
                0,
                target.offset,
                target.stride,
                (target.modifier >> 32) as u32,
                target.modifier as u32,
            );
            let busy = Arc::new(AtomicBool::new(false));
            let buffer = params.create_immed(
                target.width as i32,
                target.height as i32,
                ARGB8888,
                zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                busy.clone(),
            );
            params.destroy();
            Ok(Slot {
                target,
                buffer,
                busy,
            })
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            if let Ok(mut l) = self.live.lock() {
                l.shown = None;
                l.slots.clear();
                l.host = None;
            }
            if let Some(v) = &self.viewport {
                v.destroy();
            }
            self.subsurface.destroy();
            self.surface.destroy();
            let _ = self.conn.flush();
        }
    }

    wrap_render_handler! {
        pub(crate) struct Paint {
            page: Arc<Page>,
        }
        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut cef::Rect>) {
                if let Some(r) = rect {
                    let (width, height, _) = self.page.view_size();
                    *r = cef::Rect { x: 0, y: 0, width, height };
                }
            }
            fn screen_info(
                &self,
                _browser: Option<&mut Browser>,
                screen_info: Option<&mut ScreenInfo>,
            ) -> i32 {
                let Some(s) = screen_info else { return 0 };
                let (width, height, scale) = self.page.view_size();
                s.device_scale_factor = scale;
                s.depth = 24;
                s.depth_per_component = 8;
                s.rect = cef::Rect { x: 0, y: 0, width, height };
                s.available_rect = s.rect.clone();
                1
            }
            fn on_accelerated_paint(
                &self,
                _browser: Option<&mut Browser>,
                type_: PaintElementType,
                _dirty_rects: Option<&[cef::Rect]>,
                info: Option<&AcceleratedPaintInfo>,
            ) {
                // Popups (a `<select>`'s list) are not drawn yet.
                if type_ == PaintElementType::VIEW
                    && let Some(info) = info
                {
                    self.page.paint(info);
                }
            }
        }
    }
}
