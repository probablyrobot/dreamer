//! A client replica of a pane: an engine restored from a
//! daemon checkpoint, then the journal's records applied strictly in order.
//!
//! Rules, so a replica never silently diverges from the canonical mirror:
//! - A record at or below the last applied sequence is a duplicate and is
//!   ignored.
//! - A record above the next expected sequence is a gap: the replica stops
//!   applying and needs a resnapshot. Out-of-order delivery shows up as a
//!   gap, never as a reordering.
//! - Records or checkpoints for another subscription, connection epoch, or
//!   pane generation are stale and discarded.
//! - A new checkpoint is restored into a replacement engine and swapped in
//!   only when it restored. A failed or interrupted transfer keeps the
//!   previous view.
//!
//! [`Follower`] drives a replica over a daemon connection: subscribe,
//! resume after a reconnect, and resnapshot with bounded retries.

pub mod clipboard;
pub mod copy_mode;
pub mod decorations;
pub mod feed;
mod follow;
pub mod forward;
pub mod hosts;
pub mod keys;
pub mod search;
pub mod topology;
pub mod via;

use terminal_ghostty::core::GridSize;
use terminal_ghostty::core::family::{CANONICAL_CELL_PX, CONTINUATION_MAX_BYTES};
use terminal_ghostty::{Engine, EngineConfig};

pub use follow::{FollowError, FollowState, Follower, PaneMeta, RetryPolicy, Step};
pub use via::Via;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaError {
    /// A record skipped ahead of the next expected sequence.
    Gap { expected: u64, got: u64 },
    /// A checkpoint did not restore; the previous view is kept.
    Restore(String),
    /// A replay subscription does not continue from where this replica is.
    Position {
        replica: (u64, u64, u64),
        subscription: (u64, u64, u64),
    },
    /// Applying a record to the engine failed.
    Engine(String),
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicaError::Gap { expected, got } => {
                write!(
                    f,
                    "record {got} arrived when {expected} was next; a resnapshot is needed"
                )
            }
            ReplicaError::Restore(e) => write!(f, "checkpoint did not restore: {e}"),
            ReplicaError::Position {
                replica,
                subscription,
            } => write!(
                f,
                "replay starts at (epoch, generation, sequence) {subscription:?} but the replica is at {replica:?}"
            ),
            ReplicaError::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReplicaError {}

/// What happened to a batch or checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Records applied (duplicates are not counted).
    Applied(usize),
    /// Held until the checkpoint they follow arrives.
    Buffered(usize),
    /// From another subscription, epoch, or generation.
    Discarded(&'static str),
}

/// The stream a replica is currently following.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stream {
    subscription: u64,
    epoch: u64,
    generation: u64,
    /// The checkpoint this stream starts from, until it is restored.
    awaiting_checkpoint: Option<u64>,
}

#[derive(Debug)]
pub struct Replica {
    pub pane: u32,
    engine: Option<Engine>,
    /// What the engine holds: (epoch, generation, last applied sequence).
    epoch: u64,
    generation: u64,
    applied: u64,
    scrollback: usize,
    stream: Option<Stream>,
    early: Vec<api::TerminalRecord>,
    /// Set by a gap or failed restore; cleared by a restored checkpoint.
    needs_resnapshot: bool,
}

impl Replica {
    pub fn new(pane: u32) -> Replica {
        Replica {
            pane,
            engine: None,
            epoch: 0,
            generation: 0,
            applied: 0,
            scrollback: 0,
            stream: None,
            early: Vec::new(),
            needs_resnapshot: false,
        }
    }

    /// (epoch, generation, sequence) of what the engine holds, if anything:
    /// the resume point for the next subscription.
    pub fn position(&self) -> Option<(u64, u64, u64)> {
        self.engine
            .as_ref()
            .filter(|_| !self.needs_resnapshot)
            .map(|_| (self.epoch, self.generation, self.applied))
    }

    pub fn needs_resnapshot(&self) -> bool {
        self.needs_resnapshot
    }

    pub fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    /// The engine, for local view state (scrolling, selection). Never use it
    /// to change what the terminal holds: that comes only from the daemon.
    pub fn engine_mut(&mut self) -> Option<&mut Engine> {
        self.engine.as_mut()
    }

    /// The engine's current frame: shared unchanged rows, damage since the
    /// previous call, and the synchronized-output hold.
    pub fn frame(
        &mut self,
    ) -> Option<Result<std::sync::Arc<terminal_ghostty::core::FrameSnapshot>, String>> {
        let e = self.engine.as_mut()?;
        let t = perf::start();
        let frame = e.frame().map_err(|e| e.to_string());
        perf::finish(perf::Stage::Frame, t);
        Some(frame)
    }

    /// Move the local viewport through history. View state only: nothing
    /// is sent, and a restored checkpoint starts at the bottom again.
    pub fn scroll(&mut self, to: terminal_ghostty::Scroll) -> Result<(), String> {
        match self.engine.as_mut() {
            Some(e) => e.scroll_viewport(to).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// The replica's text, oldest line first (empty before a checkpoint).
    pub fn text(&self) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.screen_text().ok())
            .unwrap_or_default()
    }

    fn config(&self) -> EngineConfig {
        // The size comes from the snapshot; scrollback and continuation
        // limits are not serialized and must match the mirror's.
        let mut c = EngineConfig::new(GridSize { cols: 80, rows: 24 });
        c.scrollback_lines = Some(self.scrollback);
        c.scrollback_bytes = Some(usize::MAX);
        c.continuation_max_bytes = Some(CONTINUATION_MAX_BYTES);
        c
    }

    /// Start following a subscription the daemon accepted.
    pub fn begin(&mut self, s: &api::Subscription) -> Result<(), ReplicaError> {
        self.scrollback = s.scrollback_lines as usize;
        self.early.clear();
        let checkpoint = s.mode != api::SubscribeMode::Replay as i32;
        if !checkpoint {
            let here = (self.epoch, self.generation, self.applied);
            let there = (s.epoch, s.generation, s.sequence);
            if self.engine.is_none() || self.needs_resnapshot || here != there {
                return Err(ReplicaError::Position {
                    replica: here,
                    subscription: there,
                });
            }
        }
        self.stream = Some(Stream {
            subscription: s.id,
            epoch: s.epoch,
            generation: s.generation,
            awaiting_checkpoint: checkpoint.then_some(s.sequence),
        });
        Ok(())
    }

    /// Restore a checkpoint into a replacement engine and swap it in.
    pub fn checkpoint(&mut self, d: &api::CheckpointData) -> Result<Outcome, ReplicaError> {
        let Some(stream) = self.stream.filter(|s| s.subscription == d.subscription) else {
            return Ok(Outcome::Discarded("checkpoint for another subscription"));
        };
        if (d.epoch, d.generation) != (stream.epoch, stream.generation) {
            return Ok(Outcome::Discarded(
                "checkpoint from another epoch or generation",
            ));
        }
        let engine = match Engine::from_snapshot(&d.snapshot, &self.config()) {
            Ok(e) => e,
            Err(e) => {
                // Keep showing the previous view; it is no longer current.
                self.needs_resnapshot = true;
                return Err(ReplicaError::Restore(e.to_string()));
            }
        };
        self.engine = Some(engine);
        self.epoch = d.epoch;
        self.generation = d.generation;
        self.applied = d.sequence;
        self.needs_resnapshot = false;
        self.stream = Some(Stream {
            awaiting_checkpoint: None,
            ..stream
        });
        let early = std::mem::take(&mut self.early);
        let n = self.apply(early)?;
        Ok(Outcome::Applied(n))
    }

    /// Apply a batch of records.
    pub fn records(&mut self, b: &api::RecordBatch) -> Result<Outcome, ReplicaError> {
        let Some(stream) = self.stream.filter(|s| s.subscription == b.subscription) else {
            return Ok(Outcome::Discarded("records for another subscription"));
        };
        if b.epoch != stream.epoch {
            return Ok(Outcome::Discarded("records from another epoch"));
        }
        if b.generation != stream.generation {
            return Ok(Outcome::Discarded("records from another generation"));
        }
        if self.needs_resnapshot {
            return Ok(Outcome::Discarded("waiting for a resnapshot"));
        }
        if stream.awaiting_checkpoint.is_some() {
            self.early.extend(b.records.iter().cloned());
            return Ok(Outcome::Buffered(b.records.len()));
        }
        let parse = perf::start();
        let r = self.apply(b.records.clone()).map(Outcome::Applied);
        perf::finish(perf::Stage::ReplicaParse, parse);
        r
    }

    fn apply(&mut self, records: Vec<api::TerminalRecord>) -> Result<usize, ReplicaError> {
        let mut n = 0;
        for r in records {
            if r.sequence <= self.applied {
                continue; // duplicate, or already in the checkpoint
            }
            if r.sequence != self.applied + 1 {
                self.needs_resnapshot = true;
                return Err(ReplicaError::Gap {
                    expected: self.applied + 1,
                    got: r.sequence,
                });
            }
            let engine = self
                .engine
                .as_mut()
                .ok_or_else(|| ReplicaError::Engine("no engine before the checkpoint".into()))?;
            let result = match (r.cols, r.rows) {
                (Some(c), Some(rows)) => engine.resize(
                    GridSize {
                        cols: c as u16,
                        rows: rows as u16,
                    },
                    CANONICAL_CELL_PX,
                ),
                _ => engine.feed(&r.data).map(drop),
            };
            if let Err(e) = result {
                self.needs_resnapshot = true;
                return Err(ReplicaError::Engine(e.to_string()));
            }
            self.applied = r.sequence;
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_of(text: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        let mut c = EngineConfig::new(GridSize { cols, rows });
        c.scrollback_lines = Some(100);
        c.continuation_max_bytes = Some(CONTINUATION_MAX_BYTES);
        let mut e = Engine::new(c).expect("engine");
        e.feed(text).expect("feed");
        e.snapshot().expect("snapshot")
    }

    fn sub(id: u64, mode: api::SubscribeMode, sequence: u64) -> api::Subscription {
        api::Subscription {
            id,
            pane: 1,
            epoch: 7,
            generation: 2,
            mode: mode as i32,
            sequence,
            scrollback_lines: 100,
            ..api::Subscription::default()
        }
    }

    fn checkpoint(id: u64, sequence: u64, text: &[u8]) -> api::CheckpointData {
        api::CheckpointData {
            subscription: id,
            pane: 1,
            epoch: 7,
            generation: 2,
            sequence,
            snapshot: snapshot_of(text, 40, 5),
        }
    }

    fn rec(sequence: u64, data: &str) -> api::TerminalRecord {
        api::TerminalRecord {
            sequence,
            data: data.as_bytes().to_vec(),
            cols: None,
            rows: None,
        }
    }

    fn batch(id: u64, records: Vec<api::TerminalRecord>) -> api::RecordBatch {
        api::RecordBatch {
            subscription: id,
            pane: 1,
            epoch: 7,
            generation: 2,
            records,
            effects: Vec::new(),
        }
    }

    fn started() -> Replica {
        let mut r = Replica::new(1);
        r.begin(&sub(1, api::SubscribeMode::Checkpoint, 10))
            .expect("begin");
        r.checkpoint(&checkpoint(1, 10, b"base\r\n"))
            .expect("restore");
        r
    }

    #[test]
    fn records_apply_in_order_and_duplicates_are_ignored() {
        let mut r = started();
        assert_eq!(
            r.records(&batch(1, vec![rec(11, "a"), rec(12, "b")])),
            Ok(Outcome::Applied(2))
        );
        // The same records again, and an overlap: only 13 is new.
        assert_eq!(
            r.records(&batch(1, vec![rec(11, "a"), rec(12, "b")])),
            Ok(Outcome::Applied(0))
        );
        assert_eq!(
            r.records(&batch(1, vec![rec(12, "b"), rec(13, "c")])),
            Ok(Outcome::Applied(1))
        );
        assert_eq!(r.position(), Some((7, 2, 13)));
        assert!(r.text().contains("base\nabc"), "{:?}", r.text());
    }

    #[test]
    fn a_missing_record_stops_the_replica_until_a_resnapshot() {
        let mut r = started();
        let before = r.text();
        assert_eq!(
            r.records(&batch(1, vec![rec(12, "late")])),
            Err(ReplicaError::Gap {
                expected: 11,
                got: 12
            })
        );
        assert!(r.needs_resnapshot());
        assert_eq!(r.position(), None, "no resume point while behind");
        // Further records are not applied on top of the hole.
        assert_eq!(
            r.records(&batch(1, vec![rec(11, "x")])),
            Ok(Outcome::Discarded("waiting for a resnapshot"))
        );
        assert_eq!(r.text(), before);
        // A new checkpoint recovers.
        r.begin(&sub(2, api::SubscribeMode::Checkpoint, 20))
            .expect("begin");
        r.checkpoint(&checkpoint(2, 20, b"fresh\r\n"))
            .expect("restore");
        assert!(!r.needs_resnapshot());
        assert_eq!(
            r.records(&batch(2, vec![rec(21, "ok")])),
            Ok(Outcome::Applied(1))
        );
        assert!(r.text().contains("fresh\nok"));
    }

    #[test]
    fn out_of_order_delivery_is_a_gap_not_a_reordering() {
        let mut r = started();
        // 12 arrives before 11.
        assert!(matches!(
            r.records(&batch(1, vec![rec(12, "2")])),
            Err(ReplicaError::Gap { .. })
        ));
        assert!(r.needs_resnapshot());
        assert!(!r.text().contains('2'));
    }

    #[test]
    fn stale_epochs_generations_and_subscriptions_are_discarded() {
        let mut r = started();
        let mut b = batch(1, vec![rec(11, "stale")]);
        b.epoch = 6;
        assert_eq!(
            r.records(&b),
            Ok(Outcome::Discarded("records from another epoch"))
        );
        let mut b = batch(1, vec![rec(11, "stale")]);
        b.generation = 1;
        assert_eq!(
            r.records(&b),
            Ok(Outcome::Discarded("records from another generation"))
        );
        assert_eq!(
            r.records(&batch(99, vec![rec(11, "stale")])),
            Ok(Outcome::Discarded("records for another subscription"))
        );
        let mut c = checkpoint(1, 50, b"stale");
        c.epoch = 6;
        assert_eq!(
            r.checkpoint(&c),
            Ok(Outcome::Discarded(
                "checkpoint from another epoch or generation"
            ))
        );
        assert_eq!(r.position(), Some((7, 2, 10)));
        assert!(!r.text().contains("stale"));
    }

    #[test]
    fn records_before_their_checkpoint_wait_for_it() {
        let mut r = Replica::new(1);
        r.begin(&sub(1, api::SubscribeMode::Checkpoint, 10))
            .expect("begin");
        // Events overtake the bulk checkpoint: 9 and 10 are already in it.
        assert_eq!(
            r.records(&batch(
                1,
                vec![rec(9, "old"), rec(10, "old"), rec(11, "new")]
            )),
            Ok(Outcome::Buffered(3))
        );
        assert_eq!(
            r.checkpoint(&checkpoint(1, 10, b"cp\r\n")),
            Ok(Outcome::Applied(1))
        );
        assert!(r.text().contains("cp\nnew") && !r.text().contains("old"));
    }

    #[test]
    fn a_broken_checkpoint_keeps_the_previous_view() {
        let mut r = started();
        r.records(&batch(1, vec![rec(11, "kept")])).expect("apply");
        let before = r.text();
        r.begin(&sub(2, api::SubscribeMode::Checkpoint, 30))
            .expect("begin");
        let mut cut = checkpoint(2, 30, b"replacement");
        cut.snapshot.truncate(cut.snapshot.len() / 2);
        assert!(matches!(r.checkpoint(&cut), Err(ReplicaError::Restore(_))));
        assert_eq!(
            r.text(),
            before,
            "the interrupted transfer did not touch the view"
        );
        assert!(r.needs_resnapshot());
    }

    #[test]
    fn a_replay_must_continue_exactly_where_the_replica_is() {
        let mut r = started();
        assert!(r.begin(&sub(2, api::SubscribeMode::Replay, 10)).is_ok());
        let mut other_epoch = sub(3, api::SubscribeMode::Replay, 10);
        other_epoch.epoch = 8;
        assert!(matches!(
            r.begin(&other_epoch),
            Err(ReplicaError::Position { .. })
        ));
        assert!(matches!(
            r.begin(&sub(4, api::SubscribeMode::Replay, 9)),
            Err(ReplicaError::Position { .. })
        ));
    }

    #[test]
    fn resizes_apply_as_records() {
        let mut r = started();
        let resize = api::TerminalRecord {
            sequence: 11,
            data: Vec::new(),
            cols: Some(60),
            rows: Some(8),
        };
        assert_eq!(r.records(&batch(1, vec![resize])), Ok(Outcome::Applied(1)));
        let size = r.engine().expect("engine").size();
        assert_eq!((size.cols, size.rows), (60, 8));
    }
}
