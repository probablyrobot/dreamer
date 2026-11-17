//! The clipboard as the desktop uses it: the system clipboard, and on
//! Linux the primary selection. A smoke run swaps both for an in-memory
//! store, so no check ever reads or replaces what the user copied.

use std::sync::Mutex;

use gpui::{App, ClipboardItem};

/// The in-memory clipboard of a smoke run: what is on it, what is in the
/// primary selection (Linux), and every clipboard write in order.
#[derive(Debug, Default, Clone)]
pub(crate) struct Memory {
    pub clipboard: Option<String>,
    pub primary: Option<String>,
    pub writes: Vec<String>,
}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// Use an in-memory clipboard from here on (smoke runs only).
pub(crate) fn use_memory() {
    if let Ok(mut m) = MEMORY.lock()
        && m.is_none()
    {
        *m = Some(Memory::default());
    }
}

/// The in-memory clipboard, if this run uses one.
pub(crate) fn memory() -> Option<Memory> {
    MEMORY.lock().ok().and_then(|m| m.clone())
}

/// Put text on the in-memory clipboard as if another program had copied
/// it (smoke runs only).
pub(crate) fn set_memory(text: &str) {
    if let Ok(mut m) = MEMORY.lock()
        && let Some(m) = m.as_mut()
    {
        m.clipboard = Some(text.to_owned());
    }
}

/// Replace the clipboard's contents with `text`.
pub(crate) fn write(cx: &mut App, text: String) {
    if let Ok(mut m) = MEMORY.lock()
        && let Some(m) = m.as_mut()
    {
        m.writes.push(text.clone());
        m.clipboard = Some(text);
        return;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// The clipboard's text, if it holds any.
pub(crate) fn read(cx: &mut App) -> Option<String> {
    if let Ok(m) = MEMORY.lock()
        && let Some(m) = m.as_ref()
    {
        return m.clipboard.clone();
    }
    cx.read_from_clipboard().and_then(|c| c.text())
}

/// Replace the primary selection (Linux).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn write_primary(cx: &mut App, text: String) {
    if let Ok(mut m) = MEMORY.lock()
        && let Some(m) = m.as_mut()
    {
        m.primary = Some(text);
        return;
    }
    cx.write_to_primary(ClipboardItem::new_string(text));
}

/// Whether this run uses the in-memory clipboard (a smoke run, whose
/// window may not be the active one).
pub(crate) fn in_memory() -> bool {
    MEMORY.lock().is_ok_and(|m| m.is_some())
}
