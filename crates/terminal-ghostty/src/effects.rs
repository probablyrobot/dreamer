//! Ghostty effect callbacks, copied into owned [`Effects`].
//!
//! Callbacks run synchronously inside `ghostty_terminal_vt_write`. They only
//! record what happened in the engine's boxed [`CallbackState`]; they never
//! write to the terminal, block, or let a panic unwind into C.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ghostty_sys::GhosttyTerminalOption::*;
use ghostty_sys::*;
use terminal_core::{Effects, ReplySource};

use crate::{EngineError, check};

#[derive(Debug)]
pub(crate) struct CallbackState {
    capture_replies: bool,
    pending: Effects,
    pub(crate) title_changed: bool,
    pub(crate) pwd_changed: bool,
    /// Current synchronized-output state (mode 2026).
    pub(crate) held: bool,
    pub(crate) panicked: bool,
}

impl CallbackState {
    pub(crate) fn new(capture_replies: bool) -> Self {
        Self {
            capture_replies,
            pending: Effects::default(),
            title_changed: false,
            pwd_changed: false,
            held: false,
            panicked: false,
        }
    }

    pub(crate) fn take(&mut self) -> Effects {
        std::mem::take(&mut self.pending)
    }
}

/// Register the callbacks this engine observes. Effects without a callback
/// (clipboard, notifications, size and device queries, ...) stay ignored by
/// the library until a policy for them exists.
pub(crate) fn register(
    term: GhosttyTerminal,
    state: &mut Box<CallbackState>,
) -> Result<(), EngineError> {
    let userdata: *mut CallbackState = &mut **state;
    let bell: GhosttyTerminalBellFn = Some(on_bell);
    let title: GhosttyTerminalTitleChangedFn = Some(on_title_changed);
    let pwd: GhosttyTerminalPwdChangedFn = Some(on_pwd_changed);
    let hold: GhosttyTerminalRenderHoldFn = Some(on_render_hold);
    // SAFETY: `term` is live. USERDATA takes the pointer value itself; it
    // points into a Box owned by the engine that outlives the terminal.
    // Callback options take the function pointer as the value.
    unsafe {
        check(
            "set userdata",
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, userdata.cast()),
        )?;
        check(
            "set bell",
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_BELL, fn_ptr(bell)),
        )?;
        check(
            "set title_changed",
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, fn_ptr(title)),
        )?;
        check(
            "set pwd_changed",
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_PWD_CHANGED, fn_ptr(pwd)),
        )?;
        check(
            "set render_hold",
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, fn_ptr(hold)),
        )?;
        if state.capture_replies {
            let write: GhosttyTerminalWritePtyFn = Some(on_write_pty);
            check(
                "set write_pty",
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, fn_ptr(write)),
            )?;
        }
    }
    Ok(())
}

fn fn_ptr<F: Copy>(f: Option<F>) -> *const c_void {
    debug_assert_eq!(size_of::<Option<F>>(), size_of::<*const c_void>());
    // SAFETY: `Option<extern "C" fn>` has the layout of a nullable pointer.
    unsafe { std::mem::transmute_copy::<Option<F>, *const c_void>(&f) }
}

/// Run `f` on the callback state, converting a panic into a flag so it never
/// unwinds through C.
unsafe fn with_state(userdata: *mut c_void, f: impl FnOnce(&mut CallbackState)) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: userdata is the engine's boxed CallbackState, alive for the
    // terminal's lifetime and not otherwise borrowed during a vt_write.
    let state = unsafe { &mut *userdata.cast::<CallbackState>() };
    if catch_unwind(AssertUnwindSafe(|| f(&mut *state))).is_err() {
        state.panicked = true;
    }
}

unsafe extern "C" fn on_bell(_term: GhosttyTerminal, userdata: *mut c_void) {
    unsafe {
        with_state(userdata, |s| {
            s.pending.bells = s.pending.bells.saturating_add(1)
        })
    }
}

unsafe extern "C" fn on_title_changed(_term: GhosttyTerminal, userdata: *mut c_void) {
    unsafe { with_state(userdata, |s| s.title_changed = true) }
}

unsafe extern "C" fn on_pwd_changed(_term: GhosttyTerminal, userdata: *mut c_void) {
    unsafe { with_state(userdata, |s| s.pwd_changed = true) }
}

unsafe extern "C" fn on_render_hold(_term: GhosttyTerminal, userdata: *mut c_void, held: bool) {
    unsafe {
        with_state(userdata, |s| {
            s.held = held;
            s.pending.render_hold.push(held);
        })
    }
}

unsafe extern "C" fn on_write_pty(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    unsafe {
        with_state(userdata, |s| {
            if data.is_null() || len == 0 {
                return;
            }
            // SAFETY: borrowed for the duration of the callback; copied now.
            let bytes = std::slice::from_raw_parts(data, len).to_vec();
            match s.pending.replies.last_mut() {
                Some((ReplySource::Protocol, buf)) => buf.extend_from_slice(&bytes),
                None => s.pending.replies.push((ReplySource::Protocol, bytes)),
            }
        })
    }
}
