//! VT owner workers: a fixed pool of threads, each the single
//! owner of the engines of the panes assigned to it. Messages for one pane
//! always go to the same worker over one ordered channel, so a pane's
//! records are applied exactly in stream order.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;

use domain::{ConnectionEpoch, PaneGeneration, PaneKey, TmuxPaneId};
use terminal_ghostty::Engine;
pub use terminal_ghostty::core::ScreenFrame;
use terminal_ghostty::core::{Colors, GridSize, ImagePlacement, Row};
use terminal_ghostty::input::{MouseAction, MouseButton};
use tmux_backend::bootstrap::{Captures, PaneState, restore_engine};
use tokio::sync::{mpsc, oneshot};

use crate::journal::{Journal, Retention};

/// Observation tier of a pane that has a mirror. In the daemon
/// both keep a live mirror and a journal; Hot panes are visible in a
/// client, Warm ones are not. A Cold pane has no mirror at all (see
/// `MirrorState::Cold`).
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

    /// Messages waiting on the data lane.
    pub fn queued(&self) -> usize {
        self.data.max_capacity() - self.data.capacity()
    }

    /// The data lane's capacity.
    pub fn capacity(&self) -> usize {
        self.data.max_capacity()
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
    /// Command steps the shell reported (shell integration), in order.
    pub commands: Vec<terminal_ghostty::core::CommandMark>,
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
    /// Diagnostics: marked when a record is handed to this subscriber, for
    /// whoever forwards the records to time the transfer. Marked only while
    /// diagnostics are on.
    pub stamp: Option<Arc<perf::Stamp>>,
    /// A view that may be switched to screen frames: what names it to the
    /// owner, and how the owner says the screen changed.
    pub frames: Option<Arc<FrameLink>>,
}

/// A view subscription's link to its pane's owner for screen frames. While
/// the subscription is on frames, the owner sends it no records and wakes
/// `changed` instead whenever it applies one.
#[derive(Debug, Default)]
pub struct FrameLink {
    pub changed: tokio::sync::Notify,
}

/// Where a subscription is after a switch of feed.
#[derive(Debug)]
pub enum Switched {
    /// Back on records: the engine at `sequence`; records continue from
    /// `sequence + 1`.
    Stream { sequence: u64, snapshot: Vec<u8> },
    /// On frames: a full frame; no records follow.
    Frames(ScreenFrame),
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
    /// A full screen frame; frames follow instead of records.
    Frames(ScreenFrame),
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

/// What a tap's text level needs from a mirror: how much scrollback there
/// is, which screen is active, and the newest lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailResult {
    pub generation: PaneGeneration,
    pub sequence: u64,
    /// Lines of scrollback above the active screen.
    pub scrollback: usize,
    pub alt_screen: bool,
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// The cursor's row on the active screen.
    pub cursor_row: u16,
    /// The newest `history` scrollback lines (or fewer), oldest first.
    pub history: Vec<String>,
    /// The active screen's rows.
    pub screen: Vec<String>,
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
    /// Subscribers following the pane now: views, taps, and filters.
    pub followers: usize,
    /// How long since the pane last wrote anything (or since the mirror
    /// started, if it has written nothing).
    pub quiet: std::time::Duration,
    /// Records waiting in the fullest subscriber queue, and a subscriber
    /// queue's capacity (0 with no subscribers).
    pub subscriber_queued: usize,
    pub subscriber_capacity: usize,
    /// What the mirror is estimated to hold, in bytes: its screen and
    /// scrollback, and its journal. What going Cold gives back.
    pub memory: usize,
}

/// A mirror line's estimated cost: 8 bytes a cell and 64 a line, which is
/// the measured 1 KiB per 120-column line of history
/// (`docs/performance.md`). Ghostty keeps its pages outside the allocator,
/// so this is an estimate, not a reading.
pub fn line_bytes(cols: u16) -> usize {
    usize::from(cols) * 8 + 64
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
        /// Diagnostics only: when the record was decoded from tmux's output.
        at: Option<Instant>,
    },
    /// Nothing to do; makes a waiting worker look at its urgent lane.
    Wake,
    /// The tmux control connection ended: stop, ending every subscription
    /// as connection lost. Views, taps, and filters still hold the host,
    /// so the worker would otherwise outlive its connection and keep them
    /// on a mirror nothing feeds.
    Lost,
    Drop {
        pane: TmuxPaneId,
    },
    /// Drop the mirror for the Cold tier, unless anything follows it (a
    /// view, a tap, a filter): then it stays, and the reply says why.
    Retire {
        pane: TmuxPaneId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetTier {
        pane: TmuxPaneId,
        tier: Tier,
        reply: oneshot::Sender<bool>,
    },
    /// New terminal defaults (the configuration changed): apply them to
    /// every mirror, and end its subscriptions as re-attached so every view
    /// takes a checkpoint with the new colors.
    SetDefaults {
        terminal: crate::TerminalDefaults,
        reply: oneshot::Sender<()>,
    },
    /// Start a subscription on the pane's owner: decide replay or
    /// checkpoint and register the sink before any later record is applied.
    Subscribe {
        pane: TmuxPaneId,
        resume: Option<(PaneGeneration, u64)>,
        sink: Sink,
        /// Start on screen frames (the sink must have a frame link).
        frames: bool,
        reply: oneshot::Sender<Result<Started, String>>,
    },
    /// Switch the subscription with this link to frames or back to records,
    /// between two records.
    SetFeed {
        pane: TmuxPaneId,
        link: Arc<FrameLink>,
        frames: bool,
        reply: oneshot::Sender<Result<Switched, String>>,
    },
    /// The next frame for the subscription with this link, if it is on
    /// frames and the mirror moved on since its last one.
    Frame {
        pane: TmuxPaneId,
        link: Arc<FrameLink>,
        reply: oneshot::Sender<Result<Option<ScreenFrame>, String>>,
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
    Tail {
        pane: TmuxPaneId,
        history: usize,
        /// Answer `Ok(None)` when the mirror is still at this sequence.
        after: Option<u64>,
        reply: oneshot::Sender<Option<Result<Option<TailResult>, String>>>,
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
    subscribers: Vec<Sub>,
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
    /// The sequence when the default colors last changed: a view resuming
    /// from at or before it takes a checkpoint, not a replay without them.
    colors_from: u64,
    /// When the pane last wrote anything.
    last_output: Instant,
}

/// A subscriber, and its screen while it is on frames.
struct Sub {
    sink: Sink,
    /// Frames sent so far, across every period on frames.
    sent: u64,
    screen: Option<Screen>,
}

impl Sub {
    fn is(&self, link: &Arc<FrameLink>) -> bool {
        self.sink
            .frames
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(l, link))
    }
}

/// What a subscription on frames was last sent; a frame carries what
/// differs from it.
#[derive(Default)]
struct Screen {
    sequence: u64,
    rows: Vec<Arc<Row>>,
    colors: Option<Arc<Colors>>,
    images: Option<Arc<[ImagePlacement]>>,
    pixels: std::collections::HashSet<(u32, u64)>,
    title: Option<String>,
}

impl std::ops::Deref for Sub {
    type Target = Sink;
    fn deref(&self) -> &Sink {
        &self.sink
    }
}

/// Build the next frame for `sub` from `engine` at `sequence`: `None` when
/// it is not on frames, or nothing happened since its last frame.
fn next_frame(
    engine: &mut Engine,
    sequence: u64,
    title: &Option<String>,
    sub: &mut Sub,
    full: bool,
) -> Result<Option<ScreenFrame>, String> {
    let Some(screen) = sub.screen.as_mut() else {
        return Ok(None);
    };
    if !full && screen.sequence == sequence {
        return Ok(None);
    }
    let f = engine.frame().map_err(|e| format!("frame: {e}"))?;
    let modes = engine.input_modes().map_err(|e| format!("modes: {e}"))?;
    // A new size redraws everything, so it is sent as a full frame.
    let full = full
        || screen.rows.len() != f.rows.len()
        || screen.rows.first().map(|r| r.cells.len()) != f.rows.first().map(|r| r.cells.len());
    if full {
        *screen = Screen::default();
    }
    let rows = f
        .rows
        .iter()
        .enumerate()
        .filter(|(i, row)| {
            full || screen
                .rows
                .get(*i)
                .is_none_or(|old| !Arc::ptr_eq(old, row) && **old != ***row)
        })
        .map(|(i, row)| (i as u16, Arc::clone(row)))
        .collect();
    let colors = (full
        || screen
            .colors
            .as_ref()
            .is_none_or(|c| !Arc::ptr_eq(c, &f.colors) && **c != *f.colors))
    .then(|| Arc::clone(&f.colors));
    let images = (full
        || screen
            .images
            .as_ref()
            .is_none_or(|i| !Arc::ptr_eq(i, &f.images) && **i != *f.images))
    .then(|| Arc::clone(&f.images));
    let mut pixels = Vec::new();
    if let Some(placed) = &images {
        // The client keeps the pixels of placed images only, so pixels are
        // sent again for an image that comes back.
        let mut now = std::collections::HashSet::new();
        for p in placed.iter() {
            let key = (p.image.id, p.image.generation);
            if now.insert(key) && !screen.pixels.contains(&key) {
                pixels.push(Arc::clone(&p.image));
            }
        }
        screen.pixels = now;
    }
    let new_title = (full || screen.title != *title)
        .then(|| title.clone())
        .flatten();
    screen.sequence = sequence;
    screen.rows.clone_from(&f.rows);
    if colors.is_some() {
        screen.colors = Some(Arc::clone(&f.colors));
    }
    if images.is_some() {
        screen.images = Some(Arc::clone(&f.images));
    }
    screen.title.clone_from(title);
    sub.sent += 1;
    Ok(Some(ScreenFrame {
        number: sub.sent,
        full,
        sequence,
        size: f.size,
        view: f.view,
        rows,
        cursor: f.cursor,
        modes,
        title: new_title,
        colors,
        images,
        pixels,
        render_hold: f.render_hold,
    }))
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
        frames: bool,
    ) -> Result<Started, String> {
        if let Some(e) = &self.error {
            return Err(format!("the mirror failed: {e}"));
        }
        if frames {
            if sink.frames.is_none() {
                return Err("a subscription on frames needs a frame link".into());
            }
            let mut sub = Sub {
                sink,
                sent: 0,
                screen: Some(Screen::default()),
            };
            let frame = next_frame(&mut self.engine, self.sequence, &self.title, &mut sub, true)?
                .ok_or("no frame")?;
            self.subscribers.push(sub);
            return Ok(self.started(Start::Frames(frame)));
        }
        let replay = resume
            .filter(|(g, after)| *g == self.generation && *after > self.colors_from)
            .and_then(|(_, after)| {
                self.journal
                    .since(after + 1)
                    .map(|it| (after, it.map(|(s, r)| (s, r.clone())).collect::<Vec<_>>()))
            });
        let start = match replay {
            Some((after, records)) => {
                perf::count(perf::Counter::Replays, 1);
                Start::Replay { after, records }
            }
            None => {
                let t = Instant::now();
                let snapshot = self
                    .engine
                    .snapshot()
                    .map_err(|e| format!("snapshot: {e}"))?;
                perf::count(perf::Counter::Checkpoints, 1);
                perf::count(perf::Counter::CheckpointBytes, snapshot.len() as u64);
                Start::Checkpoint {
                    sequence: self.sequence,
                    snapshot,
                    pause: t.elapsed(),
                }
            }
        };
        self.subscribers.push(Sub {
            sink,
            sent: 0,
            screen: None,
        });
        Ok(self.started(start))
    }

    fn started(&self, start: Start) -> Started {
        Started {
            scrollback: self.scrollback,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            pane: self.pane.clone(),
            epoch: self.epoch,
            generation: self.generation,
            start,
        }
    }

    /// Move the subscriber with `link` to frames (a full frame now) or back
    /// to records (a checkpoint now, records from the next one on).
    fn set_feed(&mut self, link: &Arc<FrameLink>, frames: bool) -> Result<Switched, String> {
        if let Some(e) = &self.error {
            return Err(format!("the mirror failed: {e}"));
        }
        let sub = self
            .subscribers
            .iter_mut()
            .find(|s| s.is(link))
            .ok_or("the subscription has ended")?;
        if frames {
            sub.screen = Some(Screen::default());
            let frame = next_frame(&mut self.engine, self.sequence, &self.title, sub, true)?
                .ok_or("no frame")?;
            return Ok(Switched::Frames(frame));
        }
        let snapshot = self
            .engine
            .snapshot()
            .map_err(|e| format!("snapshot: {e}"))?;
        perf::count(perf::Counter::Checkpoints, 1);
        perf::count(perf::Counter::CheckpointBytes, snapshot.len() as u64);
        sub.screen = None;
        Ok(Switched::Stream {
            sequence: self.sequence,
            snapshot,
        })
    }

    fn frame(&mut self, link: &Arc<FrameLink>) -> Result<Option<ScreenFrame>, String> {
        match self.subscribers.iter_mut().find(|s| s.is(link)) {
            Some(sub) => next_frame(&mut self.engine, self.sequence, &self.title, sub, false),
            None => Ok(None),
        }
    }

    /// Hand a committed record to every subscriber. One that is full has
    /// fallen behind: it is ended as lagged, never skipped silently.
    fn publish(&mut self, seq: u64, record: &Record, effect: Option<Effect>) {
        let focused = self.focused;
        self.subscribers.retain(|s| {
            // A subscriber on frames gets no records; it is told the screen
            // changed and asks for a frame when it is ready for one.
            if s.screen.is_some() {
                if let Some(link) = &s.sink.frames {
                    link.changed.notify_one();
                }
                return !s.records.is_closed();
            }
            // A clipboard write goes to exactly one client: the focused one.
            let effect = effect.clone().map(|mut e| {
                if focused.is_none() || s.client != focused {
                    e.clipboard = None;
                }
                e
            });
            // Mark before sending: the receiving task runs elsewhere and
            // may take the mark as soon as the record arrives. A mark made
            // after the send could come too late and wait for a record that
            // never follows.
            if let Some(stamp) = &s.stamp {
                stamp.mark();
            }
            match s.records.try_send((seq, record.clone(), effect)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    perf::count(perf::Counter::Lagged, 1);
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
            followers: self
                .subscribers
                .iter()
                .filter(|s| {
                    !s.records.is_closed() && s.ended.load(Ordering::SeqCst) == end::RUNNING
                })
                .count(),
            quiet: self.last_output.elapsed(),
            subscriber_queued: self
                .subscribers
                .iter()
                .map(|s| s.records.max_capacity() - s.records.capacity())
                .max()
                .unwrap_or(0),
            subscriber_capacity: self
                .subscribers
                .iter()
                .map(|s| s.records.max_capacity())
                .max()
                .unwrap_or(0),
            memory: self.memory(),
        }
    }

    fn memory(&self) -> usize {
        let s = self.engine.size();
        let lines = self.engine.scrollback_rows().unwrap_or(0) + usize::from(s.rows);
        lines * line_bytes(s.cols) + self.journal.bytes()
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
                self.last_output = Instant::now();
                let parse = perf::start();
                let fed = self.engine.feed(b);
                if parse.is_some() {
                    perf::finish(perf::Stage::Parse, parse);
                    perf::count(perf::Counter::ParsedBytes, b.len() as u64);
                }
                // Mirrors never answer the application (tmux owns replies).
                // Title and cwd become state; they and bells are live
                // effects for subscribers present now, never journaled.
                fed.map(|fx| {
                    if fx.title.is_some() {
                        self.title.clone_from(&fx.title);
                    }
                    if fx.pwd.is_some() {
                        self.cwd.clone_from(&fx.pwd);
                    }
                    // The last write wins when one ingestion holds several.
                    let clipboard = fx.clipboard.into_iter().last();
                    if fx.bells > 0
                        || fx.title.is_some()
                        || fx.pwd.is_some()
                        || clipboard.is_some()
                        || !fx.commands.is_empty()
                    {
                        self.effects += 1;
                        effect = Some(Effect {
                            id: self.effects,
                            bells: fx.bells,
                            title: fx.title,
                            cwd: fx.pwd,
                            clipboard,
                            commands: fx.commands,
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
                let publish = perf::start();
                if !self.subscribers.is_empty() {
                    self.publish(self.journal.next_sequence(), &record, effect);
                }
                self.sequence = self.journal.push(record, Instant::now());
                if publish.is_some() {
                    perf::finish(perf::Stage::Publish, publish);
                    perf::count(perf::Counter::Records, 1);
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn tail(&self, history: usize) -> Result<TailResult, String> {
        let r = self.read(history)?;
        let view = self.engine.view_state().map_err(|e| e.to_string())?;
        let scrollback = self.engine.scrollback_rows().map_err(|e| e.to_string())?;
        let mut lines = r.lines;
        let screen = lines.split_off(r.history);
        Ok(TailResult {
            generation: self.generation,
            sequence: self.sequence,
            scrollback,
            alt_screen: view.alt_screen,
            cols: r.cols,
            rows: r.rows,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            cursor_row: r.cursor.1,
            history: lines,
            screen,
        })
    }

    fn read(&self, history: usize) -> Result<ReadResult, String> {
        let size = self.engine.size();
        let rows = size.rows as usize;
        // Only the rows wanted are formatted: a look's cost follows its
        // window, not the scrollback (this runs on the ingest worker).
        let text = self
            .engine
            .tail_text(rows + history)
            .map_err(|e| e.to_string())?;
        let all: Vec<&str> = text.split('\n').collect();
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
    mut terminal: crate::TerminalDefaults,
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
            // The client each pane's focus was last given to. Kept apart
            // from the mirror so a mirror rebuilt for the same pane (a tier
            // change, a restore) still sends clipboard writes to it.
            let mut focus: HashMap<TmuxPaneId, u64> = HashMap::new();
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
                    WorkerMsg::Lost => break,
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
                                        followers: 0,
                                        quiet: std::time::Duration::ZERO,
                                        subscriber_queued: 0,
                                        subscriber_capacity: 0,
                                        memory: 0,
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
                            focused: focus.get(&id).copied(),
                            attach,
                            reattaches,
                            colors_from: 0,
                            last_output: Instant::now(),
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
                        at,
                    } => {
                        if let Some(at) = at {
                            perf::record(perf::Stage::Route, at.elapsed());
                        }
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
                        // Focus stays: tmux never reuses a pane id, and a
                        // pane started again keeps its focused client.
                    }
                    WorkerMsg::Retire { pane, reply } => {
                        let followed = mirrors.get(&pane).map(|m| {
                            m.subscribers
                                .iter()
                                .filter(|s| {
                                    !s.records.is_closed()
                                        && s.ended.load(Ordering::SeqCst) == end::RUNNING
                                })
                                .count()
                        });
                        let r = match followed {
                            Some(0) | None => {
                                mirrors.remove(&pane);
                                failed.remove(&pane);
                                Ok(())
                            }
                            Some(n) => Err(format!(
                                "pane %{} has {n} subscriber(s) (a view, tap, or filter); it stays warm",
                                pane.0
                            )),
                        };
                        let _ = reply.send(r);
                    }
                    WorkerMsg::Subscribe {
                        pane,
                        resume,
                        sink,
                        frames,
                        reply,
                    } => {
                        let r = match mirrors.get_mut(&pane) {
                            Some(m) => m.subscribe(resume, sink, frames),
                            None => Err(format!("pane %{} has no live mirror", pane.0)),
                        };
                        let _ = reply.send(r);
                    }
                    WorkerMsg::SetFeed {
                        pane,
                        link,
                        frames,
                        reply,
                    } => {
                        let r = match mirrors.get_mut(&pane) {
                            Some(m) => m.set_feed(&link, frames),
                            None => Err(format!("pane %{} has no live mirror", pane.0)),
                        };
                        let _ = reply.send(r);
                    }
                    WorkerMsg::Frame { pane, link, reply } => {
                        let r = mirrors.get_mut(&pane).map_or(Ok(None), |m| m.frame(&link));
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
                        if let (InputEvent::Focus(gained), Some(c)) = (&event, client) {
                            if *gained {
                                focus.insert(pane, c);
                            } else if focus.get(&pane) == Some(&c) {
                                focus.remove(&pane);
                            }
                            if let Some(m) = mirrors.get_mut(&pane) {
                                m.focused = focus.get(&pane).copied();
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
                    WorkerMsg::SetDefaults { terminal: t, reply } => {
                        terminal = t;
                        for m in mirrors.values_mut() {
                            // Setting defaults on a live engine does not fail
                            // short of a broken engine, which its next record
                            // reports.
                            let _ = m
                                .engine
                                .set_scheme(terminal.scheme)
                                .and_then(|()| m.engine.set_default_cursor(terminal.cursor, terminal.blink));
                            m.colors_from = m.sequence;
                            for s in m.subscribers.drain(..) {
                                let _ = s.ended.compare_exchange(
                                    end::RUNNING,
                                    end::REATTACHED,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                );
                            }
                        }
                        let _ = reply.send(());
                    }
                    WorkerMsg::Read {
                        pane,
                        history,
                        reply,
                    } => {
                        let _ = reply.send(mirrors.get(&pane).map(|m| m.read(history)));
                    }
                    WorkerMsg::Tail {
                        pane,
                        history,
                        after,
                        reply,
                    } => {
                        let _ = reply.send(mirrors.get(&pane).map(|m| {
                            if after == Some(m.sequence) {
                                Ok(None)
                            } else {
                                m.tail(history).map(Some)
                            }
                        }));
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
            // The connection is gone (`Lost`), or the router and host are.
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
