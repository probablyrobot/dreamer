//! Control-mode output unescaping.
//!
//! tmux writes pane output with every byte below 0x20 and the backslash
//! itself replaced by a backslash and three octal digits (`\012`, `\134`).
//! All other bytes, including invalid UTF-8 and bytes >= 0x80, pass through
//! unchanged. Unescaping restores the exact bytes the application wrote.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnescapeError {
    /// Byte offset of the offending backslash.
    pub offset: usize,
}

impl fmt::Display for UnescapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed octal escape at byte {}", self.offset)
    }
}

impl std::error::Error for UnescapeError {}

/// Decode tmux's escaped pane data into `out` (appending).
pub fn unescape(escaped: &[u8], out: &mut Vec<u8>) -> Result<(), UnescapeError> {
    out.reserve(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        // Copy the run up to the next backslash in one step.
        let run_end = escaped[i..]
            .iter()
            .position(|&b| b == b'\\')
            .map_or(escaped.len(), |p| i + p);
        out.extend_from_slice(&escaped[i..run_end]);
        i = run_end;
        if i == escaped.len() {
            break;
        }
        let digits = escaped
            .get(i + 1..i + 4)
            .ok_or(UnescapeError { offset: i })?;
        let mut value: u32 = 0;
        for &d in digits {
            if !(b'0'..=b'7').contains(&d) {
                return Err(UnescapeError { offset: i });
            }
            value = value * 8 + u32::from(d - b'0');
        }
        let byte = u8::try_from(value).map_err(|_| UnescapeError { offset: i })?;
        out.push(byte);
        i += 4;
    }
    Ok(())
}

/// Escape bytes the way tmux does. Used to build fixtures and tests.
pub fn escape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &b in raw {
        if b < b' ' || b == b'\\' {
            out.extend_from_slice(format!("\\{b:03o}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &[u8]) -> Result<Vec<u8>, UnescapeError> {
        let mut out = Vec::new();
        unescape(s, &mut out).map(|()| out)
    }

    #[test]
    fn decodes_controls_backslash_and_high_bytes() {
        assert_eq!(decode(b"a\\015\\012b").expect("ok"), b"a\r\nb");
        assert_eq!(decode(b"\\134").expect("ok"), b"\\");
        assert_eq!(decode(b"\\000").expect("ok"), b"\0");
        assert_eq!(decode(b"\\033[1m").expect("ok"), b"\x1b[1m");
        assert_eq!(decode(b"\xff\xfe ok").expect("ok"), b"\xff\xfe ok");
        assert_eq!(decode(b"  trailing  ").expect("ok"), b"  trailing  ");
    }

    #[test]
    fn rejects_truncated_and_invalid_escapes() {
        assert_eq!(decode(b"ab\\01"), Err(UnescapeError { offset: 2 }));
        assert_eq!(decode(b"\\"), Err(UnescapeError { offset: 0 }));
        assert_eq!(decode(b"\\08x"), Err(UnescapeError { offset: 0 }));
        assert_eq!(decode(b"\\400"), Err(UnescapeError { offset: 0 }));
    }

    #[test]
    fn escape_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&escape(&all)).expect("ok"), all);
    }
}
