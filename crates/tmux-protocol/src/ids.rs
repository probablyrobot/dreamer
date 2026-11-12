//! tmux object identifiers as they appear on the wire (`$1`, `@2`, `%3`).
//!
//! These are only unique within one tmux server incarnation; higher layers
//! qualify them with server identity and epoch.

use std::fmt;

macro_rules! id_type {
    ($name:ident, $sigil:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl $name {
            /// Parse `<sigil><decimal>` with no sign, spaces, or leading `+`.
            pub fn parse(token: &[u8]) -> Option<Self> {
                let digits = token.strip_prefix(&[$sigil][..])?;
                if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                std::str::from_utf8(digits).ok()?.parse().ok().map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil as char, self.0)
            }
        }
    };
}

id_type!(SessionId, b'$');
id_type!(WindowId, b'@');
id_type!(PaneId, b'%');

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        assert_eq!(PaneId::parse(b"%12"), Some(PaneId(12)));
        assert_eq!(WindowId::parse(b"@0"), Some(WindowId(0)));
        assert_eq!(SessionId::parse(b"$7"), Some(SessionId(7)));
        assert_eq!(PaneId(3).to_string(), "%3");
        for bad in [&b"%"[..], b"12", b"%-1", b"%+1", b"% 1", b"@1"] {
            assert_eq!(PaneId::parse(bad), None, "{bad:?}");
        }
        assert_eq!(PaneId::parse(b"%99999999999"), None);
    }
}
