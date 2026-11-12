//! Keyboard, text, IME, and paste from the window to the pane (spec 10.1).
//!
//! One policy for every key press:
//! 1. Paste shortcuts (Cmd-V on macOS, Ctrl-Shift-V elsewhere) paste the
//!    clipboard.
//! 2. Other Cmd (macOS) shortcuts belong to the application and are left
//!    alone.
//! 3. Special keys (arrows, Enter, F-keys, ...) and anything with Ctrl or
//!    Alt are sent as key events, encoded by the daemon against the pane's
//!    current modes.
//! 4. Everything else is text. It reaches the platform's text input, so
//!    IME composition works: preedit is only drawn, and committed text is
//!    sent.

use api::terminal_input::Event;
use gpui::{Keystroke, Modifiers};

/// W3C `KeyboardEvent.code` for a GPUI key name, when it has one.
pub fn code_for(key: &str) -> Option<String> {
    let special = match key {
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "escape" => "Escape",
        "space" => "Space",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "insert" => "Insert",
        "delete" => "Delete",
        "-" => "Minus",
        "=" => "Equal",
        "[" => "BracketLeft",
        "]" => "BracketRight",
        "\\" => "Backslash",
        ";" => "Semicolon",
        "'" => "Quote",
        "," => "Comma",
        "." => "Period",
        "/" => "Slash",
        "`" => "Backquote",
        _ => "",
    };
    if !special.is_empty() {
        return Some(special.to_owned());
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=25).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => {
            Some(format!("Key{}", c.to_ascii_uppercase()))
        }
        (Some(c), None) if c.is_ascii_digit() => Some(format!("Digit{c}")),
        _ => None,
    }
}

fn is_special(key: &str) -> bool {
    matches!(
        key,
        "enter"
            | "tab"
            | "backspace"
            | "escape"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "insert"
            | "delete"
    ) || (key.starts_with('f') && key.len() > 1 && key[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn mods_bits(m: &Modifiers) -> u32 {
    u32::from(m.shift)
        | (u32::from(m.control) << 1)
        | (u32::from(m.alt) << 2)
        | (u32::from(m.platform) << 3)
}

/// What a key press means for the terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyIntent {
    /// Paste the clipboard.
    Paste,
    /// Send this event.
    Send(Event),
    /// Not ours: an application shortcut, or text the platform text input
    /// delivers (so IME keeps working).
    Pass,
}

pub fn intent(k: &Keystroke, held: bool) -> KeyIntent {
    let m = &k.modifiers;
    let paste = if cfg!(target_os = "macos") {
        m.platform && !m.control && !m.alt && k.key == "v"
    } else {
        m.control && m.shift && !m.alt && k.key == "v"
    };
    if paste {
        return KeyIntent::Paste;
    }
    if cfg!(target_os = "macos") && m.platform {
        return KeyIntent::Pass;
    }
    if !(is_special(&k.key) || m.control || m.alt) {
        return KeyIntent::Pass;
    }
    match code_for(&k.key) {
        Some(code) => KeyIntent::Send(Event::Key(api::KeyInput {
            key: code,
            text: k.key_char.clone().filter(|_| !m.control),
            mods: mods_bits(m),
            held,
        })),
        None => KeyIntent::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &'static str) -> Keystroke {
        Keystroke::parse(s).expect("keystroke")
    }

    fn key(i: KeyIntent) -> (String, u32) {
        match i {
            KeyIntent::Send(Event::Key(k)) => (k.key, k.mods),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn special_and_modified_keys_are_key_events_and_text_is_left_to_ime() {
        assert_eq!(key(intent(&ks("up"), false)), ("ArrowUp".into(), 0));
        assert_eq!(key(intent(&ks("ctrl-c"), false)), ("KeyC".into(), 2));
        assert_eq!(
            key(intent(&ks("shift-left"), false)),
            ("ArrowLeft".into(), 1)
        );
        assert_eq!(key(intent(&ks("alt-b"), false)), ("KeyB".into(), 4));
        assert_eq!(key(intent(&ks("f5"), false)), ("F5".into(), 0));
        assert_eq!(key(intent(&ks("ctrl-["), false)), ("BracketLeft".into(), 2));
        assert_eq!(intent(&ks("a"), false), KeyIntent::Pass);
        assert_eq!(intent(&ks("shift-a"), false), KeyIntent::Pass);
        if cfg!(target_os = "macos") {
            assert_eq!(intent(&ks("cmd-v"), false), KeyIntent::Paste);
            assert_eq!(intent(&ks("cmd-q"), false), KeyIntent::Pass);
        } else {
            assert_eq!(intent(&ks("ctrl-shift-v"), false), KeyIntent::Paste);
        }
    }
}
