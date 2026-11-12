//! Canonical pane mirrors in the daemon (spec 5.2, 8, 11.5).
//!
//! One router task reads a tmux control connection's events in order. For
//! each mirrored pane it runs the cold-attach protocol (pause, capture in
//! one non-yielding line, continue; see `docs/tmux-bootstrap.md`) and then
//! forwards the pane's output, and size changes from `%layout-change`, to
//! the pane's single owner on a fixed worker pool. Nothing is reordered:
//! one stream in, one ordered channel per worker out.
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
pub use worker::{LiveStats, ReadResult, Record, Sink, Start, Started, Tier, end};

#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// VT owner threads; panes are spread across them by id.
    pub workers: usize,
    /// Queued messages per worker before the router waits (backpressure).
    pub worker_queue: usize,
    pub retention: Retention,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        MirrorConfig {
            workers: cores.clamp(1, 4),
            worker_queue: 1024,
            retention: Retention::default(),
        }
    }
}

/// Where a pane's mirror stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorState {
    /// Paused and being captured.
    Attaching,
    /// Restored and following the pane's output.
    Live,
    Failed(String),
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
        done: oneshot::Sender<()>,
    },
    Phase1 {
        pane: TmuxPaneId,
        generation: PaneGeneration,
        result: Result<bool, String>,
    },
    Phase2 {
        pane: TmuxPaneId,
        generation: PaneGeneration,
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
}

/// The mirrors of one tmux connection.
#[derive(Debug, Clone)]
pub struct MirrorHost {
    control: mpsc::Sender<Control>,
    workers: Vec<mpsc::Sender<worker::WorkerMsg>>,
    epoch: ConnectionEpoch,
    retention: Retention,
    host_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

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
            .map(|i| worker::spawn(i, config.worker_queue, config.retention, host_bytes.clone()))
            .collect::<std::io::Result<Vec<_>>>()?;
        let (control, control_rx) = mpsc::channel(256);
        let (changes_tx, changes) = watch::channel(0u64);
        router::spawn(router::Router::new(
            conn,
            events,
            control_rx,
            control.clone(),
            workers.clone(),
            epoch,
            changes_tx,
        ))
        .await;
        Ok((
            MirrorHost {
                control,
                workers,
                epoch,
                retention: config.retention,
                host_bytes,
            },
            changes,
        ))
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

    /// Subscribe to a live mirror: replay after `resume` when it is still
    /// retained for the same generation, otherwise a checkpoint. Records
    /// after the starting point arrive on `sink`, in order.
    pub async fn subscribe(
        &self,
        pane: TmuxPaneId,
        resume: Option<(PaneGeneration, u64)>,
        sink: Sink,
    ) -> Result<Started, String> {
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.send(worker::WorkerMsg::Subscribe {
            pane,
            resume,
            sink,
            reply,
        })
        .await
        .map_err(|_| "the mirror workers have stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "the mirror workers have stopped".to_owned())?
    }

    /// Set a pane's tier. `false` when the pane has no live mirror.
    pub async fn set_tier(&self, pane: TmuxPaneId, tier: Tier) -> bool {
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        if w.send(worker::WorkerMsg::SetTier { pane, tier, reply })
            .await
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    /// Mirror exactly these pane executions: attach new ones, re-attach a
    /// pane whose generation changed (respawned), and drop the rest.
    /// `tmux` identifies the server for the pane keys mirrors report.
    pub async fn sync(&self, tmux: TmuxServerRef, panes: Vec<(TmuxPaneId, PaneGeneration)>) {
        let (done, wait) = oneshot::channel();
        if self
            .control
            .send(Control::Sync { tmux, panes, done })
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
        let (reply, answer) = oneshot::channel();
        let w = &self.workers[worker_of(pane, self.workers.len())];
        w.send(worker::WorkerMsg::Read {
            pane,
            history,
            reply,
        })
        .await
        .ok()?;
        answer.await.ok().flatten()
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
            if w.send(worker::WorkerMsg::Stats { reply }).await.is_ok() {
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

    /// Wait until every listed pane's mirror is live (tests, diagnostics).
    pub async fn wait_live(&self, panes: &[TmuxPaneId], timeout: Duration) -> bool {
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
