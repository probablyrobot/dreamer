//! The per-pane ordered journal (spec 11.5): every record that changed the
//! canonical engine, numbered by the mirror's committed sequence, bounded
//! per pane, per host, and by age. Trimming drops the oldest records and
//! advances the first retained sequence by exactly what it dropped.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::worker::Record;

/// Retention limits (spec 16.4 starting points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub per_pane_bytes: usize,
    pub host_bytes: usize,
    pub max_age: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            per_pane_bytes: 8 << 20,
            host_bytes: 256 << 20,
            max_age: Duration::from_secs(600),
        }
    }
}

/// Bytes a record costs in the journal.
pub fn cost(r: &Record) -> usize {
    match r {
        Record::Bytes(b) => b.len().max(1),
        Record::Resize { .. } => 8,
    }
}

#[derive(Debug)]
pub struct Journal {
    records: VecDeque<(u64, Instant, Record)>,
    bytes: usize,
    /// Sequence the next record gets.
    next: u64,
    retention: Retention,
    /// Journal bytes of every pane on this host.
    host: Arc<AtomicUsize>,
}

impl Journal {
    /// A journal whose first record will have sequence `next`.
    pub fn new(next: u64, retention: Retention, host: Arc<AtomicUsize>) -> Journal {
        Journal {
            records: VecDeque::new(),
            bytes: 0,
            next,
            retention,
            host,
        }
    }

    /// Append the record that just became committed sequence `next`.
    pub fn push(&mut self, record: Record, now: Instant) -> u64 {
        let seq = self.next;
        self.next += 1;
        let c = cost(&record);
        self.bytes += c;
        self.host.fetch_add(c, Ordering::Relaxed);
        self.records.push_back((seq, now, record));
        self.trim(now);
        seq
    }

    fn pop(&mut self) -> bool {
        match self.records.pop_front() {
            Some((_, _, r)) => {
                let c = cost(&r);
                self.bytes -= c;
                self.host.fetch_sub(c, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Enforce age, per-pane, and host limits, oldest first. Over the host
    /// limit, the pane that is writing gives up its own oldest records.
    pub fn trim(&mut self, now: Instant) {
        while self
            .records
            .front()
            .is_some_and(|(_, t, _)| now.duration_since(*t) > self.retention.max_age)
        {
            self.pop();
        }
        while self.bytes > self.retention.per_pane_bytes && self.pop() {}
        while self.host.load(Ordering::Relaxed) > self.retention.host_bytes && self.pop() {}
    }

    /// Sequence of the oldest retained record; equals `next_sequence` when
    /// nothing is retained.
    pub fn first_sequence(&self) -> u64 {
        self.records.front().map_or(self.next, |(s, _, _)| *s)
    }

    pub fn next_sequence(&self) -> u64 {
        self.next
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Records from `from` on, in order, if all of them are retained.
    pub fn since(&self, from: u64) -> Option<impl Iterator<Item = (u64, &Record)>> {
        (from >= self.first_sequence() && from <= self.next).then(|| {
            self.records
                .iter()
                .filter(move |(s, _, _)| *s >= from)
                .map(|(s, _, r)| (*s, r))
        })
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        self.host.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize) -> Record {
        Record::Bytes(vec![b'x'; n])
    }

    fn retention(pane: usize, host: usize) -> Retention {
        Retention {
            per_pane_bytes: pane,
            host_bytes: host,
            max_age: Duration::from_secs(3600),
        }
    }

    #[test]
    fn sequences_are_contiguous_and_trimming_advances_the_first_exactly() {
        let host = Arc::new(AtomicUsize::new(0));
        let mut j = Journal::new(1, retention(100, 1000), host.clone());
        let t = Instant::now();
        for i in 0..10 {
            assert_eq!(j.push(bytes(30), t), 1 + i);
        }
        // 10 records of 30 bytes, at most 100 kept: the last 3.
        assert_eq!(j.len(), 3);
        assert_eq!(j.bytes(), 90);
        assert_eq!(j.first_sequence(), 8);
        assert_eq!(j.next_sequence(), 11);
        assert_eq!(j.first_sequence() + j.len() as u64, j.next_sequence());
        assert_eq!(host.load(Ordering::Relaxed), 90);
        let seqs: Vec<u64> = j.since(9).expect("retained").map(|(s, _)| s).collect();
        assert_eq!(seqs, [9, 10]);
        assert!(
            j.since(7).is_none(),
            "trimmed records are reported as a gap"
        );
        assert_eq!(j.since(11).expect("empty tail").count(), 0);
        drop(j);
        assert_eq!(
            host.load(Ordering::Relaxed),
            0,
            "a dropped journal returns its bytes"
        );
    }

    #[test]
    fn the_host_limit_holds_across_panes() {
        let host = Arc::new(AtomicUsize::new(0));
        let mut a = Journal::new(1, retention(1000, 250), host.clone());
        let mut b = Journal::new(1, retention(1000, 250), host.clone());
        let t = Instant::now();
        for _ in 0..10 {
            a.push(bytes(20), t);
        }
        assert_eq!(host.load(Ordering::Relaxed), 200);
        for _ in 0..10 {
            b.push(bytes(20), t);
            assert!(host.load(Ordering::Relaxed) <= 250);
        }
        // The writer gave up its own records; the other pane kept its own.
        assert_eq!(a.bytes(), 200);
        assert_eq!(b.bytes(), 40);
        assert_eq!(b.first_sequence(), 9);
    }

    #[test]
    fn old_records_age_out() {
        let host = Arc::new(AtomicUsize::new(0));
        let mut j = Journal::new(
            1,
            Retention {
                per_pane_bytes: 1 << 20,
                host_bytes: 1 << 20,
                max_age: Duration::from_secs(10),
            },
            host,
        );
        let t0 = Instant::now();
        j.push(bytes(5), t0);
        j.push(bytes(5), t0 + Duration::from_secs(4));
        j.push(bytes(5), t0 + Duration::from_secs(12));
        assert_eq!(j.first_sequence(), 2);
        j.trim(t0 + Duration::from_secs(30));
        assert!(j.is_empty());
        assert_eq!(j.first_sequence(), 4);
    }
}
