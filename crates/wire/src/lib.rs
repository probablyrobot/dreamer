//! Dreamer's application protocol: framing, records,
//! handshake, limits, and the priority and bulk queues.
//!
//! The byte layout is documented in `docs/wire.md`. Each side opens with an
//! 8-byte preamble (magic and protocol version), so a major-version mismatch
//! is found before any record is decoded. The client then sends `Hello`;
//! the server answers `Welcome` or `Reject` after checking the engine build
//! family. Only after `Welcome` are requests decoded.
//!
//! [`Decoder`] and [`Outbox`] do no I/O; [`io`] adapts them to Tokio.

mod frame;
mod handshake;
pub mod io;
mod outbox;
mod records;

pub use frame::{Decoder, FLAG_MORE, HEADER_LEN, Item, Kind, encode_frame};
pub use handshake::{
    Identity, Negotiated, Verdict, evaluate, evaluate_preamble, hello_for, welcome,
};
pub use outbox::Outbox;
pub use records::{Bulk, Event, Goodbye, Hello, Reject, RejectReason, Request, Response, Welcome};

/// `D R M R`: identifies a Dreamer peer before anything else is read.
pub const MAGIC: [u8; 4] = *b"DRMR";
pub const PREAMBLE_LEN: usize = 8;
/// The protocol version this build speaks; the API schema versions with it.
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;

/// Size limits for one connection. Both sides advertise theirs in the
/// handshake and use the smaller of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest frame payload. A frame header claiming more is a protocol
    /// error, detected before any allocation.
    pub max_frame: u32,
    /// Largest reassembled bulk message.
    pub max_message: u32,
    /// Bulk messages are cut into chunks of this size, so priority frames
    /// wait behind at most one chunk.
    pub bulk_chunk: u32,
    /// Bytes that may wait in the outgoing priority queue.
    pub max_priority_queued: u32,
    /// Bytes that may wait in the outgoing bulk queue.
    pub max_bulk_queued: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_frame: 1 << 20,
            max_message: 64 << 20,
            bulk_chunk: 64 << 10,
            max_priority_queued: 8 << 20,
            max_bulk_queued: 128 << 20,
        }
    }
}

impl Limits {
    /// The smaller of each advertised limit.
    pub fn negotiate(&self, peer_max_frame: u32, peer_max_message: u32) -> Limits {
        let max_frame = self.max_frame.min(peer_max_frame.max(4096));
        Limits {
            max_frame,
            max_message: self.max_message.min(peer_max_message.max(max_frame)),
            bulk_chunk: self.bulk_chunk.min(max_frame),
            ..*self
        }
    }
}

pub fn preamble() -> [u8; PREAMBLE_LEN] {
    let mut p = [0u8; PREAMBLE_LEN];
    p[..4].copy_from_slice(&MAGIC);
    p[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_be_bytes());
    p[6..8].copy_from_slice(&PROTOCOL_MINOR.to_be_bytes());
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The first bytes are not a Dreamer preamble.
    NotDreamer,
    /// A frame header claims more than the negotiated limit.
    FrameTooLarge {
        len: u32,
        max: u32,
    },
    /// Reassembled bulk data would exceed the limit.
    MessageTooLarge {
        max: u32,
    },
    UnknownKind(u8),
    /// A frame arrived that is not allowed at this point.
    Unexpected(&'static str),
    Decode(String),
    /// The outgoing queue is full; the caller must slow down.
    Full,
    Closed,
    Io(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::NotDreamer => write!(f, "peer is not speaking the Dreamer protocol"),
            WireError::FrameTooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds the {max}-byte limit")
            }
            WireError::MessageTooLarge { max } => {
                write!(f, "message exceeds the {max}-byte limit")
            }
            WireError::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            WireError::Unexpected(what) => write!(f, "unexpected {what}"),
            WireError::Decode(e) => write!(f, "invalid record: {e}"),
            WireError::Full => write!(f, "outgoing queue is full"),
            WireError::Closed => write!(f, "connection closed"),
            WireError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<prost::DecodeError> for WireError {
    fn from(e: prost::DecodeError) -> Self {
        WireError::Decode(e.to_string())
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            WireError::Closed
        } else {
            WireError::Io(e.to_string())
        }
    }
}
