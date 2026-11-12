//! Frames: `u32 length (big-endian) | u8 kind | u8 flags | payload`.
//!
//! The decoder never reserves memory for a length a peer claims. It checks
//! the header against the limit first and then grows only with bytes that
//! actually arrived.

use crate::{Limits, MAGIC, PREAMBLE_LEN, WireError};

pub const HEADER_LEN: usize = 6;
/// On a bulk frame: more chunks of the same message follow.
pub const FLAG_MORE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Welcome = 2,
    Reject = 3,
    Request = 4,
    Response = 5,
    Event = 6,
    Bulk = 7,
    Ping = 8,
    Pong = 9,
    Goodbye = 10,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Reject,
            4 => Kind::Request,
            5 => Kind::Response,
            6 => Kind::Event,
            7 => Kind::Bulk,
            8 => Kind::Ping,
            9 => Kind::Pong,
            10 => Kind::Goodbye,
            _ => return None,
        })
    }
}

pub fn encode_frame(
    kind: Kind,
    flags: u8,
    payload: &[u8],
    max_frame: u32,
) -> Result<Vec<u8>, WireError> {
    let len = u32::try_from(payload.len()).map_err(|_| WireError::FrameTooLarge {
        len: u32::MAX,
        max: max_frame,
    })?;
    if len > max_frame {
        return Err(WireError::FrameTooLarge {
            len,
            max: max_frame,
        });
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind as u8);
    out.push(flags);
    out.extend_from_slice(payload);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Preamble {
        major: u16,
        minor: u16,
    },
    /// A complete frame; for bulk, the reassembled message.
    Frame {
        kind: Kind,
        payload: Vec<u8>,
    },
}

#[derive(Debug)]
pub struct Decoder {
    limits: Limits,
    buf: Vec<u8>,
    start: usize,
    preamble_done: bool,
    bulk: Option<Vec<u8>>,
    failed: bool,
}

impl Decoder {
    pub fn new(limits: Limits) -> Decoder {
        Decoder {
            limits,
            buf: Vec::new(),
            start: 0,
            preamble_done: false,
            bulk: None,
            failed: false,
        }
    }

    /// Adopt negotiated limits after the handshake.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Bytes this decoder holds allocated, for limit tests.
    pub fn held_capacity(&self) -> usize {
        self.buf.capacity() + self.bulk.as_ref().map_or(0, Vec::capacity)
    }

    /// Add received bytes. Feed a bounded read at a time and drain
    /// [`Decoder::next_item`] in between: buffered data never exceeds one
    /// partial frame plus one read.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_item(&mut self) -> Result<Option<Item>, WireError> {
        if self.failed {
            return Err(WireError::Unexpected("data after a protocol error"));
        }
        let r = self.next_inner();
        if r.is_err() {
            // A stream with a bad frame cannot be resynchronized.
            self.failed = true;
            self.buf = Vec::new();
            self.bulk = None;
        }
        r
    }

    fn next_inner(&mut self) -> Result<Option<Item>, WireError> {
        loop {
            let avail = &self.buf[self.start..];
            if !self.preamble_done {
                if avail.len() < PREAMBLE_LEN {
                    if !MAGIC.starts_with(&avail[..avail.len().min(4)]) {
                        return Err(WireError::NotDreamer);
                    }
                    return Ok(None);
                }
                if avail[..4] != MAGIC {
                    return Err(WireError::NotDreamer);
                }
                let major = u16::from_be_bytes([avail[4], avail[5]]);
                let minor = u16::from_be_bytes([avail[6], avail[7]]);
                self.start += PREAMBLE_LEN;
                self.preamble_done = true;
                return Ok(Some(Item::Preamble { major, minor }));
            }
            if avail.len() < HEADER_LEN {
                return Ok(None);
            }
            let len = u32::from_be_bytes([avail[0], avail[1], avail[2], avail[3]]);
            if len > self.limits.max_frame {
                return Err(WireError::FrameTooLarge {
                    len,
                    max: self.limits.max_frame,
                });
            }
            let kind = Kind::from_u8(avail[4]).ok_or(WireError::UnknownKind(avail[4]))?;
            let flags = avail[5];
            let total = HEADER_LEN + len as usize;
            if avail.len() < total {
                return Ok(None);
            }
            let payload = &avail[HEADER_LEN..total];
            if kind == Kind::Bulk {
                let so_far = self.bulk.as_ref().map_or(0, Vec::len);
                if so_far + payload.len() > self.limits.max_message as usize {
                    return Err(WireError::MessageTooLarge {
                        max: self.limits.max_message,
                    });
                }
                let mut acc = self.bulk.take().unwrap_or_default();
                acc.extend_from_slice(payload);
                self.start += total;
                if flags & FLAG_MORE != 0 {
                    self.bulk = Some(acc);
                    continue;
                }
                return Ok(Some(Item::Frame { kind, payload: acc }));
            }
            if flags & FLAG_MORE != 0 {
                return Err(WireError::Unexpected(
                    "continuation flag on a non-bulk frame",
                ));
            }
            let payload = payload.to_vec();
            self.start += total;
            return Ok(Some(Item::Frame { kind, payload }));
        }
    }
}
