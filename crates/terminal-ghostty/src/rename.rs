//! tmux's own window-name sequence, `ESC k name ST` (from GNU screen).
//!
//! A shell inside tmux (`TERM=tmux-256color` or `screen*`) commonly sends
//! it with each command, as zsh's title support does. tmux consumes it and
//! names the window; libghostty-vt does not know it and would print the
//! name as text. So the engine drops it before parsing, as tmux does: up to
//! `ESC \`, or BEL, which tmux also accepts. Every other byte passes
//! through unchanged and in order, and a sequence split across reads is
//! still found.

use std::borrow::Cow;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rename {
    #[default]
    Ground,
    /// An `ESC` was last, held until the next byte says what it starts.
    Esc,
    /// Inside the name.
    Name,
    /// An `ESC` inside the name: `\` ends it.
    NameEsc,
}

impl Rename {
    /// `bytes` without any `ESC k … ST`. Borrowed when nothing is dropped
    /// or held.
    pub(crate) fn strip<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if *self == Rename::Ground && !bytes.contains(&ESC) {
            return Cow::Borrowed(bytes);
        }
        let mut out = Vec::with_capacity(bytes.len() + 1);
        for &b in bytes {
            self.byte(b, &mut out);
        }
        Cow::Owned(out)
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        match *self {
            Rename::Ground => {
                if b == ESC {
                    *self = Rename::Esc;
                } else {
                    out.push(b);
                }
            }
            Rename::Esc => {
                if b == b'k' {
                    *self = Rename::Name;
                } else {
                    out.push(ESC);
                    *self = Rename::Ground;
                    self.byte(b, out);
                }
            }
            Rename::Name => match b {
                ESC => *self = Rename::NameEsc,
                BEL | CAN | SUB => *self = Rename::Ground,
                _ => {}
            },
            Rename::NameEsc => {
                *self = Rename::Ground;
                if b != b'\\' {
                    // The name ended without ST: this ESC starts something
                    // else.
                    self.byte(ESC, out);
                    self.byte(b, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(input: &[u8]) -> Vec<u8> {
        let mut r = Rename::default();
        r.strip(input).into_owned()
    }

    #[test]
    fn a_window_name_is_dropped_and_everything_else_kept() {
        // What zsh sent in tmux before a command's output (#59 run).
        let zsh = b"\x1b]2;printf x\x1b\\\x1bkprintf\x1b\\error[E0335]: bytes\r\n";
        assert_eq!(all(zsh), b"\x1b]2;printf x\x1b\\error[E0335]: bytes\r\n");
        assert_eq!(all(b"a\x1bkname\x07b"), b"ab", "BEL ends it too");
        let plain = b"\x1b[1mbold\x1b[0m \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \x1bM \x1b7\x1b8";
        assert_eq!(all(plain), plain.to_vec(), "other sequences pass");
        assert!(matches!(
            Rename::default().strip(b"no escapes"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_name_split_across_reads_is_still_dropped() {
        let input = b"x\x1bkname\x1b\\y\x1b[31mz";
        for cut in 0..=input.len() {
            let mut r = Rename::default();
            let mut out = r.strip(&input[..cut]).into_owned();
            out.extend_from_slice(&r.strip(&input[cut..]));
            assert_eq!(out, b"xy\x1b[31mz", "cut at {cut}");
        }
    }

    #[test]
    fn a_name_cut_short_by_another_escape_gives_that_escape_back() {
        assert_eq!(all(b"\x1bkab\x1b[31mred"), b"\x1b[31mred");
    }
}
