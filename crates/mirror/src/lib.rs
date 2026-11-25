//! Canonical pane mirrors in the daemon.
//!
//! One router task reads the events of the tmux control clients in order.
//! tmux sends a control client output only for panes in its own session,
//! so with [`SessionClients`] the router keeps one client per session and
//! takes each pane's output from exactly one of them, the pane's source:
//! the lowest session id whose session holds the pane's window. The other
//! clients turn that pane's output off. For each mirrored pane the router
//! runs the cold-attach protocol on the source (pause, capture in one
//! non-yielding line, continue; see `docs/tmux-bootstrap.md`) and then
//! forwards the pane's output, and size changes from `%layout-change`, to
//! the pane's single owner on a fixed worker pool. Nothing is reordered:
//! one stream per pane in, one ordered channel per worker out. When a
//! pane's source changes, its mirror is attached again from a capture.
//!
//! The router never waits on tmux itself; capture commands run as separate
//! tasks and report back, so the event stream keeps flowing meanwhile.

pub mod journal;
mod router;
pub mod worker;

use std::time::Duration;

use domain::{ConnectionEpoch, PaneGeneration, TmuxPaneId, TmuxServerRef};
use tmux_backend::{ConnEvent, Connection};
use tokio::sync::{mpsc, oneshot, watch};

pub use journal::Retention;
pub use terminal_ghostty::input::{MouseAction, MouseButton};
pub use worker::{
    Effect, InputEvent, Live, LiveStats, ReadResult, Record, Sink, Start, Started, TailResult,
    Tier, end,
};

#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// VT owner threads; panes are spread across them by id.
    pub workers: usize,
    /// Queued messages per worker before the router waits (backpressure).
    pub worker_queue: usize,
    pub retention: Retention,
    /// Colors and cursor every mirror starts with (canonical state).
    pub terminal: TerminalDefaults,
    /// One control client per tmux session, so panes outside the main
    /// connection's session stream too. `None`: only the main
    /// connection's session streams.
    pub sessions: Option<SessionClients>,
}

/// How the router attaches a control client to each tmux session.
#[derive(Debug, Clone)]
pub struct SessionClients {
    /// The template for each client; its `session` is replaced with the
    /// session's id. Give it `ignore-size` so the clients never size a
    /// window.
    pub connection: tmux_backend::ConnectionConfig,
    /// The session the main connection is attached to: its events come
    /// from the main connection, and no second client is made for it.
    pub primary: u32,
}

/// Where panes live, for [`MirrorHost::sync_placed`].
#[derive(Debug, Clone, Default)]
pub struct Placement {
    /// Every session on the server, by id (`$N` is `N`).
    pub sessions: std::collections::BTreeSet<u32>,
    /// The sessions holding each pane's window.
    pub homes: std::collections::HashMap<TmuxPaneId, std::collections::BTreeSet<u32>>,
}

/// A terminal's configured defaults: the color scheme and the cursor a
/// program gets until it sets its own. Applied by the canonical mirror
/// only; replicas get them from checkpoints.
#[derive(Debug, Clone, Copy)]
pub struct TerminalDefaults {
    pub scheme: &'static terminal_ghostty::core::schemes::Scheme,
    pub cursor: terminal_ghostty::core::CursorShape,
    pub blink: bool,
}

impl Default for TerminalDefaults {
    fn default() -> Self {
        TerminalDefaults {
            scheme: terminal_ghostty::core::schemes::default(),
            cursor: terminal_ghostty::core::CursorShape::Block,
            blink: false,
        }
    }
}

impl Default for MirrorConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        MirrorConfig {
            workers: cores.clamp(1, 4),
            worker_queue: 1024,
            retention: Retention::default(),
            terminal: TerminalDefaults::default(),
            sessions: None,
        }
    }
}

/// When panes that nothing follows go Cold on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierPolicy {
    /// A pane quiet for this long goes Cold; `None`: never for quiet.
    pub cold_after: Option<Duration>,
    /// At most this many Warm panes that nothing follows; beyond it the
    /// quietest go Cold. `None`: no limit.
    pub warm_limit: Option<usize>,
    /// At most this many bytes held by every live mirror together
    /// ([`LiveStats::memory`]); beyond it the quietest panes that nothing
    /// follows go Cold. Followed panes still count but are never demoted.
    /// `None`: no ceiling.
    pub warm_memory: Option<u64>,
}

/// What one pass of a [`TierPolicy`] demoted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Demoted {
    /// Quiet for longer than `cold_after`.
    pub quiet: usize,
    /// Over `warm_limit`.
    pub over_limit: usize,
    /// Over `warm_memory`.
    pub over_memory: usize,
    /// Bytes every live mirror is estimated to hold after the pass.
    pub memory: u64,
}

/// Where a pane's mirror stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorState {
    /// Paused and being captured.
    Attaching,
    /// Restored and following the pane's output.
    Live,
    Failed(String),
    /// The Cold tier: no mirror and no journal; its output is dropped
    /// unparsed. Any request for the pane's content attaches it again.
    Cold,
}

/// The Cold panes of one connection.
pub(crate) type ColdSet = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<TmuxPaneId>>>;

/// How many [`Watch`]es each pane of one connection has.
type Watched = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<TmuxPaneId, usize>>>;

/// A follower that reads a pane by looking at its mirror now and then
/// instead of subscribing to its records (a text or meta tap). While one is
/// held the pane counts as followed: it does not go Cold, on its own or
/// when asked.
#[derive(Debug)]
pub struct Watch {
    pane: TmuxPaneId,
    watched: Watched,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut w) = self.watched.lock()
            && let Some(n) = w.get_mut(&self.pane)
        {
            *n -= 1;
            if *n == 0 {
                w.remove(&self.pane);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorStatus {
    pub pane: TmuxPaneId,
    pub generation: PaneGeneration,
    pub state: MirrorState,
    pub worker: usize,
    /// Present once the worker holds the mirror.
    pub live: Option<LiveStats>,
}

#[derive(Debug)]
pub(crate) enum Control {
    Sync {
        tmux: TmuxServerRef,
        panes: Vec<(TmuxPaneId, PaneGeneration)>,
        placement: Option<Placement>,
        done: oneshot::Sender<()>,
    },
    /// A session's control client attached (or could not).
    ClientUp {
        session: u32,
        serial: u64,
        conn: Result<Connection, String>,
    },
    Phase1 {
        pane: TmuxPaneId,
        generation: PaneGeneration,
        attach: u32,
        result: Result<bool, String>,
    },
    Phase2 {
        pane: TmuxPaneId,
        generation: PaneGeneration,
        attach: u32,
        result: Result<
            Box<(
                tmux_backend::bootstrap::PaneState,
                tmux_backend::bootstrap::Captures,
            )>,
            String,
        >,
    },
    States {
        reply: oneshot::Sender<Vec<(TmuxPaneId, PaneGeneration, MirrorState)>>,
    },
    /// Into the Cold tier, unless anything follows the pane.
    Cool {
        pane: TmuxPaneId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Out of the Cold tier: `true` if it was Cold and is now attaching.
    Warm {
        pane: TmuxPaneId,
        reply: oneshot::Sender<bool>,
    },
    /// Diagnostics: input was sent to the pane at `at`.
    Input {
        pane: TmuxPaneId,
        at: std::time::Instant,
    },
    Queues {
        reply: oneshot::Sender<RouterQueues>,
    },
}

/// The router's queues at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouterQueues {
    /// Events from tmux waiting for the router, from every client.
    pub events: usize,
    /// Panes with output held because their owner's queue was full, and
    /// the bytes held.
    pub backlog_panes: usize,
    pub backlog_bytes: usize,
    /// Attaches in progress, and panes waiting for a turn.
    pub attaching: usize,
    pub attach_queue: usize,
}

/// Every queue on a connection's terminal path at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queues {
    pub router: RouterQueues,
    /// Output messages waiting on each owner's data lane, and the lane's
    /// capacity.
    pub workers: Vec<usize>,
    pub worker_capacity: usize,
    /// Subscribers (views, taps, filters), the deepest subscriber queue,
    /// and a subscriber queue's capacity.
    pub subscribers: usize,
    pub subscriber_max: usize,
    pub subscriber_capacity: usize,
}

/// The mirrors of one tmux connection.
#[derive(Debug, Clone)]
pub struct MirrorHost {
    control: mpsc::Sender<Control>,
    workers: Vec<worker::WorkerHandle>,
    epoch: ConnectionEpoch,
    retention: Retention,
    host_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    cold: ColdSet,
    watched: Watched,
    /// The control connection, for tmux's paste buffers.
    conn: Connection,
    /// Bumped with the buffer's name whenever tmux reports a paste buffer
    /// changed (a copy in tmux, `set-buffer`, or Dreamer's own write).
    buffers: watch::Receiver<(u64, String)>,
    /// What Dreamer itself last wrote to a buffer, so its own write is not
    /// reported back as a change from tmux.
    written: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

/// A fresh file in the daemon's private runtime directory for one paste
/// buffer's text on its way to or from tmux.
fn buffer_file() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    platform::runtime_dir().join(format!(
        "buffer-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Write `bytes` to a new file only its owner can read.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

/// The most of a paste buffer read or written, in bytes.
pub const BUFFER_MAX: usize = 4 << 20;

/// How long a request waits for a Cold pane to be attached again.
const COLD_WAKE: Duration = Duration::from_secs(5);

pub(crate) fn worker_of(pane: TmuxPaneId, workers: usize) -> usize {
    pane.0 as usize % workers.max(1)
}

impl MirrorHost {
    /// Take over `events` (the connection's event stream) and start the
    /// router and workers. Call this before anything else waits on the
    /// connection: events must keep flowing for responses to arrive. The returned watch counts structural changes
    /// (windows, layouts, pane processes); the owner re-reads the inventory
    /// when it moves and calls [`MirrorHost::sync`].
    pub async fn start(
        conn: Connection,
        events: mpsc::Receiver<ConnEvent>,
        epoch: ConnectionEpoch,
        config: MirrorConfig,
    ) -> std::io::Result<(MirrorHost, watch::Receiver<u64>)> {
        let host_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let workers = (0..config.workers.max(1))
            .map(|i| {
                worker::spawn(
                    i,
                    config.worker_queue,
                    config.retention,
                    config.terminal,
                    host_bytes.clone(),
                )
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let (control, control_rx) = mpsc::channel(256);
        let (changes_tx, changes) = watch::channel(0u64);
        let (buffers_tx, buffers) = watch::channel((0u64, String::new()));
        let cold = ColdSet::default();
        let tmux = conn.clone();
        router::spawn(
            router::Router::new(
                conn,
                events,
                control_rx,
                control.clone(),
                workers.clone(),
                epoch,
                changes_tx,
            )
            .with_cold(cold.clone())
            .with_buffers(buffers_tx)
            .with_sessions(config.sessions),
        )
        .await;
        Ok((
            MirrorHost {
                control,
                workers,
                epoch,
                retention: config.retention,
                host_bytes,
                cold,
                watched: Watched::default(),
                conn: tmux,
                buffers,
                written: std::sync::Arc::default(),
            },
            changes,
        ))
    }

    /// Follow tmux's paste buffers: the receiver moves whenever one changes.
    pub fn buffers(&self) -> watch::Receiver<(u64, String)> {
        self.buffers.clone()
    }

    /// A paste buffer's text (`name`, or the newest), at most
    /// [`BUFFER_MAX`] bytes; `None` when there is no such buffer.
    ///
    /// Read through a private file (`save-buffer`), never as command
    /// output: tmux renders a buffer's control characters in its output
    /// (on Linux a newline came back as `_`), and only the file is exact.
    pub async fn buffer_get(&self, name: Option<&str>) -> Option<String> {
        let path = buffer_file();
        let mut cmd = tmux_backend::TmuxCommand::new("save-buffer");
        if let Some(n) = name {
            cmd = cmd.opt("-b", n);
        }
        let saved = self
            .conn
            .execute(cmd.arg(path.to_string_lossy()))
            .await
            .is_ok();
        let bytes = saved.then(|| std::fs::read(&path).ok()).flatten();
        let _ = std::fs::remove_file(&path);
        let mut bytes = bytes?;
        bytes.truncate(BUFFER_MAX);
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        // A cut in the middle of a character.
        while text.ends_with('\u{fffd}') && bytes.len() == BUFFER_MAX {
            text.pop();
        }
        Some(text)
    }

    /// Make `text` tmux's newest paste buffer, so tmux's own paste (and
    /// other tmux clients) see it. Dreamer's own write is not reported
    /// back as a change ([`MirrorHost::is_own_buffer`]). Loaded from a
    /// private file (`load-buffer`), so every byte arrives as written.
    pub async fn buffer_set(&self, text: &str) -> Result<(), String> {
        if text.is_empty() || text.len() > BUFFER_MAX {
            return Err(format!("a paste buffer holds 1 to {BUFFER_MAX} bytes"));
        }
        let path = buffer_file();
        write_private(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Ok(mut w) = self.written.lock() {
            *w = Some(text.to_owned());
        }
        let r = self
            .conn
            .execute(tmux_backend::TmuxCommand::new("load-buffer").arg(path.to_string_lossy()))
            .await
            .map(drop)
            .map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&path);
        r
    }

    /// Whether `text` is what Dreamer itself last wrote to a buffer.
    pub fn is_own_buffer(&self, text: &str) -> bool {
        self.written
            .lock()
            .is_ok_and(|w| w.as_deref() == Some(text))
    }

    pub fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// Journal bytes held for every pane of this connection.
    pub fn host_journal_bytes(&self) -> usize {
        self.host_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the pane is in the Cold tier.
    pub fn is_cold(&self, pane: TmuxPaneId) -> bool {
        self.cold.lock().is_ok_and(|c| c.contains(&pane))
    }

    /// Bring a Cold pane back before its content is asked for: a fresh
    /// attach (a cold attach, as for a new pane), waited for
    /// up to `COLD_WAKE`. Nothing for any other pane, so the hot path costs
    /// one set lookup.
    async fn awake(&self, pane: TmuxPaneId) {
        if !self.is_cold(pane) {
            return;
        }
        let (reply, answer) = oneshot::channel();
        if self
            .control
            .send(Control::Warm { pane, reply })
            .await
            .is_ok()
            && answer.await.unwrap_or(false)
        {
            self.wait_live(&[pane], COLD_WAKE).await;
        }
    }

    /// Count as a follower of `pane` until the returned [`Watch`] is
    /// dropped, without subscribing to its records.
    pub fn watch(&self, pane: TmuxPaneId) -> Watch {
        if let Ok(mut w) = self.watched.lock() {
            *w.entry(pane).or_default() += 1;
        }
        Watch {
            pane,
            watched: self.watched.clone(),
        }
    }

    /// How many [`Watch`]es `pane` has.
    fn watchers(&self, pane: TmuxPaneId) -> usize {
        self.watched
            .lock()
            .map_or(0, |w| w.get(&pane).copied().unwrap_or(0))
    }

    /// Put a pane in the Cold tier: its mirror and journal are dropped, and
    /// its output is discarded before any VT work. Refused while anything
    /// follows the pane (a view, a tap, a filter).
    pub async fn set_cold(&self, pane: TmuxPaneId) -> Result<(), String> {
        let n = self.watchers(pane);
        if n > 0 {
            return Err(format!(
                "pane %{} has {n} subscriber(s) (a view, tap, or filter); it stays warm",
                pane.0
            ));
        }
        let (reply, answer) = oneshot::channel();
        self.control
            .send(Control::Cool { pane, reply })
            .await
            .map_err(|_| "the mirror router has stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the mirror router has stopped".to_owned())?
    }

    /// Apply a tier policy once: the Warm panes that nothing follows go
    /// Cold, quietest first, while they are quiet for longer than
    /// `cold_after`, there are more than `warm_limit` of them, or all
    /// mirrors together hold more than `warm_memory`. A pane that gains a
    /// follower meanwhile is refused and stays Warm. Panes in `keep` count
    /// as followed (a harness drawer while it is hidden), as do panes with
    /// a [`Watch`].
    pub async fn apply_policy(
        &self,
        policy: TierPolicy,
        keep: &std::collections::HashSet<TmuxPaneId>,
    ) -> Demoted {
        let live: Vec<MirrorStatus> = self
            .status()
            .await
            .into_iter()
            .filter(|m| m.state == MirrorState::Live)
            .collect();
        let mut memory: u64 = live
            .iter()
            .filter_map(|m| m.live.as_ref())
            .map(|l| l.memory as u64)
            .sum();
        let mut unfollowed: Vec<(TmuxPaneId, Duration, u64)> = live
            .into_iter()
            .filter(|m| !keep.contains(&m.pane) && self.watchers(m.pane) == 0)
            .filter_map(|m| {
                m.live
                    .filter(|l| l.followers == 0 && l.tier == Tier::Warm && l.error.is_none())
                    .map(|l| (m.pane, l.quiet, l.memory as u64))
            })
            .collect();
        unfollowed.sort_by_key(|a| std::cmp::Reverse(a.1));
        let mut left = unfollowed.len();
        let mut out = Demoted::default();
        for (pane, quiet, held) in unfollowed {
            let is_quiet = policy.cold_after.is_some_and(|d| quiet >= d);
            let over = policy.warm_limit.is_some_and(|n| left > n);
            let heavy = policy.warm_memory.is_some_and(|b| memory > b);
            // Quietest first: once none holds, none holds for the rest.
            if !is_quiet && !over && !heavy {
                break;
            }
            if self.set_cold(pane).await.is_ok() {
                left -= 1;
                memory = memory.saturating_sub(held);
                if is_quiet {
                    out.quiet += 1;
                } else if over {
                    out.over_limit += 1;
                } else {
                    out.over_memory += 1;
                }
            }
        }
        out.memory = memory;
        out
    }

    /// Subscribe to a live mirror: replay after `resume` when it is still
    /// retained for the same generation, otherwise a checkpoint. Records
    /// after the starting point arrive on `sink`, in order. A Cold pane is
    /// attached again first.
    pub async fn subscribe(
        &self,
        pane: TmuxPaneId,
        resume: Option<(PaneGeneration, u64)>,
        sink: Sink,
    ) -> Result<Started, String> {
        self.awake(pane).await;
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.urgent(worker::WorkerMsg::Subscribe {
            pane,
            resume,
            sink,
            reply,
        })
        .map_err(|_| "the mirror workers have stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the mirror workers have stopped".to_owned())?
    }

    /// Encode input on the pane's owner against its canonical state. A
    /// focus event from `client` makes it (or stops it being) the pane's
    /// focused client, the only one clipboard writes are addressed to.
    pub async fn encode(
        &self,
        pane: TmuxPaneId,
        generation: Option<PaneGeneration>,
        event: worker::InputEvent,
        client: Option<u64>,
    ) -> Result<Vec<u8>, String> {
        self.awake(pane).await;
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.urgent(worker::WorkerMsg::Encode {
            pane,
            generation,
            event,
            client,
            reply,
        })
        .map_err(|_| "the mirror workers have stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the mirror workers have stopped".to_owned())?
    }

    /// Set a pane's tier. `false` when the pane has no live mirror.
    pub async fn set_tier(&self, pane: TmuxPaneId, tier: Tier) -> bool {
        self.awake(pane).await;
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        if w.urgent(worker::WorkerMsg::SetTier { pane, tier, reply })
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// New terminal defaults for every mirror, now and from now on: each
    /// view takes a fresh checkpoint with the new colors. Colors a program
    /// set itself (OSC 4, 10, 11, 12) stay as it set them.
    pub async fn set_terminal_defaults(&self, terminal: TerminalDefaults) {
        let mut waits = Vec::new();
        for w in &self.workers {
            let (reply, answer) = oneshot::channel();
            if w.urgent(worker::WorkerMsg::SetDefaults { terminal, reply })
                .is_ok()
            {
                waits.push(answer);
            }
        }
        for a in waits {
            let _ = a.await;
        }
    }

    /// Mirror exactly these pane executions: attach new ones, re-attach a
    /// pane whose generation changed (respawned), and drop the rest.
    /// `tmux` identifies the server for the pane keys mirrors report.
    pub async fn sync(&self, tmux: TmuxServerRef, panes: Vec<(TmuxPaneId, PaneGeneration)>) {
        self.sync_with(tmux, panes, None).await
    }

    /// [`MirrorHost::sync`], and where each pane lives: with
    /// [`SessionClients`], the router keeps a control client for each
    /// session in `placement`, drops the rest, and moves a pane to another
    /// client (attaching it again) when its windows' sessions change.
    pub async fn sync_placed(
        &self,
        tmux: TmuxServerRef,
        panes: Vec<(TmuxPaneId, PaneGeneration)>,
        placement: Placement,
    ) {
        self.sync_with(tmux, panes, Some(placement)).await
    }

    async fn sync_with(
        &self,
        tmux: TmuxServerRef,
        panes: Vec<(TmuxPaneId, PaneGeneration)>,
        placement: Option<Placement>,
    ) {
        let (done, wait) = oneshot::channel();
        if self
            .control
            .send(Control::Sync {
                tmux,
                panes,
                placement,
                done,
            })
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    }

    /// Text and cursor of a live mirror. `None` when the pane is not
    /// mirrored (yet).
    pub async fn read(
        &self,
        pane: TmuxPaneId,
        history: usize,
    ) -> Option<Result<ReadResult, String>> {
        self.awake(pane).await;
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.urgent(worker::WorkerMsg::Read {
            pane,
            history,
            reply,
        })
        .ok()?;
        answer.await.ok().flatten()
    }

    /// The newest `history` scrollback lines, the active screen, and the
    /// scrollback size of a live mirror (for taps); `Ok(None)` when it is
    /// still at sequence `after`. `None` when the pane is not mirrored.
    pub async fn tail(
        &self,
        pane: TmuxPaneId,
        history: usize,
        after: Option<u64>,
    ) -> Option<Result<Option<worker::TailResult>, String>> {
        self.awake(pane).await;
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.urgent(worker::WorkerMsg::Tail {
            pane,
            history,
            after,
            reply,
        })
        .ok()?;
        answer.await.ok().flatten()
    }

    /// Diagnostics: input is being sent to `pane` now, so the time to its
    /// next output can be timed. Nothing while diagnostics are off.
    pub fn note_input(&self, pane: TmuxPaneId) {
        if perf::on() {
            let _ = self.control.try_send(Control::Input {
                pane,
                at: std::time::Instant::now(),
            });
        }
    }

    /// The depth of every queue on this connection's terminal path.
    pub async fn queues(&self) -> Queues {
        let (reply, answer) = oneshot::channel();
        let router = if self.control.send(Control::Queues { reply }).await.is_ok() {
            answer.await.unwrap_or_default()
        } else {
            RouterQueues::default()
        };
        let mut q = Queues {
            router,
            workers: self.workers.iter().map(|w| w.queued()).collect(),
            worker_capacity: self.workers.first().map_or(0, |w| w.capacity()),
            ..Queues::default()
        };
        for m in self.status().await {
            if let Some(l) = m.live {
                q.subscribers += l.followers;
                q.subscriber_max = q.subscriber_max.max(l.subscriber_queued);
                q.subscriber_capacity = q.subscriber_capacity.max(l.subscriber_capacity);
            }
        }
        q
    }

    /// Every mirrored pane and its state.
    pub async fn status(&self) -> Vec<MirrorStatus> {
        let (reply, answer) = oneshot::channel();
        if self.control.send(Control::States { reply }).await.is_err() {
            return Vec::new();
        }
        let states = answer.await.unwrap_or_default();
        let mut live = Vec::new();
        for w in &self.workers {
            let (reply, answer) = oneshot::channel();
            if w.urgent(worker::WorkerMsg::Stats { reply }).is_ok() {
                live.extend(answer.await.unwrap_or_default());
            }
        }
        states
            .into_iter()
            .map(|(pane, generation, mut state)| {
                let live = live
                    .iter()
                    .find(|l| l.pane.pane == pane && l.generation == generation)
                    .cloned();
                if let Some(e) = live.as_ref().and_then(|l| l.error.clone()) {
                    state = MirrorState::Failed(e);
                }
                MirrorStatus {
                    pane,
                    generation,
                    state,
                    worker: worker_of(pane, self.workers.len()),
                    live,
                }
            })
            .collect()
    }

    /// Wait until every listed pane's mirror is live. A Cold pane among
    /// them is attached again first: a request handler waits here before
    /// it subscribes, sends input, or taps, and each of those asks for the
    /// pane's content.
    pub async fn wait_live(&self, panes: &[TmuxPaneId], timeout: Duration) -> bool {
        for p in panes {
            if self.is_cold(*p) {
                let (reply, answer) = oneshot::channel();
                if self
                    .control
                    .send(Control::Warm { pane: *p, reply })
                    .await
                    .is_ok()
                {
                    let _ = answer.await;
                }
            }
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let s = self.status().await;
            if panes.iter().all(|p| {
                s.iter()
                    .any(|m| m.pane == *p && m.state == MirrorState::Live && m.live.is_some())
            }) {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
