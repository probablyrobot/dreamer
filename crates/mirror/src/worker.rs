//! VT owner workers (spec 5.2): a fixed pool of threads, each the single
//! owner of the engines of the panes assigned to it. Messages for one pane
//! always go to the same worker over one ordered channel, so a pane's
//! records are applied exactly in stream order.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;

use domain::{ConnectionEpoch, PaneGeneration, PaneKey, TmuxPaneId};
use terminal_ghostty::Engine;
use terminal_ghostty::core::GridSize;
use tmux_backend::bootstrap::{Captures, PaneState, restore_engine};
use tokio::sync::{mpsc, oneshot};

use crate::journal::{Journal, Retention};

/// Observation tier (spec 16.6). In the daemon both keep a live mirror and
/// a journal; Hot panes are visible in a client, Warm ones are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    Hot,
    #[default]
    Warm,
}

pub use terminal_ghostty::core::family::CANONICAL_CELL_PX as CELL_PX;

/// Why a subscriber's record stream ended, set before its channel closes.
pub mod end {
    pub const RUNNING: u8 = 0;
    /// The subscriber's queue was full: it fell behind.
    pub const LAGGED: u8 = 1;
    /// The mirror was dropped (pane closed or respawned).
    pub const MIRROR_GONE: u8 = 2;
    /// The worker stopped with its tmux connection.
    pub const CONNECTION_LOST: u8 = 3;
}

/// A subscriber's side of the stream: records after its starting point.
#[derive(Debug)]
pub struct Sink {
    pub records: mpsc::Sender<(u64, Record)>,
    pub ended: Arc<AtomicU8>,
}

/// How a subscription starts.
#[derive(Debug)]
pub enum Start {
    /// The engine at `sequence`; records continue from `sequence + 1`.
    Checkpoint {
        sequence: u64,
        snapshot: Vec<u8>,
        pause: std::time::Duration,
    },
    /// Retained records right after the resume point, then live records.
    Replay {
        after: u64,
        records: Vec<(u64, Record)>,
    },
}

#[derive(Debug)]
pub struct Started {
    /// Scrollback lines the mirror keeps.
    pub scrollback: usize,
    pub pane: PaneKey,
    pub epoch: ConnectionEpoch,
    pub generation: PaneGeneration,
    pub start: Start,
}

/// Input to encode against a pane's canonical engine.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Key {
        key: String,
        text: Option<String>,
        mods: u16,
        held: bool,
    },
    Text(String),
    Paste(Vec<u8>),
    Focus(bool),
}

/// One record that changes replicated terminal semantics.
#[derive(Debug, Clone)]
pub enum Record {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// What a read of a mirror returns. Always names the exact pane execution
/// and connection epoch it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub pane: PaneKey,
    pub generation: PaneGeneration,
    pub epoch: ConnectionEpoch,
    /// Records applied since the mirror was attached.
    pub sequence: u64,
    pub cols: u16,
    pub rows: u16,
    pub cursor: (u16, u16),
    /// Oldest first: `history` scrollback lines, then the screen.
    pub lines: Vec<String>,
    pub history: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStats {
    pub pane: PaneKey,
    pub generation: PaneGeneration,
    pub epoch: ConnectionEpoch,
    pub sequence: u64,
    pub ingested: u64,
    pub cols: u16,
    pub rows: u16,
    pub error: Option<String>,
    pub tier: Tier,
    pub journal_bytes: usize,
    pub journal_records: usize,
    /// Oldest retained record; `sequence + 1` when nothing is retained.
    pub journal_first: u64,
}

#[derive(Debug)]
pub enum WorkerMsg {
    Init {
        pane: PaneKey,
        epoch: ConnectionEpoch,
        generation: PaneGeneration,
        state: Box<PaneState>,
        captures: Box<Captures>,
        continuation: Vec<Record>,
    },
    Apply {
        pane: TmuxPaneId,
        generation: PaneGeneration,
        record: Record,
    },
    Drop {
        pane: TmuxPaneId,
    },
    SetTier {
        pane: TmuxPaneId,
        tier: Tier,
        reply: oneshot::Sender<bool>,
    },
    /// Start a subscription on the pane's owner: decide replay or
    /// checkpoint and register the sink before any later record is applied.
    Subscribe {
        pane: TmuxPaneId,
        resume: Option<(PaneGeneration, u64)>,
        sink: Sink,
        reply: oneshot::Sender<Result<Started, String>>,
    },
    /// Encode input on the pane's owner, against its current modes.
    Encode {
        pane: TmuxPaneId,
        generation: Option<PaneGeneration>,
        event: InputEvent,
        reply: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    Read {
        pane: TmuxPaneId,
        history: usize,
        reply: oneshot::Sender<Option<Result<ReadResult, String>>>,
    },
    Stats {
        reply: oneshot::Sender<Vec<LiveStats>>,
    },
}

struct Mirror {
    pane: PaneKey,
    epoch: ConnectionEpoch,
    generation: PaneGeneration,
    engine: Engine,
    sequence: u64,
    ingested: u64,
    error: Option<String>,
    tier: Tier,
    journal: Journal,
    subscribers: Vec<Sink>,
    scrollback: usize,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        for s in &self.subscribers {
            let _ = s.ended.compare_exchange(
                end::RUNNING,
                end::MIRROR_GONE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }
}

impl Mirror {
    fn subscribe(
        &mut self,
        resume: Option<(PaneGeneration, u64)>,
        sink: Sink,
    ) -> Result<Started, String> {
        if let Some(e) = &self.error {
            return Err(format!("the mirror failed: {e}"));
        }
        let replay = resume
            .filter(|(g, _)| *g == self.generation)
            .and_then(|(_, after)| {
                self.journal
                    .since(after + 1)
                    .map(|it| (after, it.map(|(s, r)| (s, r.clone())).collect::<Vec<_>>()))
            });
        let start = match replay {
            Some((after, records)) => Start::Replay { after, records },
            None => {
                let t = Instant::now();
                let snapshot = self
                    .engine
                    .snapshot()
                    .map_err(|e| format!("snapshot: {e}"))?;
                Start::Checkpoint {
                    sequence: self.sequence,
                    snapshot,
                    pause: t.elapsed(),
                }
            }
        };
        self.subscribers.push(sink);
        Ok(Started {
            scrollback: self.scrollback,
            pane: self.pane.clone(),
            epoch: self.epoch,
            generation: self.generation,
            start,
        })
    }

    /// Hand a committed record to every subscriber. One that is full has
    /// fallen behind: it is ended as lagged, never skipped silently.
    fn publish(&mut self, seq: u64, record: &Record) {
        self.subscribers
            .retain(|s| match s.records.try_send((seq, record.clone())) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    s.ended.store(end::LAGGED, Ordering::SeqCst);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }

    fn stats(&self) -> LiveStats {
        let s = self.engine.size();
        LiveStats {
            pane: self.pane.clone(),
            generation: self.generation,
            epoch: self.epoch,
            sequence: self.sequence,
            ingested: self.ingested,
            cols: s.cols,
            rows: s.rows,
            error: self.error.clone(),
            tier: self.tier,
            journal_bytes: self.journal.bytes(),
            journal_records: self.journal.len(),
            journal_first: self.journal.first_sequence(),
        }
    }

    /// Apply one record to the engine and, once it is committed, journal it.
    fn apply(&mut self, record: Record) {
        if self.error.is_some() {
            return;
        }
        let r = match &record {
            Record::Bytes(b) => {
                self.ingested += b.len() as u64;
                // Effects and replies are not acted on here; the reply policy
                // is a later ticket. Mirrors never answer the application.
                self.engine.feed(b).map(drop)
            }
            Record::Resize { cols, rows } => self.engine.resize(
                GridSize {
                    cols: *cols,
                    rows: *rows,
                },
                CELL_PX,
            ),
        };
        match r {
            Ok(()) => {
                if !self.subscribers.is_empty() {
                    self.publish(self.journal.next_sequence(), &record);
                }
                self.sequence = self.journal.push(record, Instant::now());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn read(&self, history: usize) -> Result<ReadResult, String> {
        let size = self.engine.size();
        let text = self.engine.screen_text().map_err(|e| e.to_string())?;
        let all: Vec<&str> = text.split('\n').collect();
        let rows = size.rows as usize;
        let take = (rows + history).min(all.len());
        let lines: Vec<String> = all[all.len() - take..]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let cursor = self.engine.cursor_position().map_err(|e| e.to_string())?;
        Ok(ReadResult {
            pane: self.pane.clone(),
            generation: self.generation,
            epoch: self.epoch,
            sequence: self.sequence,
            cols: size.cols,
            rows: size.rows,
            cursor,
            history: take.saturating_sub(rows),
            lines,
        })
    }
}

fn encode(engine: &Engine, event: InputEvent) -> Result<Vec<u8>, String> {
    use terminal_ghostty::input::{KeyAction, Mods, key_code};
    match event {
        InputEvent::Key {
            key,
            text,
            mods,
            held,
        } => {
            let code = key_code(&key)
                .ok_or_else(|| format!("unknown key `{key}` (use KeyboardEvent.code names)"))?;
            let action = if held {
                KeyAction::Repeat
            } else {
                KeyAction::Press
            };
            engine
                .encode_key(code, Mods(mods), text.as_deref(), action)
                .map_err(|e| e.to_string())
        }
        // Committed text goes to the application as typed.
        InputEvent::Text(t) => Ok(t.into_bytes()),
        InputEvent::Paste(data) => engine.encode_paste(&data).map_err(|e| e.to_string()),
        InputEvent::Focus(gained) => engine.encode_focus(gained).map_err(|e| e.to_string()),
    }
}

pub fn spawn(
    index: usize,
    capacity: usize,
    retention: Retention,
    host_bytes: Arc<AtomicUsize>,
) -> std::io::Result<mpsc::Sender<WorkerMsg>> {
    let (tx, mut rx) = mpsc::channel::<WorkerMsg>(capacity);
    std::thread::Builder::new()
        .name(format!("dreamer-vt-{index}"))
        .spawn(move || {
            let mut mirrors: HashMap<TmuxPaneId, Mirror> = HashMap::new();
            // Panes whose engine could not be restored, reported in stats.
            let mut failed: HashMap<TmuxPaneId, LiveStats> = HashMap::new();
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    WorkerMsg::Init {
                        pane,
                        epoch,
                        generation,
                        state,
                        captures,
                        continuation,
                    } => {
                        let id = pane.pane;
                        failed.remove(&id);
                        let engine = match restore_engine(&state, &captures, &[]) {
                            Ok(e) => e,
                            Err(e) => {
                                failed.insert(
                                    id,
                                    LiveStats {
                                        pane,
                                        generation,
                                        epoch,
                                        sequence: 0,
                                        ingested: 0,
                                        cols: state.size().cols,
                                        rows: state.size().rows,
                                        error: Some(format!("restore failed: {e}")),
                                        tier: Tier::Warm,
                                        journal_bytes: 0,
                                        journal_records: 0,
                                        journal_first: 1,
                                    },
                                );
                                continue;
                            }
                        };
                        let mut m = Mirror {
                            pane,
                            epoch,
                            generation,
                            engine,
                            sequence: 0,
                            ingested: 0,
                            error: None,
                            tier: Tier::Warm,
                            journal: Journal::new(1, retention, host_bytes.clone()),
                            subscribers: Vec::new(),
                            scrollback: state.history_limit as usize,
                        };
                        for r in continuation {
                            m.apply(r);
                        }
                        mirrors.insert(id, m);
                    }
                    WorkerMsg::Apply {
                        pane,
                        generation,
                        record,
                    } => {
                        // A record for an older execution of the pane is stale.
                        if let Some(m) = mirrors
                            .get_mut(&pane)
                            .filter(|m| m.generation == generation)
                        {
                            m.apply(record);
                        }
                    }
                    WorkerMsg::Drop { pane } => {
                        mirrors.remove(&pane);
                        failed.remove(&pane);
                    }
                    WorkerMsg::Subscribe {
                        pane,
                        resume,
                        sink,
                        reply,
                    } => {
                        let r = match mirrors.get_mut(&pane) {
                            Some(m) => m.subscribe(resume, sink),
                            None => Err(format!("pane %{} has no live mirror", pane.0)),
                        };
                        let _ = reply.send(r);
                    }
                    WorkerMsg::Encode {
                        pane,
                        generation,
                        event,
                        reply,
                    } => {
                        let r = match mirrors.get(&pane) {
                            None => Err(format!("pane %{} has no live mirror", pane.0)),
                            Some(m) if generation.is_some_and(|g| g != m.generation) => Err(format!(
                                "pane %{} now runs generation {}, not {}; its process was replaced",
                                pane.0,
                                m.generation.0,
                                generation.map_or(0, |g| g.0)
                            )),
                            Some(m) => encode(&m.engine, event),
                        };
                        let _ = reply.send(r);
                    }
                    WorkerMsg::SetTier { pane, tier, reply } => {
                        let found = mirrors.get_mut(&pane).map(|m| m.tier = tier).is_some();
                        let _ = reply.send(found);
                    }
                    WorkerMsg::Read {
                        pane,
                        history,
                        reply,
                    } => {
                        let _ = reply.send(mirrors.get(&pane).map(|m| m.read(history)));
                    }
                    WorkerMsg::Stats { reply } => {
                        let now = Instant::now();
                        for m in mirrors.values_mut() {
                            // Idle panes age out here as well as on append.
                            m.journal.trim(now);
                        }
                        let _ = reply.send(
                            mirrors
                                .values()
                                .map(Mirror::stats)
                                .chain(failed.values().cloned())
                                .collect(),
                        );
                    }
                }
            }
            // The router and host are gone with their connection.
            for m in mirrors.values() {
                for sub in &m.subscribers {
                    sub.ended.store(end::CONNECTION_LOST, Ordering::SeqCst);
                }
            }
        })?;
    Ok(tx)
}
