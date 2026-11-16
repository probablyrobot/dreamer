//! The router: one task, the connection's events in order, a small state
//! machine per mirrored pane.

use std::collections::HashMap;

use domain::{ConnectionEpoch, PaneGeneration, PaneKey, TmuxPaneId, TmuxServerRef, WindowLayout};
use tmux_backend::bootstrap::{
    parse_captures, phase1_batch, phase1_found, phase2_batch, resume_command,
};
use tmux_backend::layout::parse_layout;
use tmux_backend::{CommandError, ConnEvent, Connection, TmuxCommand, affects_structure};
use tmux_protocol::{Notification, PaneId};
use tokio::sync::{mpsc, watch};

use crate::worker::{Record, WorkerHandle, WorkerMsg};
use crate::{Control, MirrorState, worker_of};

/// Output held for a pane whose worker lane is full, before the pane is
/// declared overrun and re-attached. Absorbs bursts without a re-capture.
const BACKLOG_BYTES: usize = 8 << 20;

/// Attaches in progress at once; more wait their turn. Each attach is a
/// few command batches on the shared control connection, whose queue is
/// bounded: hundreds of new panes at once would overflow it.
const MAX_ATTACHES: usize = 16;

/// How often held output is offered to its worker again.
const BACKLOG_TICK: std::time::Duration = std::time::Duration::from_millis(5);

/// Subscription that reports pane process changes (respawns) as
/// `%subscription-changed`; tmux checks it about once a second.
const PID_SUBSCRIPTION: &str = "dreamer-pid";

enum Flow {
    /// Waiting for `%pause`; output before it is already in the capture.
    Pausing {
        generation: PaneGeneration,
    },
    /// Paused and captured (or capturing); everything after `%pause` is the
    /// continuation, kept in order.
    Buffering {
        generation: PaneGeneration,
        records: Vec<Record>,
        continued: bool,
        captured: Option<
            Box<(
                tmux_backend::bootstrap::PaneState,
                tmux_backend::bootstrap::Captures,
            )>,
        >,
    },
    Live {
        generation: PaneGeneration,
    },
    Failed {
        generation: PaneGeneration,
        reason: String,
    },
    /// The Cold tier: no mirror and no journal; the pane's output is
    /// dropped here, before any VT work. (tmux's per-pane `off` is not
    /// used: with no other client it stops reading the pane, which would
    /// stall the program.)
    Cold {
        generation: PaneGeneration,
    },
    /// Waiting for a turn to attach (see `MAX_ATTACHES`); its output is in
    /// the coming capture.
    Queued {
        generation: PaneGeneration,
    },
}

impl Flow {
    fn generation(&self) -> PaneGeneration {
        match self {
            Flow::Pausing { generation }
            | Flow::Buffering { generation, .. }
            | Flow::Live { generation }
            | Flow::Failed { generation, .. }
            | Flow::Cold { generation }
            | Flow::Queued { generation } => *generation,
        }
    }
}

pub(crate) struct Router {
    conn: Connection,
    events: mpsc::Receiver<ConnEvent>,
    control: mpsc::Receiver<Control>,
    to_self: mpsc::Sender<Control>,
    workers: Vec<WorkerHandle>,
    tmux: Option<TmuxServerRef>,
    /// Each pane's current attach; output is tagged with it so a worker can
    /// drop output queued before a re-attach.
    attaches: HashMap<TmuxPaneId, u32>,
    /// Output waiting for room in a full worker lane, in order, with its
    /// size: the router itself never waits on one pane.
    backlog: HashMap<TmuxPaneId, (std::collections::VecDeque<Record>, usize)>,
    epoch: ConnectionEpoch,
    changes: watch::Sender<u64>,
    flows: HashMap<TmuxPaneId, Flow>,
    sizes: HashMap<TmuxPaneId, (u16, u16)>,
    /// The Cold panes, shared with the host so a request can tell at once
    /// whether a pane needs waking.
    cold: crate::ColdSet,
    /// Panes attaching now, and those waiting for a turn, in order.
    attaching: std::collections::HashSet<TmuxPaneId>,
    queued: std::collections::VecDeque<(TmuxPaneId, PaneGeneration)>,
}

pub(crate) async fn spawn(router: Router) {
    tokio::spawn(router.run());
}

fn pid(p: TmuxPaneId) -> PaneId {
    PaneId(p.0)
}

impl Router {
    pub(crate) fn new(
        conn: Connection,
        events: mpsc::Receiver<ConnEvent>,
        control: mpsc::Receiver<Control>,
        to_self: mpsc::Sender<Control>,
        workers: Vec<WorkerHandle>,
        epoch: ConnectionEpoch,
        changes: watch::Sender<u64>,
    ) -> Router {
        Router {
            conn,
            events,
            control,
            to_self,
            workers,
            tmux: None,
            attaches: HashMap::new(),
            backlog: HashMap::new(),
            epoch,
            changes,
            flows: HashMap::new(),
            sizes: HashMap::new(),
            cold: crate::ColdSet::default(),
            attaching: std::collections::HashSet::new(),
            queued: std::collections::VecDeque::new(),
        }
    }

    /// Share the Cold panes with the host.
    pub(crate) fn with_cold(mut self, cold: crate::ColdSet) -> Router {
        self.cold = cold;
        self
    }

    fn set_cold(&self, pane: TmuxPaneId, cold: bool) {
        if let Ok(mut c) = self.cold.lock() {
            if cold {
                c.insert(pane);
            } else {
                c.remove(&pane);
            }
        }
    }

    fn worker(&self, pane: TmuxPaneId) -> &WorkerHandle {
        &self.workers[worker_of(pane, self.workers.len())]
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    async fn run(mut self) {
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let _ = conn
                .execute(
                    TmuxCommand::new("refresh-client")
                        .opt_format("-B", format!("{PID_SUBSCRIPTION}:%*:#{{pane_pid}}")),
                )
                .await;
        });
        loop {
            let held = !self.backlog.is_empty();
            tokio::select! {
                () = tokio::time::sleep(BACKLOG_TICK), if held => self.drain(),
                c = self.control.recv() => match c {
                    Some(c) => self.on_control(c).await,
                    None => break,
                },
                e = self.events.recv() => match e {
                    Some(ConnEvent::Notification(n)) => self.on_notification(n).await,
                    Some(ConnEvent::LateOutcome { .. }) => {}
                    // The connection is gone: stop, which stops the workers
                    // and ends every subscription as connection lost.
                    Some(ConnEvent::Closed { .. }) | None => {
                        self.closed();
                        break;
                    }
                },
            }
        }
    }

    fn closed(&mut self) {
        for flow in self.flows.values_mut() {
            *flow = Flow::Failed {
                generation: flow.generation(),
                reason: "the tmux control connection ended".into(),
            };
        }
        self.changed();
    }

    async fn on_control(&mut self, c: Control) {
        match c {
            Control::Sync { tmux, panes, done } => {
                self.tmux = Some(tmux);
                let wanted: HashMap<TmuxPaneId, PaneGeneration> = panes.into_iter().collect();
                let stale: Vec<TmuxPaneId> = self
                    .flows
                    .iter()
                    .filter(|(p, f)| wanted.get(p) != Some(&f.generation()))
                    .map(|(p, _)| *p)
                    .collect();
                for p in stale {
                    self.stop(p).await;
                }
                for (p, g) in wanted {
                    if !self.flows.contains_key(&p) {
                        self.start(p, g);
                    }
                }
                let _ = done.send(());
            }
            Control::Phase1 {
                pane,
                generation,
                result,
            } => {
                if self
                    .flows
                    .get(&pane)
                    .is_some_and(|f| f.generation() == generation)
                {
                    match result {
                        Ok(true) => {}
                        Ok(false) => {
                            self.flows.remove(&pane);
                            self.attached(pane);
                        }
                        Err(reason) => {
                            self.flows.insert(pane, Flow::Failed { generation, reason });
                            self.attached(pane);
                        }
                    }
                }
            }
            Control::Phase2 {
                pane,
                generation,
                result,
            } => {
                let current = self
                    .flows
                    .get_mut(&pane)
                    .filter(|f| f.generation() == generation);
                match (current, result) {
                    (Some(Flow::Buffering { captured, .. }), Ok(c)) => *captured = Some(c),
                    (Some(flow), Err(reason)) => {
                        *flow = Flow::Failed { generation, reason };
                        self.attached(pane);
                    }
                    _ => {}
                }
                self.try_finish(pane).await;
            }
            Control::Cool { pane, reply } => {
                let r = match self.flows.get(&pane) {
                    Some(Flow::Cold { .. }) => Ok(()),
                    Some(Flow::Live { generation }) => {
                        let generation = *generation;
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        match self
                            .worker(pane)
                            .urgent(WorkerMsg::Retire { pane, reply: tx })
                        {
                            Err(_) => Err("the mirror workers have stopped".to_owned()),
                            Ok(()) => match rx.await {
                                Ok(Ok(())) => {
                                    // Output still queued for the old mirror is
                                    // stale from here on.
                                    *self.attaches.entry(pane).or_default() += 1;
                                    self.backlog.remove(&pane);
                                    self.flows.insert(pane, Flow::Cold { generation });
                                    self.set_cold(pane, true);
                                    self.changed();
                                    Ok(())
                                }
                                Ok(Err(why)) => Err(why),
                                Err(_) => Err("the mirror workers have stopped".to_owned()),
                            },
                        }
                    }
                    Some(_) => Err(format!("pane %{} is attaching; try again", pane.0)),
                    None => Err(format!("pane %{} is not mirrored", pane.0)),
                };
                let _ = reply.send(r);
            }
            Control::Warm { pane, reply } => {
                let woke = match self.flows.get(&pane) {
                    Some(Flow::Cold { generation }) => {
                        let generation = *generation;
                        self.set_cold(pane, false);
                        self.start(pane, generation);
                        self.changed();
                        true
                    }
                    _ => false,
                };
                let _ = reply.send(woke);
            }
            Control::States { reply } => {
                let _ = reply.send(
                    self.flows
                        .iter()
                        .map(|(p, f)| {
                            let s = match f {
                                Flow::Pausing { .. } | Flow::Buffering { .. } => {
                                    MirrorState::Attaching
                                }
                                Flow::Live { .. } => MirrorState::Live,
                                Flow::Failed { reason, .. } => MirrorState::Failed(reason.clone()),
                                Flow::Cold { .. } => MirrorState::Cold,
                                Flow::Queued { .. } => MirrorState::Attaching,
                            };
                            (*p, f.generation(), s)
                        })
                        .collect(),
                );
            }
        }
    }

    /// Attach a pane now, or queue it when `MAX_ATTACHES` are in progress.
    fn start(&mut self, pane: TmuxPaneId, generation: PaneGeneration) {
        if !self.attaching.contains(&pane) && self.attaching.len() >= MAX_ATTACHES {
            self.backlog.remove(&pane);
            self.flows.insert(pane, Flow::Queued { generation });
            self.queued.push_back((pane, generation));
            return;
        }
        self.attaching.insert(pane);
        self.attach(pane, generation);
    }

    /// An attach ended (live, failed, gone, or stopped): give the next
    /// queued pane its turn.
    fn attached(&mut self, pane: TmuxPaneId) {
        self.attaching.remove(&pane);
        while self.attaching.len() < MAX_ATTACHES {
            let Some((p, g)) = self.queued.pop_front() else {
                break;
            };
            if matches!(self.flows.get(&p), Some(Flow::Queued { generation }) if *generation == g) {
                self.attaching.insert(p);
                self.attach(p, g);
            }
        }
    }

    fn attach(&mut self, pane: TmuxPaneId, generation: PaneGeneration) {
        *self.attaches.entry(pane).or_default() += 1;
        // Held output belongs to the old attach; the capture covers it.
        self.backlog.remove(&pane);
        self.flows.insert(pane, Flow::Pausing { generation });
        let conn = self.conn.clone();
        let back = self.to_self.clone();
        tokio::spawn(async move {
            let result = match conn.execute(phase1_batch(pid(pane))).await {
                Ok(r) => Ok(phase1_found(pid(pane), &r)),
                Err(e) => Err(format!("pausing for capture: {e}")),
            };
            let _ = back
                .send(Control::Phase1 {
                    pane,
                    generation,
                    result,
                })
                .await;
        });
    }

    async fn stop(&mut self, pane: TmuxPaneId) {
        match self.flows.remove(&pane) {
            Some(Flow::Live { .. }) | Some(Flow::Failed { .. }) => {
                let _ = self.worker(pane).urgent(WorkerMsg::Drop { pane });
            }
            Some(Flow::Pausing { .. } | Flow::Buffering { .. }) => {
                // Never leave a pane paused for this client.
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    let _ = conn.execute(resume_command(pid(pane))).await;
                });
            }
            Some(Flow::Cold { .. }) => self.set_cold(pane, false),
            Some(Flow::Queued { .. }) => {}
            None => {}
        }
        self.sizes.remove(&pane);
        self.attaches.remove(&pane);
        if self.attaching.contains(&pane) {
            self.attached(pane);
        }
        self.backlog.remove(&pane);
    }

    /// Offer held output to the workers again, oldest first per pane.
    fn drain(&mut self) {
        let panes: Vec<TmuxPaneId> = self.backlog.keys().copied().collect();
        for pane in panes {
            let Some(Flow::Live { generation }) = self.flows.get(&pane) else {
                self.backlog.remove(&pane);
                continue;
            };
            let generation = *generation;
            let attach = self.attaches.get(&pane).copied().unwrap_or_default();
            let worker = &self.workers[worker_of(pane, self.workers.len())];
            let Some((queue, bytes)) = self.backlog.get_mut(&pane) else {
                continue;
            };
            while !queue.is_empty() {
                match worker.try_reserve() {
                    Ok(slot) => {
                        let Some(record) = queue.pop_front() else {
                            break;
                        };
                        *bytes -= record_size(&record);
                        slot.send(WorkerMsg::Apply {
                            pane,
                            generation,
                            attach,
                            record,
                        });
                    }
                    Err(crate::worker::LaneError::Full) => break,
                    Err(crate::worker::LaneError::Gone) => queue.clear(),
                }
            }
            if queue.is_empty() {
                self.backlog.remove(&pane);
            }
        }
    }

    async fn try_finish(&mut self, pane: TmuxPaneId) {
        let ready = matches!(
            self.flows.get(&pane),
            Some(Flow::Buffering {
                continued: true,
                captured: Some(_),
                ..
            })
        );
        if !ready {
            return;
        }
        let Some(Flow::Buffering {
            generation,
            records,
            captured: Some(captured),
            ..
        }) = self.flows.remove(&pane)
        else {
            return;
        };
        let Some(tmux) = self.tmux.clone() else {
            return;
        };
        let (state, captures) = *captured;
        let size = state.size();
        let mut final_size = (size.cols, size.rows);
        for r in &records {
            if let Record::Resize { cols, rows } = r {
                final_size = (*cols, *rows);
            }
        }
        self.sizes.insert(pane, final_size);
        let msg = WorkerMsg::Init {
            pane: PaneKey { tmux, pane },
            epoch: self.epoch,
            generation,
            state: Box::new(state),
            captures: Box::new(captures),
            continuation: records,
            attach: self.attaches.get(&pane).copied().unwrap_or_default(),
        };
        // Ahead of output still queued from an earlier attach, which the
        // worker then drops as stale.
        let _ = self.worker(pane).urgent(msg);
        self.flows.insert(pane, Flow::Live { generation });
        self.attached(pane);
    }

    async fn record(&mut self, pane: TmuxPaneId, record: Record) {
        match self.flows.get_mut(&pane) {
            Some(Flow::Buffering { records, .. }) => records.push(record),
            Some(Flow::Live { generation }) => {
                let generation = *generation;
                let attach = self.attaches.get(&pane).copied().unwrap_or_default();
                // Never wait on one pane: the router reads every pane's
                // output. Output the worker has no room for is held, in
                // order; held output beyond BACKLOG_BYTES means the pane
                // outran its mirror, so it is rebuilt from a fresh capture
                // (output from here to its %pause is part of that capture).
                if !self.backlog.contains_key(&pane)
                    && let Ok(slot) = self.worker(pane).try_reserve()
                {
                    slot.send(WorkerMsg::Apply {
                        pane,
                        generation,
                        attach,
                        record,
                    });
                    return;
                }
                let (queue, bytes) = self.backlog.entry(pane).or_default();
                *bytes += record_size(&record);
                queue.push_back(record);
                if *bytes > BACKLOG_BYTES {
                    self.start(pane, generation);
                    self.changed();
                }
            }
            // Pausing: already in the coming capture. Failed or unmirrored:
            // nothing to keep.
            _ => {}
        }
    }

    async fn on_notification(&mut self, n: Notification) {
        if affects_structure(&n) {
            self.changed();
        }
        match n {
            Notification::Output { pane, data }
            | Notification::ExtendedOutput { pane, data, .. } => {
                self.record(TmuxPaneId(pane.0), Record::Bytes(data)).await;
            }
            Notification::Pause { pane } => {
                let p = TmuxPaneId(pane.0);
                if let Some(Flow::Pausing { generation }) = self.flows.get(&p) {
                    let generation = *generation;
                    self.flows.insert(
                        p,
                        Flow::Buffering {
                            generation,
                            records: Vec::new(),
                            continued: false,
                            captured: None,
                        },
                    );
                    let conn = self.conn.clone();
                    let back = self.to_self.clone();
                    tokio::spawn(async move {
                        let result = match conn.execute(phase2_batch(pane)).await {
                            Ok(r) => parse_captures(&r).map(Box::new).map_err(|e| e.to_string()),
                            Err(e) => {
                                // Whatever ran, make sure the pane resumes.
                                let _ = conn.execute(resume_command(pane)).await;
                                Err(match e {
                                    CommandError::Failed { .. } => {
                                        format!(
                                            "capture failed: {}",
                                            tmux_backend::ops::failure_message(&e)
                                        )
                                    }
                                    other => format!("capture: {other}"),
                                })
                            }
                        };
                        let _ = back
                            .send(Control::Phase2 {
                                pane: p,
                                generation,
                                result,
                            })
                            .await;
                    });
                }
            }
            Notification::Continue { pane } => {
                let p = TmuxPaneId(pane.0);
                if let Some(Flow::Buffering { continued, .. }) = self.flows.get_mut(&p) {
                    *continued = true;
                }
                self.try_finish(p).await;
            }
            Notification::LayoutChange { visible_layout, .. } => {
                if let WindowLayout::Tiled(node) = parse_layout(&visible_layout) {
                    for (p, rect) in node.panes() {
                        let size = (rect.cols, rect.rows);
                        let known = self.sizes.get(&p).copied();
                        let buffering = matches!(self.flows.get(&p), Some(Flow::Buffering { .. }));
                        if buffering || known.is_some_and(|k| k != size) {
                            self.sizes.insert(p, size);
                            self.record(
                                p,
                                Record::Resize {
                                    cols: size.0,
                                    rows: size.1,
                                },
                            )
                            .await;
                        }
                    }
                }
            }
            Notification::SubscriptionChanged { name, .. } if name == PID_SUBSCRIPTION => {
                self.changed()
            }
            _ => {}
        }
    }
}

fn record_size(r: &Record) -> usize {
    match r {
        Record::Bytes(b) => b.len(),
        Record::Resize { .. } => 0,
    }
}
