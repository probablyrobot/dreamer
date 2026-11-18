//! macOS: CEF's application protocol on the toolkit's own `NSApplication`
//! subclass, and the `NSView` calls that keep a browser's view on a pane.
//!
//! All of it must run on the main thread.

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, Sel};
use objc2::{ffi, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};

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
