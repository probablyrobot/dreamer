//! macOS: CEF's application protocol on the toolkit's own `NSApplication`
//! subclass, the `NSView` calls that keep a browser's view on a pane, and
//! input sent as AppKit delivers it (for the desktop's smoke checks).
//!
//! All of it must run on the main thread.

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, Sel};
use objc2::{ffi, msg_send, sel};
use objc2_foundation::{NSPoint, NSRange, NSRect, NSSize, NSString};

/// A rectangle in a view's coordinates, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Whether the application is inside `sendEvent:`. A registered class
/// cannot gain instance variables, and there is one application object,
/// so the flag is process-wide.
static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

extern "C-unwind" fn is_handling_send_event(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling_send_event(_this: &AnyObject, _sel: Sel, value: Bool) {
    HANDLING_SEND_EVENT.store(value.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
    let was = HANDLING_SEND_EVENT.swap(true, Ordering::Relaxed);
    if let Some(superclass) = AnyClass::get(c"NSApplication") {
        // SAFETY: `this` is the application object, an instance of a
        // subclass of NSApplication, and `event` is the NSEvent AppKit
        // passed in.
        unsafe {
            let _: () = msg_send![super(this, superclass), sendEvent: event];
        }
    }
    HANDLING_SEND_EVENT.store(was, Ordering::Relaxed);
}

/// # Safety
///
/// `imp` must match `types` and the selector's calling convention.
unsafe fn add_method(class: &AnyClass, name: Sel, imp: Imp, types: &CStr) -> bool {
    // SAFETY: the class pointer is valid; adding a method to a registered
    // class is allowed at any time.
    unsafe {
        ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            name,
            imp,
            types.as_ptr(),
        )
        .as_bool()
    }
}

/// Make the running application object answer CEF's event-handling
/// protocol (`CrAppProtocol`, `CrAppControlProtocol`, `CefAppProtocol`):
/// `isHandlingSendEvent`, `setHandlingSendEvent:`, and a `sendEvent:` that
/// sets the flag around NSApplication's own. The methods are added to the
/// class of `NSApp`, which must be a subclass of NSApplication that does not
/// implement them itself (GPUI's `GPUIApplication`).
///
/// # Safety
///
/// Main thread only, after the CEF framework is loaded (it defines the
/// protocols) and after the application object exists, before
/// `cef::initialize`.
pub unsafe fn adopt_application_protocol() -> Result<(), String> {
    let base = AnyClass::get(c"NSApplication").ok_or("no NSApplication class")?;
    // SAFETY: main thread (caller); NSApp exists.
    let app: *mut AnyObject = unsafe { msg_send![base, sharedApplication] };
    if app.is_null() {
        return Err("no application object".into());
    }
    // SAFETY: `app` is a live object.
    let class = unsafe { (*app).class() };
    if std::ptr::eq(class, base) {
        return Err("the application object is a plain NSApplication".into());
    }
    if let Some(p) = AnyProtocol::get(c"CrAppControlProtocol") {
        // SAFETY: valid object and protocol.
        let already: Bool = unsafe { msg_send![app, conformsToProtocol: p] };
        if already.as_bool() {
            return Ok(());
        }
    }
    // SAFETY: each function matches its type encoding: BOOL(id, SEL),
    // void(id, SEL, BOOL), void(id, SEL, id).
    let added = unsafe {
        [
            add_method(
                class,
                sel!(isHandlingSendEvent),
                std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(
                    is_handling_send_event,
                ),
                c"B@:",
            ),
            add_method(
                class,
                sel!(setHandlingSendEvent:),
                std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel, Bool), Imp>(
                    set_handling_send_event,
                ),
                c"v@:B",
            ),
            add_method(
                class,
                sel!(sendEvent:),
                std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject), Imp>(
                    send_event,
                ),
                c"v@:@",
            ),
        ]
    };
    if added.contains(&false) {
        return Err(format!(
            "{} already implements part of CEF's application protocol",
            class.name().to_string_lossy()
        ));
    }
    for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
        let Some(p) = AnyProtocol::get(name) else {
            // CefAppProtocol is only declared by CEF's C++ wrapper headers;
            // Chromium checks the other two.
            if name == c"CefAppProtocol" {
                continue;
            }
            return Err(format!(
                "the CEF framework defines no {}",
                name.to_string_lossy()
            ));
        };
        // SAFETY: valid class and protocol pointers.
        unsafe {
            ffi::class_addProtocol(
                class as *const AnyClass as *mut AnyClass,
                p as *const AnyProtocol as *mut AnyProtocol,
            );
        }
    }
    Ok(())
}

/// Place `view` at `frame` in its superview's coordinates, with no
/// autoresizing: the pane's geometry comes from the layout, not AppKit.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn set_frame(view: *mut c_void, frame: Frame) {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return;
    }
    let rect = NSRect::new(
        NSPoint::new(frame.x, frame.y),
        NSSize::new(frame.width, frame.height),
    );
    // SAFETY: caller guarantees a live NSView on the main thread.
    unsafe {
        let _: () = msg_send![view, setAutoresizingMask: 0usize];
        let _: () = msg_send![view, setFrame: rect];
    }
}

/// The frame of `view` in its superview's coordinates.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn frame(view: *mut c_void) -> Option<Frame> {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a live NSView.
    let r: NSRect = unsafe { msg_send![view, frame] };
    Some(Frame {
        x: r.origin.x,
        y: r.origin.y,
        width: r.size.width,
        height: r.size.height,
    })
}

/// Show or hide `view` (and its subviews).
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn set_hidden(view: *mut c_void, hidden: bool) {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return;
    }
    // SAFETY: caller guarantees a live NSView.
    unsafe {
        let _: () = msg_send![view, setHidden: Bool::new(hidden)];
    }
}

/// Take `view` out of its superview (and its window).
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn remove_from_superview(view: *mut c_void) {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return;
    }
    // SAFETY: caller guarantees a live NSView.
    unsafe {
        let _: () = msg_send![view, removeFromSuperview];
    }
}

/// Make `view` its window's first responder: keys go to it.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn make_first_responder(view: *mut c_void) -> bool {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return false;
    }
    // SAFETY: caller guarantees a live NSView; `window` may be nil, and a
    // message to nil returns NO.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return false;
        }
        let ok: Bool = msg_send![window, makeFirstResponder: view];
        ok.as_bool()
    }
}

/// Whether the first responder of `view`'s window is `view` or a view
/// inside it: keys go there.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn has_first_responder(view: *mut c_void) -> bool {
    let view = view.cast::<AnyObject>();
    let Some(ns_view) = AnyClass::get(c"NSView") else {
        return false;
    };
    if view.is_null() {
        return false;
    }
    // SAFETY: caller guarantees a live NSView; the responder is checked to
    // be a view before `isDescendantOf:` is sent to it.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return false;
        }
        let responder: *mut AnyObject = msg_send![window, firstResponder];
        if responder.is_null() {
            return false;
        }
        let is_view: Bool = msg_send![responder, isKindOfClass: ns_view];
        if !is_view.as_bool() {
            return false;
        }
        let inside: Bool = msg_send![responder, isDescendantOf: view];
        inside.as_bool()
    }
}

/// The window server's number for the window holding `view`, which
/// `screencapture -l` takes.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn window_number(view: *mut c_void) -> Option<isize> {
    let view = view.cast::<AnyObject>();
    if view.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a live NSView.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return None;
        }
        let n: isize = msg_send![window, windowNumber];
        Some(n)
    }
}

/// `frame`, measured from the top left of `parent` as toolkits lay out, in
/// `parent`'s own coordinates: AppKit's origin is at the bottom left unless
/// the view is flipped.
///
/// # Safety
///
/// Main thread; `parent` is null or a live `NSView`.
pub unsafe fn from_top_left(parent: *mut c_void, frame: Frame) -> Frame {
    let parent = parent.cast::<AnyObject>();
    if parent.is_null() {
        return frame;
    }
    // SAFETY: caller guarantees a live NSView.
    let (flipped, bounds): (Bool, NSRect) =
        unsafe { (msg_send![parent, isFlipped], msg_send![parent, bounds]) };
    if flipped.as_bool() {
        return frame;
    }
    Frame {
        y: bounds.size.height - frame.y - frame.height,
        ..frame
    }
}

/// AppKit's event types and the Command modifier flag.
const KEY_DOWN: usize = 10;
const KEY_UP: usize = 11;
const LEFT_MOUSE_DOWN: usize = 1;
const LEFT_MOUSE_UP: usize = 2;
const COMMAND: usize = 1 << 20;

fn class(name: &CStr) -> Result<&'static AnyClass, String> {
    AnyClass::get(name).ok_or_else(|| format!("no {} class", name.to_string_lossy()))
}

/// The number of the window holding `view`, which must be the key window.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
unsafe fn key_window(view: *mut AnyObject) -> Result<isize, String> {
    if view.is_null() {
        return Err("no view".into());
    }
    // SAFETY: caller guarantees a live NSView; main thread.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return Err("the view is in no window".into());
        }
        let key: Bool = msg_send![window, isKeyWindow];
        if !key.as_bool() {
            return Err("the window is not the key window".into());
        }
        Ok(msg_send![window, windowNumber])
    }
}

/// Deliver `event` as AppKit delivers a person's: `NSApp sendEvent:`.
///
/// # Safety
///
/// Main thread; `event` is null or a live `NSEvent`.
unsafe fn send(event: *mut AnyObject) -> Result<(), String> {
    if event.is_null() {
        return Err("AppKit made no event".into());
    }
    // SAFETY: the application object exists in a running app; main thread.
    unsafe {
        let app: *mut AnyObject = msg_send![class(c"NSApplication")?, sharedApplication];
        let _: () = msg_send![app, sendEvent: event];
    }
    Ok(())
}

/// Queue `event` behind the events already waiting, as a person's arrives:
/// `NSApp postEvent:atStart:NO`. The application's event loop delivers it.
///
/// # Safety
///
/// Main thread; `event` is null or a live `NSEvent`.
unsafe fn post(event: *mut AnyObject) -> Result<(), String> {
    if event.is_null() {
        return Err("AppKit made no event".into());
    }
    // SAFETY: the application object exists in a running app; main thread.
    unsafe {
        let app: *mut AnyObject = msg_send![class(c"NSApplication")?, sharedApplication];
        let _: () = msg_send![app, postEvent: event, atStart: Bool::NO];
    }
    Ok(())
}

/// Seconds since the system started, as event timestamps count.
fn uptime() -> Result<f64, String> {
    // SAFETY: NSProcessInfo's documented calls.
    unsafe {
        let info: *mut AnyObject = msg_send![class(c"NSProcessInfo")?, processInfo];
        Ok(msg_send![info, systemUptime])
    }
}

/// Press and release a key in the window holding `view` (the key window),
/// queued on the application's events as a typed key is (`NSApp
/// postEvent:atStart:`): the event loop delivers it later, through
/// `sendEvent:`, so key equivalents (the window's views, then the menu
/// bar) see it before the first responder, and a toolkit handling it is
/// not already inside a call of its own. `key_code` is the virtual key
/// code, `chars` what it types.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn press_key(
    view: *mut c_void,
    key_code: u16,
    chars: &str,
    command: bool,
) -> Result<(), String> {
    let chars = NSString::from_str(chars);
    let flags = if command { COMMAND } else { 0 };
    // SAFETY: live view (caller), main thread; the arguments match
    // keyEventWithType:location:modifierFlags:timestamp:windowNumber:
    // context:characters:charactersIgnoringModifiers:isARepeat:keyCode:.
    unsafe {
        let number = key_window(view.cast())?;
        let events = class(c"NSEvent")?;
        for kind in [KEY_DOWN, KEY_UP] {
            objc2::rc::autoreleasepool(|_| {
                let event: *mut AnyObject = msg_send![
                    events,
                    keyEventWithType: kind,
                    location: NSPoint::new(0.0, 0.0),
                    modifierFlags: flags,
                    timestamp: uptime()?,
                    windowNumber: number,
                    context: std::ptr::null_mut::<AnyObject>(),
                    characters: &*chars,
                    charactersIgnoringModifiers: &*chars,
                    isARepeat: Bool::NO,
                    keyCode: key_code,
                ];
                post(event)
            })?;
        }
    }
    Ok(())
}

/// Click (press and release the left button) at (`x`, `y`) in `view`,
/// measured from its top left, through `NSApp sendEvent:`: the window
/// gives it to whichever view is under the point.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn click(view: *mut c_void, x: f64, y: f64) -> Result<(), String> {
    let view = view.cast::<AnyObject>();
    // SAFETY: live view (caller), main thread; the arguments match
    // mouseEventWithType:location:modifierFlags:timestamp:windowNumber:
    // context:eventNumber:clickCount:pressure:.
    unsafe {
        let number = key_window(view)?;
        let (flipped, bounds): (Bool, NSRect) =
            (msg_send![view, isFlipped], msg_send![view, bounds]);
        let local = if flipped.as_bool() {
            NSPoint::new(x, y)
        } else {
            NSPoint::new(x, bounds.size.height - y)
        };
        let at: NSPoint =
            msg_send![view, convertPoint: local, toView: std::ptr::null_mut::<AnyObject>()];
        let events = class(c"NSEvent")?;
        for kind in [LEFT_MOUSE_DOWN, LEFT_MOUSE_UP] {
            objc2::rc::autoreleasepool(|_| {
                let event: *mut AnyObject = msg_send![
                    events,
                    mouseEventWithType: kind,
                    location: at,
                    modifierFlags: 0usize,
                    timestamp: uptime()?,
                    windowNumber: number,
                    context: std::ptr::null_mut::<AnyObject>(),
                    eventNumber: 0isize,
                    clickCount: 1isize,
                    pressure: 1.0f32,
                ];
                send(event)
            })?;
        }
    }
    Ok(())
}

/// An input method's composition in the first responder of `view`'s
/// window, which must be `view` or inside it (`NSTextInputClient`):
/// `marked` shown as marked text, then `committed` inserted in its place.
///
/// # Safety
///
/// Main thread; `view` is null or a live `NSView`.
pub unsafe fn compose(view: *mut c_void, marked: &str, committed: &str) -> Result<(), String> {
    // SAFETY: live view or null (caller); main thread.
    if !unsafe { has_first_responder(view) } {
        return Err("the view does not have keyboard focus".into());
    }
    let none = NSRange::new(isize::MAX as usize, 0);
    let (marked, committed) = (NSString::from_str(marked), NSString::from_str(committed));
    // SAFETY: the first responder is a live view inside `view` (checked
    // above), asked whether it takes marked text before it is given any.
    unsafe {
        let window: *mut AnyObject = msg_send![view.cast::<AnyObject>(), window];
        let input: *mut AnyObject = msg_send![window, firstResponder];
        let client: Bool = msg_send![
            input,
            respondsToSelector: sel!(setMarkedText:selectedRange:replacementRange:)
        ];
        if !client.as_bool() {
            return Err("the focused view takes no input method text".into());
        }
        let _: () = msg_send![
            input,
            setMarkedText: &*marked,
            selectedRange: NSRange::new(marked.length(), 0),
            replacementRange: none
        ];
        let _: () = msg_send![input, insertText: &*committed, replacementRange: none];
    }
    Ok(())
}
