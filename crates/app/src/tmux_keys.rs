//! The window's key presses in tmux's terms (see `replica::keys`).

use replica::keys::{Code, Key};

/// A window key press in tmux's terms. `None` for keys tmux has no name
/// for, and for presses with the platform modifier (Cmd on macOS).
pub fn from_keystroke(k: &gpui::Keystroke) -> Option<Key> {
    let m = &k.modifiers;
    if m.platform {
        return None;
    }
    let named = match k.key.as_str() {
        "enter" => Some(Code::Enter),
        "escape" => Some(Code::Escape),
        "tab" => Some(Code::Tab),
        "backspace" => Some(Code::Backspace),
        "up" => Some(Code::Up),
        "down" => Some(Code::Down),
        "left" => Some(Code::Left),
        "right" => Some(Code::Right),
        "home" => Some(Code::Home),
        "end" => Some(Code::End),
        "pageup" => Some(Code::PageUp),
        "pagedown" => Some(Code::PageDown),
        "insert" => Some(Code::Insert),
        "delete" => Some(Code::Delete),
        "space" => Some(Code::Char(' ')),
        f if f.len() > 1 && f.starts_with('f') => f[1..].parse().ok().map(Code::F),
        _ => None,
    };
    if let Some(code) = named {
        let char_key = matches!(code, Code::Char(_));
        return Some(Key {
            code,
            ctrl: m.control,
            alt: m.alt,
            shift: m.shift && !char_key,
        });
    }
    let mut chars = k.key.chars();
    let base = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => return None,
    };
    // The character the key makes with Shift; with Alt alone the platform
    // may compose another character, which tmux would not see.
    let c = if m.shift {
        k.key_char
            .as_deref()
            .filter(|_| !m.control && !m.alt)
            .and_then(|s| {
                let mut cs = s.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                }
            })
            .unwrap_or_else(|| shifted(base))
    } else {
        base
    };
    Some(Key {
        code: Code::Char(if m.control { c.to_ascii_lowercase() } else { c }),
        ctrl: m.control,
        alt: m.alt,
        shift: false,
    })
}

/// The character Shift makes on a US layout, when the platform did not
/// say.
fn shifted(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use replica::keys::parse;

    fn ks(s: &'static str) -> gpui::Keystroke {
        gpui::Keystroke::parse(s).expect("keystroke")
    }

    #[test]
    fn key_presses_match_the_names_tmux_uses() {
        assert_eq!(from_keystroke(&ks("ctrl-b")), parse("C-b"));
        assert_eq!(from_keystroke(&ks("ctrl-space")), parse("C-Space"));
        assert_eq!(from_keystroke(&ks("shift-g")), parse("G"));
        assert_eq!(from_keystroke(&ks("shift-5")), parse("%"));
        assert_eq!(from_keystroke(&ks("shift-'")), parse("\""));
        assert_eq!(from_keystroke(&ks("alt-w")), parse("M-w"));
        assert_eq!(from_keystroke(&ks("pageup")), parse("PPage"));
        assert_eq!(from_keystroke(&ks("shift-up")), parse("S-Up"));
        assert_eq!(from_keystroke(&ks("[")), parse("["));
        assert_eq!(from_keystroke(&ks("cmd-c")), None);
    }
}
