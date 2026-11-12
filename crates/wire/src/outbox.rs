//! Outgoing frames in two queues (spec 11.4). Priority frames (input,
//! requests, responses, events) always go first; a bulk message is cut into
//! chunks so a large transfer delays priority traffic by at most one chunk.

use std::collections::VecDeque;

use crate::frame::{FLAG_MORE, Kind, encode_frame};
use crate::{Limits, WireError};

#[derive(Debug)]
pub struct Outbox {
    limits: Limits,
    priority: VecDeque<Vec<u8>>,
    priority_bytes: usize,
    bulk: VecDeque<Vec<u8>>,
    bulk_bytes: usize,
    /// Bytes of the head bulk message already sent.
    sent: usize,
}

impl Outbox {
    pub fn new(limits: Limits) -> Outbox {
        Outbox {
            limits,
            priority: VecDeque::new(),
            priority_bytes: 0,
            bulk: VecDeque::new(),
            bulk_bytes: 0,
            sent: 0,
        }
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Queue one small frame. `Full` means the peer is not reading; the
    /// caller decides whether to wait or drop the connection.
    pub fn push_priority(&mut self, kind: Kind, payload: &[u8]) -> Result<(), WireError> {
        if kind == Kind::Bulk {
            return Err(WireError::Unexpected("bulk data on the priority queue"));
        }
        let frame = encode_frame(kind, 0, payload, self.limits.max_frame)?;
        if self.priority_bytes + frame.len() > self.limits.max_priority_queued as usize {
            return Err(WireError::Full);
        }
        self.priority_bytes += frame.len();
        self.priority.push_back(frame);
        Ok(())
    }

    /// Queue one bulk message (a `Bulk` record, already encoded).
    pub fn push_bulk(&mut self, message: Vec<u8>) -> Result<(), WireError> {
        if message.len() > self.limits.max_message as usize {
            return Err(WireError::MessageTooLarge {
                max: self.limits.max_message,
            });
        }
        if self.bulk_bytes + message.len() > self.limits.max_bulk_queued as usize {
            return Err(WireError::Full);
        }
        self.bulk_bytes += message.len();
        self.bulk.push_back(message);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.priority.is_empty() && self.bulk.is_empty()
    }

    pub fn queued_bytes(&self) -> (usize, usize) {
        (self.priority_bytes, self.bulk_bytes - self.sent)
    }

    /// The next frame to write.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if let Some(f) = self.priority.pop_front() {
            self.priority_bytes -= f.len();
            return Some(f);
        }
        let head = self.bulk.front()?;
        let chunk = (self.limits.bulk_chunk.min(self.limits.max_frame) as usize).max(1);
        let end = (self.sent + chunk).min(head.len());
        let more = end < head.len();
        let frame = encode_frame(
            Kind::Bulk,
            if more { FLAG_MORE } else { 0 },
            &head[self.sent..end],
            self.limits.max_frame,
        )
        .ok()?;
        if more {
            self.sent = end;
        } else {
            let done = self.bulk.pop_front().map_or(0, |m| m.len());
            self.bulk_bytes -= done;
            self.sent = 0;
        }
        Some(frame)
    }
}
