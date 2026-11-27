//! Incremental, byte-oriented parser for tmux control mode (`tmux -C`).
//!
//! Feed bytes in whatever chunks the transport delivers; the parser frames
//! lines itself, so a read boundary may fall anywhere (inside an escape, a
//! guard line, or a multi-byte character). Terminal data from `%output` and
//! `%extended-output` is unescaped back to the original pane bytes and never
//! passed through a UTF-8 conversion.
//!
//! Inside a `%begin`/`%end` (or `%error`) block every line is response body,
//! even one that starts with `%`: tmux never interleaves notifications with
//! a command response.
//!
//! Limits are enforced before allocation grows past them. Exceeding a limit,
//! a malformed escape, or a broken guard is a [`ProtocolError`]; after one,
//! the parser refuses further input, because a control stream cannot be
//! resynchronized safely. The connection owner must reconnect and reconcile.

mod escape;
mod ids;
mod notification;

pub use escape::{UnescapeError, escape, unescape};
pub use ids::{PaneId, SessionId, WindowId};
pub use notification::{Notification, parse_notification};

use std::fmt;

/// Bounds on what the parser will buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest single line accepted, in bytes, excluding the newline.
    pub max_line: usize,
    /// Most body bytes accepted for one command response.
    pub max_response: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // A `%output` line carries at most what tmux read from the pane
            // at once, escaped (up to 4x). 1 MiB is far above that.
            max_line: 1 << 20,
            // Large `capture-pane` results with history fit comfortably.
            max_response: 64 << 20,
        }
    }
}

/// Guard header shared by `%begin`, `%end`, and `%error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    pub time: u64,
    pub number: u64,
    pub flags: u32,
}

/// A complete command response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub guard: Guard,
    /// `false` when the block ended with `%error`.
    pub ok: bool,
    /// Body lines without their newline, as raw bytes.
    pub body: Vec<Vec<u8>>,
}

/// One parsed item from the control stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    LineTooLong {
        limit: usize,
    },
    ResponseTooLarge {
        limit: usize,
    },
    /// `%end`/`%error` whose numbers do not match the open `%begin`.
    GuardMismatch {
        begin: Guard,
        end: Guard,
    },
    /// A guard line that could not be parsed.
    MalformedGuard {
        line: Vec<u8>,
    },
    /// `%end` or `%error` with no open block.
    UnexpectedEnd {
        line: Vec<u8>,
    },
    /// A recognized notification with arguments that could not be parsed.
    MalformedNotification {
        line: Vec<u8>,
        reason: String,
    },
    /// A line outside a response that is not a notification.
    UnexpectedLine {
        line: Vec<u8>,
    },
    /// End of stream inside a response or a partial line.
    UnexpectedEof {
        in_response: bool,
        partial_line: bool,
    },
    /// Input after an earlier error.
    Poisoned,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(80)]).into_owned();
        match self {
            Self::LineTooLong { limit } => write!(f, "control line longer than {limit} bytes"),
            Self::ResponseTooLarge { limit } => {
                write!(f, "command response larger than {limit} bytes")
            }
            Self::GuardMismatch { begin, end } => write!(
                f,
                "response guard mismatch: began {} {}, ended {} {}",
                begin.time, begin.number, end.time, end.number
            ),
            Self::MalformedGuard { line } => write!(f, "malformed guard line `{}`", show(line)),
            Self::UnexpectedEnd { line } => {
                write!(f, "end guard without a response: `{}`", show(line))
            }
            Self::MalformedNotification { line, reason } => {
                write!(f, "malformed notification ({reason}): `{}`", show(line))
            }
            Self::UnexpectedLine { line } => {
                write!(f, "unexpected line outside a response: `{}`", show(line))
            }
            Self::UnexpectedEof {
                in_response,
                partial_line,
            } => write!(
                f,
                "control stream ended early (in response: {in_response}, partial line: {partial_line})"
            ),
            Self::Poisoned => write!(f, "parser already failed; reconnect"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug)]
struct Open {
    guard: Guard,
    body: Vec<Vec<u8>>,
    bytes: usize,
}

/// Incremental control-mode parser.
#[derive(Debug)]
pub struct Parser {
    limits: Limits,
    line: Vec<u8>,
    open: Option<Open>,
    failed: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Parser {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            line: Vec::new(),
            open: None,
            failed: false,
        }
    }

    /// True while a command response is open.
    pub fn in_response(&self) -> bool {
        self.open.is_some()
    }

    /// Consume a chunk, appending complete events to `out`.
    ///
    /// Events parsed before an error are still appended, so callers can act on
    /// everything that preceded the failure.
    pub fn feed(&mut self, mut chunk: &[u8], out: &mut Vec<Event>) -> Result<(), ProtocolError> {
        if self.failed {
            return Err(ProtocolError::Poisoned);
        }
        while !chunk.is_empty() {
            let (piece, rest, complete) = match memchr::memchr(b'\n', chunk) {
                Some(i) => (&chunk[..i], &chunk[i + 1..], true),
                None => (chunk, &[][..], false),
            };
            if self.line.len() + piece.len() > self.limits.max_line {
                return Err(self.fail(ProtocolError::LineTooLong {
                    limit: self.limits.max_line,
                }));
            }
            chunk = rest;
            if !complete {
                self.line.extend_from_slice(piece);
                continue;
            }
            // A line that arrived whole is handled where it lies; only one
            // split across reads is put together first, in a buffer kept
            // for the next one.
            let result = if self.line.is_empty() {
                self.handle_line(piece, out)
            } else {
                let mut line = std::mem::take(&mut self.line);
                line.extend_from_slice(piece);
                let r = self.handle_line(&line, out);
                line.clear();
                self.line = line;
                r
            };
            if let Err(e) = result {
                return Err(self.fail(e));
            }
        }
        Ok(())
    }

    /// Signal end of stream. Errors if a response or line was left open.
    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        if self.failed {
            return Err(ProtocolError::Poisoned);
        }
        let in_response = self.open.is_some();
        let partial_line = !self.line.is_empty();
        if in_response || partial_line {
            return Err(self.fail(ProtocolError::UnexpectedEof {
                in_response,
                partial_line,
            }));
        }
        Ok(())
    }

    fn fail(&mut self, e: ProtocolError) -> ProtocolError {
        self.failed = true;
        self.line = Vec::new();
        self.open = None;
        e
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<Event>) -> Result<(), ProtocolError> {
        if let Some(open) = &mut self.open {
            let end = if line.starts_with(b"%end ") {
                Some(true)
            } else if line.starts_with(b"%error ") {
                Some(false)
            } else {
                None
            };
            if let Some(ok) = end {
                let rest = &line[if ok { 5 } else { 7 }..];
                let guard = parse_guard(rest).ok_or_else(|| ProtocolError::MalformedGuard {
                    line: line.to_vec(),
                })?;
                // A body line can legitimately read like an end guard; only a
                // guard carrying the open block's numbers closes it.
                if guard.number == open.guard.number && guard.time == open.guard.time {
                    let open = self.open.take().ok_or(ProtocolError::Poisoned)?;
                    out.push(Event::Response(Response {
                        guard: open.guard,
                        ok,
                        body: open.body,
                    }));
                    return Ok(());
                }
            }
            open.bytes += line.len() + 1;
            if open.bytes > self.limits.max_response {
                return Err(ProtocolError::ResponseTooLarge {
                    limit: self.limits.max_response,
                });
            }
            open.body.push(line.to_vec());
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix(b"%begin ") {
            let guard = parse_guard(rest).ok_or_else(|| ProtocolError::MalformedGuard {
                line: line.to_vec(),
            })?;
            self.open = Some(Open {
                guard,
                body: Vec::new(),
                bytes: 0,
            });
            return Ok(());
        }
        if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
            return Err(ProtocolError::UnexpectedEnd {
                line: line.to_vec(),
            });
        }
        if line.first() == Some(&b'%') {
            let n = parse_notification(line).map_err(|reason| {
                ProtocolError::MalformedNotification {
                    line: line.to_vec(),
                    reason,
                }
            })?;
            out.push(Event::Notification(n));
            return Ok(());
        }
        Err(ProtocolError::UnexpectedLine {
            line: line.to_vec(),
        })
    }
}

fn parse_guard(rest: &[u8]) -> Option<Guard> {
    let text = std::str::from_utf8(rest).ok()?;
    let mut parts = text.split(' ');
    let time = parts.next()?.parse().ok()?;
    let number = parts.next()?.parse().ok()?;
    let flags = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Guard {
        time,
        number,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> Result<Vec<Event>, ProtocolError> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(input, &mut out)?;
        p.finish()?;
        Ok(out)
    }

    #[test]
    fn response_body_lines_that_look_like_protocol_stay_body() {
        let input = b"%begin 1 7 1\n%begin 9 9 9\n%output %1 hi\n%exit\n%end 2 7 1\n%error 1 8 1\nplain\n%end 1 7 1\n";
        let events = parse_all(input).expect("parse");
        assert_eq!(events.len(), 1);
        let Event::Response(r) = &events[0] else {
            panic!("expected response")
        };
        assert!(r.ok);
        assert_eq!(r.guard.number, 7);
        let body: Vec<&[u8]> = r.body.iter().map(Vec::as_slice).collect();
        assert_eq!(
            body,
            [
                &b"%begin 9 9 9"[..],
                b"%output %1 hi",
                b"%exit",
                b"%end 2 7 1",
                b"%error 1 8 1",
                b"plain"
            ]
        );
    }

    #[test]
    fn error_blocks_are_failed_responses() {
        let events = parse_all(b"%begin 5 3 0\nunknown command: x\n%error 5 3 0\n").expect("parse");
        assert_eq!(
            events,
            [Event::Response(Response {
                guard: Guard {
                    time: 5,
                    number: 3,
                    flags: 0
                },
                ok: false,
                body: vec![b"unknown command: x".to_vec()],
            })]
        );
    }

    #[test]
    fn eof_inside_a_response_is_an_error() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"%begin 1 1 0\npartial body\n", &mut out)
            .expect("feed");
        assert_eq!(
            p.finish(),
            Err(ProtocolError::UnexpectedEof {
                in_response: true,
                partial_line: false
            })
        );
    }

    #[test]
    fn eof_with_a_partial_line_is_an_error() {
        let mut p = Parser::default();
        p.feed(b"%output %1 abc", &mut Vec::new()).expect("feed");
        assert!(matches!(
            p.finish(),
            Err(ProtocolError::UnexpectedEof {
                partial_line: true,
                ..
            })
        ));
    }

    #[test]
    fn limits_poison_the_parser() {
        let mut p = Parser::new(Limits {
            max_line: 8,
            max_response: 16,
        });
        let mut out = Vec::new();
        assert_eq!(
            p.feed(b"%output %1 0123456789\n", &mut out),
            Err(ProtocolError::LineTooLong { limit: 8 })
        );
        assert_eq!(p.feed(b"\n", &mut out), Err(ProtocolError::Poisoned));

        let mut p = Parser::new(Limits {
            max_line: 64,
            max_response: 16,
        });
        let r = p.feed(b"%begin 1 1 0\n0123456789\n0123456789\n", &mut out);
        assert_eq!(r, Err(ProtocolError::ResponseTooLarge { limit: 16 }));
    }

    #[test]
    fn stray_end_and_bare_text_are_errors() {
        assert!(matches!(
            parse_all(b"%end 1 1 0\n"),
            Err(ProtocolError::UnexpectedEnd { .. })
        ));
        assert!(matches!(
            parse_all(b"hello\n"),
            Err(ProtocolError::UnexpectedLine { .. })
        ));
    }

    #[test]
    fn events_before_an_error_are_delivered() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        let r = p.feed(b"%output %1 a\nbogus\n%output %1 b\n", &mut out);
        assert!(r.is_err());
        assert_eq!(out.len(), 1);
    }
}
