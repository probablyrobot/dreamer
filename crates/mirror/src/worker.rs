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
use terminal_ghostty::input::{MouseAction, MouseButton};
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
    /// The pane's output outran the mirror and it was rebuilt from a fresh
    /// capture.
    pub const REATTACHED: u8 = 4;
}

/// A worker's two lanes. Output goes on the bounded data lane, in order.
/// Requests that must not wait behind a pane's backlog (a new attach,
/// input encoding, reads, subscriptions, statistics) go on the urgent
/// lane, which the worker always drains first.
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    data: mpsc::Sender<WorkerMsg>,
    urgent: mpsc::UnboundedSender<WorkerMsg>,
}

/// Why a message did not go on the data lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneError {
    /// The lane is full: the pane's output is ahead of its mirror.
    Full,
    /// The worker has stopped.
    Gone,
}

/// The worker has stopped (its connection ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerGone;

impl WorkerHandle {
    /// A slot on the data lane, without waiting: send into it to keep
    /// order with the pane's output. Nothing is lost when there is none.
    pub fn try_reserve(&self) -> Result<mpsc::Permit<'_, WorkerMsg>, LaneError> {
        self.data.try_reserve().map_err(|e| match e {
            mpsc::error::TrySendError::Full(()) => LaneError::Full,
            mpsc::error::TrySendError::Closed(()) => LaneError::Gone,
        })
    }

    /// Ahead of any queued output.
    pub fn urgent(&self, m: WorkerMsg) -> Result<(), WorkerGone> {
        self.urgent.send(m).map_err(|_| WorkerGone)?;
        // Wake a worker waiting on an empty data lane; a full lane means it
        // is busy and checks the urgent lane before its next message.
        let _ = self.data.try_send(WorkerMsg::Wake);
        Ok(())
    }
}

/// What applying a live record did besides changing the screen. Only
/// subscribers live at the time see it; it is never journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// Increasing per mirror.
    pub id: u64,
    pub bells: u32,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Text the program wrote to the clipboard; kept only in the copy sent
    /// to the pane's focused client.
    pub clipboard: Option<Vec<u8>>,
}

/// A record on its way to a subscriber, with the live effects it produced
/// (empty for replayed records).
pub type Live = (u64, Record, Option<Effect>);

/// A subscriber's side of the stream: records after its starting point.
#[derive(Debug)]
pub struct Sink {
    pub records: mpsc::Sender<Live>,
    /// The subscribing client's id, if it gave one.
    pub client: Option<u64>,
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
    /// Current title and working directory: state, sent with the start.
    pub title: Option<String>,
    pub cwd: Option<String>,
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
    Mouse {
        action: MouseAction,
        button: Option<MouseButton>,
        col: u16,
        row: u16,
        mods: u16,
        buttons_held: bool,
    },
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
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub reattaches: u32,
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
        attach: u32,
    },
    Apply {
        pane: TmuxPaneId,
        generation: PaneGeneration,
        /// Which attach of the pane this output follows; output from an
        /// earlier attach (left queued when the pane was re-attached) is
        /// already part of the newer capture and is dropped.
        attach: u32,
        record: Record,
    },
    /// Nothing to do; makes a waiting worker look at its urgent lane.
    Wake,
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
        client: Option<u64>,
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
    title: Option<String>,
    cwd: Option<String>,
    effects: u64,
    /// The client whose window last gained focus on this pane.
    focused: Option<u64>,
    /// The attach this mirror was built from, and how many times the pane
    /// was re-attached because its output outran the mirror.
    attach: u32,
    reattaches: u32,
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
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            pane: self.pane.clone(),
            epoch: self.epoch,
            generation: self.generation,
            start,
        })
    }

    /// Hand a committed record to every subscriber. One that is full has
    /// fallen behind: it is ended as lagged, never skipped silently.
    fn publish(&mut self, seq: u64, record: &Record, effect: Option<Effect>) {
        let focused = self.focused;
        self.subscribers.retain(|s| {
            // A clipboard write goes to exactly one client: the focused one.
            let effect = effect.clone().map(|mut e| {
                if focused.is_none() || s.client != focused {
                    e.clipboard = None;
                }
                e
            });
            match s.records.try_send((seq, record.clone(), effect)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    s.ended.store(end::LAGGED, Ordering::SeqCst);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
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
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            reattaches: self.reattaches,
        }
    }

    /// Apply one record to the engine and, once it is committed, journal it.
    fn apply(&mut self, record: Record) {
        if self.error.is_some() {
            return;
        }
        let mut effect = None;
        let r = match &record {
            Record::Bytes(b) => {
                self.ingested += b.len() as u64;
                // Mirrors never answer the application (tmux owns replies).
                // Title and cwd become state; they and bells are live
                // effects for subscribers present now, never journaled.
                self.engine.feed(b).map(|fx| {
                    if fx.title.is_some() {
                        self.title.clone_from(&fx.title);
                    }
                    if fx.pwd.is_some() {
                        self.cwd.clone_from(&fx.pwd);
                    }
                    // The last write wins when one ingestion holds several.
                    let clipboard = fx.clipboard.into_iter().last();
                    if fx.bells > 0 || fx.title.is_some() || fx.pwd.is_some() || clipboard.is_some()
                    {
                        self.effects += 1;
                        effect = Some(Effect {
                            id: self.effects,
                            bells: fx.bells,
                            title: fx.title,
                            cwd: fx.pwd,
                            clipboard,
                        });
                    }
                })
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
                    self.publish(self.journal.next_sequence(), &record, effect);
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
        InputEvent::Mouse {
            action,
            button,
            col,
            row,
            mods,
            buttons_held,
        } => engine
            .encode_mouse(action, button, col, row, Mods(mods), buttons_held)
            .map_err(|e| e.to_string()),
    }
}

pub fn spawn(
    index: usize,
    capacity: usize,
    retention: Retention,
    terminal: crate::TerminalDefaults,
    host_bytes: Arc<AtomicUsize>,
) -> std::io::Result<WorkerHandle> {
    let (tx, mut rx) = mpsc::channel::<WorkerMsg>(capacity);
    let (urgent_tx, mut urgent) = mpsc::unbounded_channel::<WorkerMsg>();
    // Tests: make one pane's output slow to apply, to outrun its mirror.
    let slow: Option<(u32, std::time::Duration)> =
        std::env::var("DREAMER_TEST_SLOW_PANE").ok().and_then(|v| {
            let (pane, us) = v.split_once(':')?;
            Some((
                pane.parse().ok()?,
                std::time::Duration::from_micros(us.parse().ok()?),
            ))
        });
    std::thread::Builder::new()
        .name(format!("dreamer-vt-{index}"))
        .spawn(move || {
            let mut mirrors: HashMap<TmuxPaneId, Mirror> = HashMap::new();
            // Panes whose engine could not be restored, reported in stats.
            let mut failed: HashMap<TmuxPaneId, LiveStats> = HashMap::new();
            loop {
                let msg = match urgent.try_recv() {
                    Ok(m) => m,
                    Err(_) => match rx.blocking_recv() {
                        Some(m) => m,
                        None => break,
                    },
                };
                match msg {
                    WorkerMsg::Wake => {}
                    WorkerMsg::Init {
                        pane,
                        epoch,
                        generation,
                        state,
                        captures,
                        continuation,
                        attach,
                    } => {
                        let id = pane.pane;
                        // A re-attach of the same execution replaces a
                        // mirror that fell behind: its subscribers start
                        // over, and the new journal begins past a gap so no
                        // resume point of the old one can be spliced on.
                        let previous = mirrors
                            .remove(&id)
                            .filter(|m| m.generation == generation)
                            .map(|old| {
                                for s in &old.subscribers {
                                    let _ = s.ended.compare_exchange(
                                        end::RUNNING,
                                        end::REATTACHED,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    );
                                }
                                (old.sequence + 2, old.reattaches + 1, old.title.clone(), old.cwd.clone())
                            });
                        let (base, reattaches, title, cwd) = previous.unwrap_or((1, 0, None, None));
                        failed.remove(&id);
                        let engine = match restore_engine(&state, &captures, &[]).and_then(
                            |mut e| {
                                e.set_scheme(terminal.scheme)?;
                                e.set_default_cursor(terminal.cursor, terminal.blink)?;
                                Ok(e)
                            },
                        ) {
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
                                        title: None,
                                        cwd: None,
                                        reattaches: 0,
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
                            sequence: base - 1,
                            ingested: 0,
                            error: None,
                            tier: Tier::Warm,
                            journal: Journal::new(base, retention, host_bytes.clone()),
                            subscribers: Vec::new(),
                            scrollback: state.history_limit as usize,
                            // Only what the program sets after attach; the
                            // tmux pane title is in the topology.
                            title,
                            cwd,
                            effects: 0,
                            focused: None,
                            attach,
                            reattaches,
                        };
                        // Clipboard writes count only from live output, never
                        // from what bootstrap restored.
                        if let Err(e) = m.engine.set_capture_clipboard(true) {
                            m.error = Some(format!("clipboard capture: {e}"));
                        }
                        for r in continuation {
                            m.apply(r);
                        }
                        mirrors.insert(id, m);
                    }
                    WorkerMsg::Apply {
                        pane,
                        generation,
                        attach,
                        record,
                    } => {
                        // A record for an older execution of the pane, or
                        // from before its latest attach, is stale.
                        if let Some(m) = mirrors
                            .get_mut(&pane)
                            .filter(|m| m.generation == generation && m.attach == attach)
                        {
                            if let Some((p, delay)) = slow
                                && p == pane.0
                            {
                                std::thread::sleep(delay);
                            }
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
                        client,
                        reply,
                    } => {
                        // Focus decides which client clipboard writes go to.
                        if let (InputEvent::Focus(gained), Some(c), Some(m)) =
                            (&event, client, mirrors.get_mut(&pane))
                        {
                            if *gained {
                                m.focused = Some(c);
                            } else if m.focused == Some(c) {
                                m.focused = None;
                            }
                        }
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
    Ok(WorkerHandle {
        data: tx,
        urgent: urgent_tx,
    })
}
