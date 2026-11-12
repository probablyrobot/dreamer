//! Input encoding against the engine's current state (spec 10.1): keys,
//! focus, and paste are encoded with the modes the application has set in
//! this terminal (cursor-key and keypad modes, Kitty keyboard flags,
//! modifyOtherKeys, focus reporting, bracketed paste).

use std::ptr;

use ghostty_sys::GhosttyFocusEvent::{GHOSTTY_FOCUS_GAINED, GHOSTTY_FOCUS_LOST};
use ghostty_sys::GhosttyKey::{self, Type as KeyCode};
use ghostty_sys::GhosttyKeyAction::{
    GHOSTTY_KEY_ACTION_PRESS, GHOSTTY_KEY_ACTION_RELEASE, GHOSTTY_KEY_ACTION_REPEAT,
};
use ghostty_sys::GhosttyResult::GHOSTTY_OUT_OF_SPACE;
use ghostty_sys::*;

use crate::{Engine, EngineError, check};

/// Modifier bits (`GHOSTTY_MODS_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u16);

impl Mods {
    pub const SHIFT: Mods = Mods(GHOSTTY_MODS_SHIFT as u16);
    pub const CTRL: Mods = Mods(GHOSTTY_MODS_CTRL as u16);
    pub const ALT: Mods = Mods(GHOSTTY_MODS_ALT as u16);
    pub const SUPER: Mods = Mods(GHOSTTY_MODS_SUPER as u16);

    pub fn with(self, other: Mods) -> Mods {
        Mods(self.0 | other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Mouse buttons; wheel up and down are buttons 4 and 5, as in X11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// A physical key by its W3C `KeyboardEvent.code` name (`KeyA`, `Digit1`,
/// `ArrowUp`, `Enter`, `F5`, ...).
pub fn key_code(name: &str) -> Option<KeyCode> {
    use GhosttyKey::*;
    if let Some(c) = name.strip_prefix("Key").filter(|c| c.len() == 1) {
        let i = c.as_bytes()[0].checked_sub(b'A').filter(|i| *i < 26)?;
        return Some(GHOSTTY_KEY_A + KeyCode::from(i));
    }
    if let Some(d) = name.strip_prefix("Digit").filter(|d| d.len() == 1) {
        let i = d.as_bytes()[0].checked_sub(b'0').filter(|i| *i < 10)?;
        return Some(GHOSTTY_KEY_DIGIT_0 + KeyCode::from(i));
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=25).contains(&n).then(|| GHOSTTY_KEY_F1 + (n - 1));
    }
    Some(match name {
        "Enter" => GHOSTTY_KEY_ENTER,
        "Tab" => GHOSTTY_KEY_TAB,
        "Backspace" => GHOSTTY_KEY_BACKSPACE,
        "Escape" => GHOSTTY_KEY_ESCAPE,
        "Space" => GHOSTTY_KEY_SPACE,
        "ArrowUp" => GHOSTTY_KEY_ARROW_UP,
        "ArrowDown" => GHOSTTY_KEY_ARROW_DOWN,
        "ArrowLeft" => GHOSTTY_KEY_ARROW_LEFT,
        "ArrowRight" => GHOSTTY_KEY_ARROW_RIGHT,
        "Home" => GHOSTTY_KEY_HOME,
        "End" => GHOSTTY_KEY_END,
        "PageUp" => GHOSTTY_KEY_PAGE_UP,
        "PageDown" => GHOSTTY_KEY_PAGE_DOWN,
        "Insert" => GHOSTTY_KEY_INSERT,
        "Delete" => GHOSTTY_KEY_DELETE,
        "Minus" => GHOSTTY_KEY_MINUS,
        "Equal" => GHOSTTY_KEY_EQUAL,
        "BracketLeft" => GHOSTTY_KEY_BRACKET_LEFT,
        "BracketRight" => GHOSTTY_KEY_BRACKET_RIGHT,
        "Backslash" => GHOSTTY_KEY_BACKSLASH,
        "Semicolon" => GHOSTTY_KEY_SEMICOLON,
        "Quote" => GHOSTTY_KEY_QUOTE,
        "Comma" => GHOSTTY_KEY_COMMA,
        "Period" => GHOSTTY_KEY_PERIOD,
        "Slash" => GHOSTTY_KEY_SLASH,
        "Backquote" => GHOSTTY_KEY_BACKQUOTE,
        _ => return None,
    })
}

/// Run an encoder that writes into a caller buffer, growing it on
/// `OUT_OF_SPACE`.
fn encode_into(
    call: &'static str,
    mut f: impl FnMut(*mut u8, usize, &mut usize) -> GhosttyResult::Type,
) -> Result<Vec<u8>, EngineError> {
    let mut cap = 64usize;
    loop {
        let mut buf = vec![0u8; cap];
        let mut written = 0usize;
        let rc = f(buf.as_mut_ptr(), buf.len(), &mut written);
        if rc == GHOSTTY_OUT_OF_SPACE && cap < (64 << 20) {
            cap = cap.saturating_mul(4).max(written);
            continue;
        }
        check(call, rc)?;
        buf.truncate(written);
        return Ok(buf);
    }
}

impl Engine {
    /// Bytes for a key event, encoded with the terminal's current keyboard
    /// modes. `text` is the text the key produced, if any. Empty when the
    /// event produces nothing (for example a release without Kitty flags).
    pub fn encode_key(
        &self,
        key: KeyCode,
        mods: Mods,
        text: Option<&str>,
        action: KeyAction,
    ) -> Result<Vec<u8>, EngineError> {
        let mut encoder: GhosttyKeyEncoder = ptr::null_mut();
        let mut event: GhosttyKeyEvent = ptr::null_mut();
        // SAFETY: encoder and event are created here, used only while `term`
        // is live, and freed before returning on every path; `text` outlives
        // the encode call that reads it.
        unsafe {
            check(
                "ghostty_key_encoder_new",
                ghostty_key_encoder_new(ptr::null(), &mut encoder),
            )?;
            let result = (|| {
                check(
                    "ghostty_key_event_new",
                    ghostty_key_event_new(ptr::null(), &mut event),
                )?;
                check(
                    "ghostty_key_encoder_set_from_terminal",
                    ghostty_key_encoder_set_from_terminal(encoder, self.term),
                )?;
                let action = match action {
                    KeyAction::Press => GHOSTTY_KEY_ACTION_PRESS,
                    KeyAction::Repeat => GHOSTTY_KEY_ACTION_REPEAT,
                    KeyAction::Release => GHOSTTY_KEY_ACTION_RELEASE,
                };
                check(
                    "ghostty_key_event_set_action",
                    ghostty_key_event_set_action(event, action),
                )?;
                check(
                    "ghostty_key_event_set_key",
                    ghostty_key_event_set_key(event, key),
                )?;
                check(
                    "ghostty_key_event_set_mods",
                    ghostty_key_event_set_mods(event, mods.0),
                )?;
                if let Some(t) = text {
                    check(
                        "ghostty_key_event_set_utf8",
                        ghostty_key_event_set_utf8(
                            event,
                            GhosttyString {
                                ptr: t.as_ptr(),
                                len: t.len(),
                            },
                        ),
                    )?;
                }
                encode_into("ghostty_key_encoder_encode", |buf, len, out| {
                    ghostty_key_encoder_encode(encoder, event, buf, len, out)
                })
            })();
            if !event.is_null() {
                ghostty_key_event_free(event);
            }
            ghostty_key_encoder_free(encoder);
            result
        }
    }

    /// A mouse event at a cell, encoded with the terminal's mouse tracking
    /// mode and format. Empty when the application does not track the mouse
    /// (or not this kind of event). Positions are canonical cells; the
    /// encoder sees the centre of the cell in canonical pixels.
    pub fn encode_mouse(
        &self,
        action: MouseAction,
        button: Option<MouseButton>,
        col: u16,
        row: u16,
        mods: Mods,
        any_button_pressed: bool,
    ) -> Result<Vec<u8>, EngineError> {
        use ghostty_sys::GhosttyMouseAction::*;
        use ghostty_sys::GhosttyMouseButton::*;
        use ghostty_sys::GhosttyMouseEncoderOption::*;
        let (cw, ch) = terminal_core::family::CANONICAL_CELL_PX;
        let grid = self.size();
        let size = GhosttyMouseEncoderSize {
            size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
            screen_width: u32::from(grid.cols) * cw,
            screen_height: u32::from(grid.rows) * ch,
            cell_width: cw,
            cell_height: ch,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let position = GhosttyMousePosition {
            x: (f32::from(col.min(grid.cols.saturating_sub(1))) + 0.5) * cw as f32,
            y: (f32::from(row.min(grid.rows.saturating_sub(1))) + 0.5) * ch as f32,
        };
        let mut encoder: GhosttyMouseEncoder = ptr::null_mut();
        let mut event: GhosttyMouseEvent = ptr::null_mut();
        // SAFETY: encoder and event are created here, used while `term` is
        // live, and freed on every path; option values point at locals that
        // outlive the calls reading them.
        unsafe {
            check(
                "ghostty_mouse_encoder_new",
                ghostty_mouse_encoder_new(ptr::null(), &mut encoder),
            )?;
            let result = (|| {
                check(
                    "ghostty_mouse_event_new",
                    ghostty_mouse_event_new(ptr::null(), &mut event),
                )?;
                check(
                    "ghostty_mouse_encoder_set_from_terminal",
                    ghostty_mouse_encoder_set_from_terminal(encoder, self.term),
                )?;
                check(
                    "ghostty_mouse_encoder_set(size)",
                    ghostty_mouse_encoder_set(
                        encoder,
                        GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                        (&raw const size).cast(),
                    ),
                )?;
                check(
                    "ghostty_mouse_encoder_set(any_button_pressed)",
                    ghostty_mouse_encoder_set(
                        encoder,
                        GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                        (&raw const any_button_pressed).cast(),
                    ),
                )?;
                let action = match action {
                    MouseAction::Press => GHOSTTY_MOUSE_ACTION_PRESS,
                    MouseAction::Release => GHOSTTY_MOUSE_ACTION_RELEASE,
                    MouseAction::Motion => GHOSTTY_MOUSE_ACTION_MOTION,
                };
                check(
                    "ghostty_mouse_event_set_action",
                    ghostty_mouse_event_set_action(event, action),
                )?;
                match button {
                    Some(b) => {
                        let b = match b {
                            MouseButton::Left => GHOSTTY_MOUSE_BUTTON_LEFT,
                            MouseButton::Middle => GHOSTTY_MOUSE_BUTTON_MIDDLE,
                            MouseButton::Right => GHOSTTY_MOUSE_BUTTON_RIGHT,
                            MouseButton::WheelUp => GHOSTTY_MOUSE_BUTTON_FOUR,
                            MouseButton::WheelDown => GHOSTTY_MOUSE_BUTTON_FIVE,
                        };
                        check(
                            "ghostty_mouse_event_set_button",
                            ghostty_mouse_event_set_button(event, b),
                        )?;
                    }
                    None => check(
                        "ghostty_mouse_event_clear_button",
                        ghostty_mouse_event_clear_button(event),
                    )?,
                }
                check(
                    "ghostty_mouse_event_set_mods",
                    ghostty_mouse_event_set_mods(event, mods.0),
                )?;
                check(
                    "ghostty_mouse_event_set_position",
                    ghostty_mouse_event_set_position(event, position),
                )?;
                encode_into("ghostty_mouse_encoder_encode", |buf, len, out| {
                    ghostty_mouse_encoder_encode(encoder, event, buf, len, out)
                })
            })();
            if !event.is_null() {
                ghostty_mouse_event_free(event);
            }
            ghostty_mouse_encoder_free(encoder);
            result
        }
    }

    /// Focus in or out, if the application asked for focus reports (mode
    /// 1004); otherwise nothing.
    pub fn encode_focus(&self, gained: bool) -> Result<Vec<u8>, EngineError> {
        if !self.mode(1004, false)? {
            return Ok(Vec::new());
        }
        let event = if gained {
            GHOSTTY_FOCUS_GAINED
        } else {
            GHOSTTY_FOCUS_LOST
        };
        encode_into("ghostty_focus_encode", |buf, len, out| {
            // SAFETY: writes at most `len` bytes into `buf`.
            unsafe { ghostty_focus_encode(event, buf, len, out) }
        })
    }

    /// A paste, bracketed when the application enabled bracketed paste
    /// (mode 2004). Ghostty neutralizes control sequences in the pasted
    /// text, so it cannot end the bracket early.
    pub fn encode_paste(&self, text: &[u8]) -> Result<Vec<u8>, EngineError> {
        let bracketed = self.mode(2004, false)?;
        // The encoder may rewrite the input in place; give it a copy.
        let mut data = text.to_vec();
        encode_into("ghostty_paste_encode", |buf, len, out| {
            // SAFETY: `data` is a live, writable buffer of `data.len()` bytes;
            // at most `len` bytes are written into `buf`.
            unsafe {
                ghostty_paste_encode(
                    data.as_mut_ptr().cast(),
                    data.len(),
                    bracketed,
                    buf,
                    len,
                    out,
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineConfig, core::GridSize};

    fn engine() -> Engine {
        Engine::new(EngineConfig::new(GridSize { cols: 80, rows: 24 })).expect("engine")
    }

    fn key(e: &Engine, name: &str, mods: Mods, text: Option<&str>) -> Vec<u8> {
        e.encode_key(key_code(name).expect(name), mods, text, KeyAction::Press)
            .expect("encode")
    }

    #[test]
    fn keys_follow_the_terminal_modes() {
        let mut e = engine();
        assert_eq!(key(&e, "ArrowUp", Mods::default(), None), b"\x1b[A");
        // The application switches to cursor-key application mode.
        e.feed(b"\x1b[?1h").expect("feed");
        assert_eq!(key(&e, "ArrowUp", Mods::default(), None), b"\x1bOA");
        assert_eq!(key(&e, "KeyA", Mods::default(), Some("a")), b"a");
        assert_eq!(key(&e, "KeyC", Mods::CTRL, None), b"\x03");
        assert_eq!(key(&e, "Enter", Mods::default(), None), b"\r");
        assert_eq!(key(&e, "Backspace", Mods::default(), None), b"\x7f");
        assert_eq!(key(&e, "ArrowLeft", Mods::SHIFT, None), b"\x1b[1;2D");
        assert!(
            key_code("KeyAA").is_none() && key_code("F26").is_none() && key_code("Nope").is_none()
        );
    }

    #[test]
    fn mouse_follows_the_tracking_mode_and_format() {
        let mut e = engine();
        let click = |e: &Engine, a| {
            e.encode_mouse(a, Some(MouseButton::Left), 4, 2, Mods::default(), false)
                .expect("mouse")
        };
        assert!(
            click(&e, MouseAction::Press).is_empty(),
            "no tracking requested"
        );
        // Normal tracking with SGR encoding: 1-based column and row.
        e.feed(b"\x1b[?1000h\x1b[?1006h").expect("feed");
        assert_eq!(click(&e, MouseAction::Press), b"\x1b[<0;5;3M");
        assert_eq!(click(&e, MouseAction::Release), b"\x1b[<0;5;3m");
        let wheel = e
            .encode_mouse(
                MouseAction::Press,
                Some(MouseButton::WheelUp),
                0,
                0,
                Mods::default(),
                false,
            )
            .expect("wheel");
        assert_eq!(wheel, b"\x1b[<64;1;1M");
        // Motion is reported only in button-event mode while a button is held.
        let drag = |e: &Engine| {
            e.encode_mouse(
                MouseAction::Motion,
                Some(MouseButton::Left),
                6,
                2,
                Mods::default(),
                true,
            )
            .expect("drag")
        };
        assert!(drag(&e).is_empty());
        e.feed(b"\x1b[?1002h").expect("feed");
        assert_eq!(drag(&e), b"\x1b[<32;7;3M");
    }

    #[test]
    fn focus_reports_only_when_asked() {
        let mut e = engine();
        assert!(e.encode_focus(true).expect("focus").is_empty());
        e.feed(b"\x1b[?1004h").expect("feed");
        assert_eq!(e.encode_focus(true).expect("focus"), b"\x1b[I");
        assert_eq!(e.encode_focus(false).expect("focus"), b"\x1b[O");
    }

    #[test]
    fn paste_is_bracketed_once_and_cannot_close_its_own_bracket() {
        let mut e = engine();
        assert_eq!(e.encode_paste(b"ls\nfoo").expect("paste"), b"ls\rfoo");
        e.feed(b"\x1b[?2004h").expect("feed");
        let out = e.encode_paste(b"evil\x1b[201~rm -rf ~\n").expect("paste");
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"),
            "{s:?}"
        );
        assert_eq!(s.matches("\x1b[201~").count(), 1, "{s:?}");
        let big = vec![b'x'; 300_000];
        let out = e.encode_paste(&big).expect("big paste");
        assert_eq!(out.len(), big.len() + 12);
    }
}
