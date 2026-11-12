//! Link-level smoke test: the pinned library loads, reports the pinned
//! version, and turns VT bytes into screen text.

#![allow(unsafe_code)]

use std::ffi::CStr;
use std::ptr;

use ghostty_sys::GhosttyResult::GHOSTTY_SUCCESS;
use ghostty_sys::*;

#[test]
fn runtime_version_matches_the_pin() {
    // SAFETY: returns a static NUL-terminated string.
    let version = unsafe { CStr::from_ptr(ghostty_version()) }
        .to_str()
        .expect("utf-8 version");
    assert!(
        version.starts_with(LINKED_VERSION),
        "linked {version}, pinned {LINKED_VERSION}"
    );
}

#[test]
fn feeds_bytes_and_reads_back_screen_text() {
    let mut term: GhosttyTerminal = ptr::null_mut();
    // SAFETY: valid out-pointer; NULL allocator selects the default.
    let rc = unsafe { ghostty_terminal_new(ptr::null(), &mut term, 20, 4) };
    assert_eq!(rc, GHOSTTY_SUCCESS);
    assert!(!term.is_null());

    let input = "hi \x1b[1;32mthere\x1b[0m\r\n\u{1F600}\u{4E16}!";
    // SAFETY: `term` is live; the slice is valid for its length.
    unsafe { ghostty_terminal_vt_write(term, input.as_ptr(), input.len()) };

    let mut fmt: GhosttyFormatter = ptr::null_mut();
    // SAFETY: `term` outlives `fmt`; both are freed below in reverse order.
    let rc = unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut fmt, term) };
    assert_eq!(rc, GHOSTTY_SUCCESS);

    let mut out: *mut u8 = ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: valid out-pointers; the buffer is freed with the same allocator.
    let rc = unsafe { ghostty_formatter_format_alloc(fmt, ptr::null(), &mut out, &mut len) };
    assert_eq!(rc, GHOSTTY_SUCCESS);
    let text = if out.is_null() {
        String::new()
    } else {
        // SAFETY: the library returned `len` initialized bytes at `out`.
        let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
        unsafe { ghostty_free(ptr::null(), out, len) };
        String::from_utf8(bytes).expect("plain output is UTF-8")
    };

    // SAFETY: handles are live and freed exactly once.
    unsafe {
        ghostty_formatter_free(fmt);
        ghostty_terminal_free(term);
    }

    assert_eq!(text.trim_end(), "hi there\n\u{1F600}\u{4E16}!");
}
