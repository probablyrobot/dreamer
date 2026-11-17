//! tmux key names (`C-b`, `M-Up`, `PPage`) as keys, so a window's key
//! layer can tell the user's prefix when it is pressed, hand the next key
//! to tmux by name, and drive copy mode with tmux's own key tables.

/// A key without its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// A character key. Shift is part of the character (`G`, `%`).
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

/// A key press: the key and the modifiers that are not part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
    /// Only for keys that are not characters (`S-Up`).
    pub shift: bool,
}

impl Key {
    pub const fn plain(code: Code) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub const fn ctrl(c: char) -> Key {
        Key {
            code: Code::Char(c),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    pub const fn alt(c: char) -> Key {
        Key {
            code: Code::Char(c),
            ctrl: false,
            alt: true,
            shift: false,
        }
    }

    /// The character typed, for a key with no Ctrl or Alt.
    pub fn text(&self) -> Option<char> {
        match self.code {
            Code::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }

    /// The key as tmux names it, for `send-keys`.
    pub fn tmux_name(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("C-");
        }
        if self.alt {
            out.push_str("M-");
        }
        if self.shift {
            out.push_str("S-");
        }
        let name = match self.code {
            Code::Char(' ') => "Space",
            Code::Char(c) => {
                out.push(c);
                return out;
            }
            Code::Enter => "Enter",
            Code::Escape => "Escape",
            Code::Tab => "Tab",
            Code::Backspace => "BSpace",
            Code::Up => "Up",
            Code::Down => "Down",
            Code::Left => "Left",
            Code::Right => "Right",
            Code::Home => "Home",
            Code::End => "End",
            Code::PageUp => "PPage",
            Code::PageDown => "NPage",
            Code::Insert => "IC",
            Code::Delete => "DC",
            Code::F(n) => {
                out.push_str(&format!("F{n}"));
                return out;
            }
        };
        out.push_str(name);
        out
    }
}

/// A tmux key name as a key; `None` for `None` and for names this layer
/// does not know.
pub fn parse(name: &str) -> Option<Key> {
    let mut key = Key::plain(Code::Enter);
    let mut rest = name;
    loop {
        if let Some(r) = rest.strip_prefix("C-").filter(|r| !r.is_empty()) {
            key.ctrl = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-").filter(|r| !r.is_empty()) {
            key.alt = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-").filter(|r| !r.is_empty()) {
            key.shift = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('^').filter(|r| r.chars().count() == 1) {
            key.ctrl = true;
            rest = r;
        } else {
            break;
        }
    }
    key.code = match rest {
        "Enter" => Code::Enter,
        "Escape" => Code::Escape,
        "Tab" => Code::Tab,
        "BTab" => {
            key.shift = true;
            Code::Tab
        }
        "BSpace" => Code::Backspace,
        "Space" => Code::Char(' '),
        "Up" => Code::Up,
        "Down" => Code::Down,
        "Left" => Code::Left,
        "Right" => Code::Right,
        "Home" => Code::Home,
        "End" => Code::End,
        "PPage" | "PageUp" | "PgUp" => Code::PageUp,
        "NPage" | "PageDown" | "PgDn" => Code::PageDown,
        "IC" | "Insert" => Code::Insert,
        "DC" | "Delete" => Code::Delete,
        _ => {
            if let Some(n) = rest.strip_prefix('F').and_then(|n| n.parse::<u8>().ok())
                && (1..=24).contains(&n)
            {
                Code::F(n)
            } else {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Code::Char(c),
                    _ => return None,
                }
            }
        }
    };
    if let Code::Char(c) = key.code {
        // tmux reads C-A as C-a; a character carries its own shift.
        if key.ctrl {
            key.code = Code::Char(c.to_ascii_lowercase());
        }
        key.shift = false;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_names_parse_and_print_back() {
        assert_eq!(parse("C-b"), Some(Key::ctrl('b')));
        assert_eq!(parse("C-B"), Some(Key::ctrl('b')));
        assert_eq!(parse("^A"), Some(Key::ctrl('a')));
        assert_eq!(parse("C-Space"), Some(Key::ctrl(' ')));
        assert_eq!(parse("M-x"), Some(Key::alt('x')));
        assert_eq!(parse("`"), Some(Key::plain(Code::Char('`'))));
        assert_eq!(parse("F12"), Some(Key::plain(Code::F(12))));
        assert_eq!(parse("None"), None);
        for name in [
            "C-b", "c", "%", "\"", "C-o", "M-1", "S-Up", "PPage", "F1", "Space", "C-Space",
        ] {
            assert_eq!(parse(name).map(|k| k.tmux_name()).as_deref(), Some(name));
        }
    }
}
