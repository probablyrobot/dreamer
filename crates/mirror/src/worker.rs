//! VT owner workers (spec 5.2): a fixed pool of threads, each the single
//! owner of the engines of the panes assigned to it. Messages for one pane
//! always go to the same worker over one ordered channel, so a pane's
//! records are applied exactly in stream order.

use std::collections::HashMap;

use domain::{ConnectionEpoch, PaneGeneration, PaneKey, TmuxPaneId};
use terminal_ghostty::Engine;
use terminal_ghostty::core::GridSize;
use tmux_backend::bootstrap::{Captures, PaneState, restore_engine};
use tokio::sync::{mpsc, oneshot};

/// Canonical cell size in pixels until clients negotiate one (spec 11.8).
pub const CELL_PX: (u32, u32) = (10, 20);

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
}

impl Mirror {
    fn apply(&mut self, record: &Record) {
        if self.error.is_some() {
            return;
        }
        let r = match record {
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
            Ok(()) => self.sequence += 1,
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

pub fn spawn(index: usize, capacity: usize) -> std::io::Result<mpsc::Sender<WorkerMsg>> {
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
                        };
                        for r in &continuation {
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
                            m.apply(&record);
                        }
                    }
                    WorkerMsg::Drop { pane } => {
                        mirrors.remove(&pane);
                        failed.remove(&pane);
                    }
                    WorkerMsg::Read {
                        pane,
                        history,
                        reply,
                    } => {
                        let _ = reply.send(mirrors.get(&pane).map(|m| m.read(history)));
                    }
                    WorkerMsg::Stats { reply } => {
                        let _ = reply.send(
                            mirrors
                                .values()
                                .map(|m| {
                                    let s = m.engine.size();
                                    LiveStats {
                                        pane: m.pane.clone(),
                                        generation: m.generation,
                                        epoch: m.epoch,
                                        sequence: m.sequence,
                                        ingested: m.ingested,
                                        cols: s.cols,
                                        rows: s.rows,
                                        error: m.error.clone(),
                                    }
                                })
                                .chain(failed.values().cloned())
                                .collect(),
                        );
                    }
                }
            }
        })?;
    Ok(tx)
}
