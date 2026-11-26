//! The router: one task, each control client's events in order, a small
//! state machine per mirrored pane. A pane's events are taken from one
//! client only, its source (see the crate docs).

use std::collections::{BTreeSet, HashMap, HashSet};

use domain::{ConnectionEpoch, PaneGeneration, PaneKey, TmuxPaneId, TmuxServerRef, WindowLayout};
use tmux_backend::bootstrap::{
    pane_output_command, parse_captures, phase1_batch, phase1_found, phase2_batch, resume_command,
};
use tmux_backend::layout::parse_layout;
use tmux_backend::{Batch, CommandError, ConnEvent, Connection, TmuxCommand, affects_structure};
use tmux_protocol::{Notification, PaneId};
use tokio::sync::{mpsc, watch};

use crate::worker::{Record, WorkerHandle, WorkerMsg};
use crate::{Control, MirrorState, SessionClients, worker_of};

/// Memory of output held for a pane whose worker lane is full (see
/// [`held_cost`]), before the pane is declared overrun and re-attached.
/// Absorbs bursts without a re-capture.
const BACKLOG_BYTES: usize = 8 << 20;

/// Attaches in progress at once; more wait their turn. Each attach is a
/// few command batches on the shared control connection, whose queue is
/// bounded: hundreds of new panes at once would overflow it.
const MAX_ATTACHES: usize = 16;

/// How often held output is offered to its worker again when no more of
/// that pane's output arrives (which offers it at once).
const BACKLOG_TICK: std::time::Duration = std::time::Duration::from_millis(5);

/// Subscription that reports pane process changes (respawns) as
/// `%subscription-changed`; tmux checks it about once a second.
const PID_SUBSCRIPTION: &str = "dreamer-pid";

/// Events from the per-session clients waiting for the router.
const STREAM_CAPACITY: usize = 4096;

fn pid_subscription() -> TmuxCommand {
    TmuxCommand::new("refresh-client")
        .opt_format("-B", format!("{PID_SUBSCRIPTION}:%*:#{{pane_pid}}"))
}

/// One session's control client.
enum Client {
    /// Attaching; its events are read but no pane takes them yet.
    Starting {
        serial: u64,
    },
    Up {
        serial: u64,
        conn: Connection,
    },
}

impl Client {
    fn serial(&self) -> u64 {
        match self {
            Client::Starting { serial } | Client::Up { serial, .. } => *serial,
        }
    }
}

/// An event from a session's client: the session, the client's serial
/// (a client made again for the same session gets a new one), the event.
type Tagged = (u32, u64, ConnEvent);

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

/// A pane's held output, in order. Each record keeps its decode time
/// while diagnostics are on.
type Held = std::collections::VecDeque<(Record, Option<std::time::Instant>)>;

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
    /// Output waiting for room in a full worker lane, in order, with the
    /// memory it holds: the router itself never waits on one pane.
    backlog: HashMap<TmuxPaneId, (Held, usize)>,
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
    /// When to hand freed memory back to the system: shortly after the
    /// last pane went Cold, once for a whole batch of demotions.
    release_at: Option<tokio::time::Instant>,
    /// tmux's paste-buffer changes, for the host's followers.
    buffers: Option<watch::Sender<(u64, String)>>,
    /// Diagnostics only: when the read the current notifications came
    /// from returned, and when it was decoded.
    read_at: Option<(std::time::Instant, std::time::Instant)>,
    /// Diagnostics only: when input was last sent to each pane, until its
    /// next output is read.
    inputs: HashMap<TmuxPaneId, std::time::Instant>,
    /// The session the main connection (`conn`, `events`) is attached to.
    primary: u32,
    /// How to attach a client to each other session; `None`: only the
    /// main connection's session streams.
    sessions: Option<SessionClients>,
    /// The control clients by session, the main connection's included.
    clients: HashMap<u32, Client>,
    next_serial: u64,
    /// The other clients' events, each client's in order.
    streams: mpsc::Receiver<Tagged>,
    streams_tx: mpsc::Sender<Tagged>,
    /// The sessions holding each pane's window (from the last sync).
    homes: HashMap<TmuxPaneId, BTreeSet<u32>>,
    /// The session whose client each pane's current attach reads: the only
    /// client its output, pauses and sizes are taken from.
    sources: HashMap<TmuxPaneId, u32>,
    /// Panes each client was told to send no output for.
    muted: HashMap<u32, HashSet<TmuxPaneId>>,
    /// Panes waiting for their session's client to attach.
    waiting: HashSet<TmuxPaneId>,
}

/// How long after the last demotion freed memory is released.
const RELEASE_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

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
        let (streams_tx, streams) = mpsc::channel(STREAM_CAPACITY);
        let mut clients = HashMap::new();
        clients.insert(
            0,
            Client::Up {
                serial: 0,
                conn: conn.clone(),
            },
        );
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
            release_at: None,
            buffers: None,
            read_at: None,
            inputs: HashMap::new(),
            primary: 0,
            sessions: None,
            clients,
            next_serial: 0,
            streams,
            streams_tx,
            homes: HashMap::new(),
            sources: HashMap::new(),
            muted: HashMap::new(),
            waiting: HashSet::new(),
        }
    }

    /// Keep a control client per tmux session (see [`SessionClients`]).
    pub(crate) fn with_sessions(mut self, sessions: Option<SessionClients>) -> Router {
        if let Some(s) = &sessions {
            let main = self.clients.remove(&self.primary);
            self.primary = s.primary;
            if let Some(main) = main {
                self.clients.insert(s.primary, main);
            }
        }
        self.sessions = sessions;
        self
    }

    /// Share the Cold panes with the host.
    pub(crate) fn with_cold(mut self, cold: crate::ColdSet) -> Router {
        self.cold = cold;
        self
    }

    /// Report paste-buffer changes on `tx`.
    pub(crate) fn with_buffers(mut self, tx: watch::Sender<(u64, String)>) -> Router {
        self.buffers = Some(tx);
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
        tokio::spawn(self.conn.submit(pid_subscription()));
        // One ticker for the whole run: a sleep made afresh on each turn
        // would never end while events arrive faster than the tick, and held
        // output would wait for a lull in every pane's output.
        let mut tick = tokio::time::interval(BACKLOG_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let held = !self.backlog.is_empty();
            tokio::select! {
                _ = tick.tick(), if held => self.drain(),
                () = tokio::time::sleep_until(self.release_at.unwrap_or_else(tokio::time::Instant::now)),
                    if self.release_at.is_some() =>
                {
                    self.release_at = None;
                    tokio::task::spawn_blocking(platform::release_free_memory);
                }
                c = self.control.recv() => match c {
                    Some(c) => self.on_control(c).await,
                    None => break,
                },
                Some((session, serial, e)) = self.streams.recv() => {
                    self.on_stream(session, serial, e).await
                }
                e = self.events.recv() => match e {
                    Some(ConnEvent::Notification(n)) => self.on_notification(self.primary, n).await,
                    Some(ConnEvent::LateOutcome { .. }) => {}
                    Some(ConnEvent::Read { read, decoded }) => self.read_at = Some((read, decoded)),
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
        // End every subscription now. A tmux server started again on the
        // same socket numbers its panes from zero, so nothing may stay
        // attached to this connection's mirrors under those ids.
        for w in &self.workers {
            let _ = w.urgent(WorkerMsg::Lost);
        }
        self.changed();
    }

    async fn on_control(&mut self, c: Control) {
        match c {
            Control::Sync {
                tmux,
                panes,
                placement,
                done,
            } => {
                self.tmux = Some(tmux);
                if let Some(placement) = placement
                    && self.sessions.is_some()
                {
                    self.homes = placement.homes;
                    self.update_clients(&placement.sessions);
                }
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
                for (p, g) in &wanted {
                    if !self.flows.contains_key(p) {
                        self.start(*p, *g);
                    }
                }
                // Pane ids are never reused on a server: forget gone ones.
                for m in self.muted.values_mut() {
                    m.retain(|p| wanted.contains_key(p));
                }
                self.homes.retain(|p, _| wanted.contains_key(p));
                self.rehome();
                let _ = done.send(());
            }
            Control::ClientUp {
                session,
                serial,
                conn,
            } => {
                if self.clients.get(&session).map(Client::serial) != Some(serial) {
                    // Dropped meanwhile: its session went.
                    return;
                }
                match conn {
                    Ok(conn) => {
                        self.clients.insert(session, Client::Up { serial, conn });
                        self.rehome();
                    }
                    Err(reason) => {
                        // Visible as Failed; the next sync tries again.
                        self.clients.remove(&session);
                        let stuck: Vec<TmuxPaneId> = self
                            .waiting
                            .iter()
                            .filter(|p| self.wanted(**p) == session)
                            .copied()
                            .collect();
                        for p in stuck {
                            self.waiting.remove(&p);
                            if let Some(flow) = self.flows.get_mut(&p) {
                                *flow = Flow::Failed {
                                    generation: flow.generation(),
                                    reason: reason.clone(),
                                };
                            }
                        }
                        // Their state changed; no re-read, which would only
                        // try the same attach again at once.
                    }
                }
            }
            Control::Phase1 {
                pane,
                generation,
                attach,
                result,
            } => {
                if self
                    .flows
                    .get(&pane)
                    .is_some_and(|f| f.generation() == generation)
                    && self.attaches.get(&pane) == Some(&attach)
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
                attach,
                result,
            } => {
                // A result for an earlier attach (the pane moved to another
                // client meanwhile) belongs to nothing now.
                let fresh = self.attaches.get(&pane) == Some(&attach);
                let current = self
                    .flows
                    .get_mut(&pane)
                    .filter(|f| fresh && f.generation() == generation);
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
                                    self.release_at =
                                        Some(tokio::time::Instant::now() + RELEASE_AFTER);
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
            Control::Input { pane, at } => {
                if perf::on() {
                    self.inputs.insert(pane, at);
                }
            }
            Control::Queues { reply } => {
                let _ = reply.send(crate::RouterQueues {
                    events: self.events.len() + self.streams.len(),
                    backlog_panes: self.backlog.len(),
                    backlog_bytes: self.backlog.values().map(|(_, b)| *b).sum(),
                    attaching: self.attaching.len(),
                    attach_queue: self.queued.len(),
                });
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
        let Some((source, conn)) = self.source_for(pane) else {
            // Its session's client is not attached yet.
            self.attaching.remove(&pane);
            self.backlog.remove(&pane);
            self.leave_source(pane);
            self.flows.insert(pane, Flow::Queued { generation });
            self.waiting.insert(pane);
            return;
        };
        let attach = {
            let a = self.attaches.entry(pane).or_default();
            *a += 1;
            *a
        };
        // Held output belongs to the old attach; the capture covers it.
        self.backlog.remove(&pane);
        if self.sources.get(&pane) != Some(&source) {
            self.leave_source(pane);
        }
        self.flows.insert(pane, Flow::Pausing { generation });
        self.sources.insert(pane, source);
        self.waiting.remove(&pane);
        // Submitted in order on the source: output on, then the pause.
        self.unmute(source, pane);
        let phase1 = conn.submit(phase1_batch(pid(pane)));
        let back = self.to_self.clone();
        tokio::spawn(async move {
            let result = match phase1.await {
                Ok(r) => Ok(phase1_found(pid(pane), &r)),
                Err(e) => Err(format!("pausing for capture: {e}")),
            };
            let _ = back
                .send(Control::Phase1 {
                    pane,
                    generation,
                    attach,
                    result,
                })
                .await;
        });
    }

    /// The session whose client should carry a pane: the lowest session id
    /// holding its window, or the main connection's session.
    fn wanted(&self, pane: TmuxPaneId) -> u32 {
        self.homes
            .get(&pane)
            .and_then(|h| h.first().copied())
            .unwrap_or(self.primary)
    }

    fn conn_of(&self, session: u32) -> Option<&Connection> {
        match self.clients.get(&session) {
            Some(Client::Up { conn, .. }) => Some(conn),
            _ => None,
        }
    }

    /// The client a pane should be attached through, if it is up.
    fn source_for(&self, pane: TmuxPaneId) -> Option<(u32, Connection)> {
        let want = self.wanted(pane);
        self.conn_of(want).map(|c| (want, c.clone()))
    }

    fn is_source(&self, pane: TmuxPaneId, session: u32) -> bool {
        self.sources.get(&pane) == Some(&session)
    }

    /// Turn a pane's output back on for a client that had it off.
    fn unmute(&mut self, session: u32, pane: TmuxPaneId) {
        if self
            .muted
            .get_mut(&session)
            .is_some_and(|m| m.remove(&pane))
            && let Some(conn) = self.conn_of(session)
        {
            tokio::spawn(conn.submit(pane_output_command(pid(pane), true)));
        }
    }

    /// The pane stops reading its current source: never leave it paused
    /// there (a paused pane would not pause again, and so never report
    /// `%pause`, if the pane came back to that client).
    fn leave_source(&mut self, pane: TmuxPaneId) {
        if let Some(old) = self.sources.remove(&pane)
            && let Some(conn) = self.conn_of(old)
        {
            tokio::spawn(conn.submit(resume_command(pid(pane))));
        }
    }

    /// A pane whose client is gone waits for its next one; its mirror is
    /// attached again from a capture then.
    fn park(&mut self, pane: TmuxPaneId, generation: PaneGeneration) {
        *self.attaches.entry(pane).or_default() += 1;
        self.backlog.remove(&pane);
        self.leave_source(pane);
        self.flows.insert(pane, Flow::Queued { generation });
        self.waiting.insert(pane);
        if self.attaching.contains(&pane) {
            self.attached(pane);
        }
    }

    /// Keep or make a control client for exactly these sessions (the main
    /// connection's is never dropped here: it ends with its session).
    fn update_clients(&mut self, sessions: &BTreeSet<u32>) {
        let gone: Vec<u32> = self
            .clients
            .keys()
            .filter(|s| **s != self.primary && !sessions.contains(s))
            .copied()
            .collect();
        for s in gone {
            self.drop_client(s);
        }
        for s in sessions {
            if !self.clients.contains_key(s) {
                self.spawn_client(*s);
            }
        }
    }

    /// Forget a client (dropping the last handle ends its tmux client).
    /// Panes it carried lose their source; [`Router::rehome`] moves them.
    fn drop_client(&mut self, session: u32) {
        self.clients.remove(&session);
        self.muted.remove(&session);
        self.sources.retain(|_, s| *s != session);
    }

    fn spawn_client(&mut self, session: u32) {
        let Some(sc) = &self.sessions else { return };
        self.next_serial += 1;
        let serial = self.next_serial;
        self.clients.insert(session, Client::Starting { serial });
        let mut config = sc.connection.clone();
        config.session = format!("${session}");
        config.new_session_args = None;
        let tx = self.streams_tx.clone();
        let back = self.to_self.clone();
        tokio::spawn(async move {
            let conn = match Connection::spawn(config).await {
                Ok((conn, mut events)) => {
                    // Events must flow for responses to arrive.
                    tokio::spawn(async move {
                        while let Some(e) = events.recv().await {
                            let last = matches!(e, ConnEvent::Closed { .. });
                            if tx.send((session, serial, e)).await.is_err() || last {
                                return;
                            }
                        }
                        let _ = tx
                            .send((session, serial, ConnEvent::Closed { error: None }))
                            .await;
                    });
                    // As on the main connection: layouts with floating
                    // panes, and pane process changes for this session.
                    tokio::spawn(
                        conn.submit(TmuxCommand::new("refresh-client").opt("-f", "new-layouts")),
                    );
                    tokio::spawn(conn.submit(pid_subscription()));
                    Ok(conn)
                }
                Err(e) => Err(format!(
                    "cannot attach a control client to tmux session ${session}: {e}"
                )),
            };
            let _ = back
                .send(Control::ClientUp {
                    session,
                    serial,
                    conn,
                })
                .await;
        });
    }

    /// Give every mirrored pane the source it should have: a pane whose
    /// lowest session changed is attached again through that session's
    /// client (or waits for it), and every other client turns its output
    /// off. Each byte then arrives once, and tmux's flow control still
    /// works: tmux keeps reading a pane while any client that was never
    /// told about it is attached, so a busy pane would outrun its source
    /// unless every other client has it off.
    fn rehome(&mut self) {
        if self.sessions.is_none() {
            return;
        }
        let clients: Vec<u32> = self
            .clients
            .iter()
            .filter(|(_, c)| matches!(c, Client::Up { .. }))
            .map(|(s, _)| *s)
            .collect();
        let panes: Vec<(TmuxPaneId, PaneGeneration)> = self
            .flows
            .iter()
            .map(|(p, f)| (*p, f.generation()))
            .collect();
        let mut mute: HashMap<u32, Vec<TmuxPaneId>> = HashMap::new();
        for (p, g) in panes {
            let want = self.wanted(p);
            let up = self.conn_of(want).is_some();
            let moved = self.sources.get(&p) != Some(&want);
            match self.flows.get(&p) {
                // No mirror to rebuild, but the source must keep taking the
                // pane's output, or tmux could stop reading the pane.
                Some(Flow::Cold { .. }) => {
                    if moved && up {
                        self.leave_source(p);
                        self.unmute(want, p);
                        self.sources.insert(p, want);
                    }
                }
                Some(Flow::Queued { .. }) => {
                    if up && self.waiting.remove(&p) {
                        self.start(p, g);
                    }
                }
                Some(_) if moved => {
                    if up {
                        self.start(p, g);
                    } else {
                        self.park(p, g);
                    }
                }
                _ => {}
            }
            let Some(source) = self.sources.get(&p).copied() else {
                continue;
            };
            for s in &clients {
                if *s != source && self.muted.entry(*s).or_default().insert(p) {
                    mute.entry(*s).or_default().push(p);
                }
            }
        }
        for (s, panes) in mute {
            if let Some(conn) = self.conn_of(s) {
                let batch = panes.iter().fold(Batch::new(), |b, p| {
                    b.push(pane_output_command(pid(*p), false))
                });
                tokio::spawn(conn.submit(batch));
            }
        }
    }

    /// An event from a session's client.
    async fn on_stream(&mut self, session: u32, serial: u64, e: ConnEvent) {
        if self.clients.get(&session).map(Client::serial) != Some(serial) {
            return;
        }
        match e {
            ConnEvent::Notification(n) => self.on_notification(session, n).await,
            ConnEvent::Read { read, decoded } => self.read_at = Some((read, decoded)),
            ConnEvent::LateOutcome { .. } => {}
            // Its session was killed, or the client failed: its panes move
            // to another client, and the owner re-reads tmux.
            ConnEvent::Closed { .. } => {
                self.drop_client(session);
                self.rehome();
                self.changed();
            }
        }
    }

    async fn stop(&mut self, pane: TmuxPaneId) {
        match self.flows.remove(&pane) {
            Some(Flow::Live { .. }) | Some(Flow::Failed { .. }) => {
                let _ = self.worker(pane).urgent(WorkerMsg::Drop { pane });
            }
            Some(Flow::Pausing { .. } | Flow::Buffering { .. }) => {
                // Never leave a pane paused for its client.
                self.leave_source(pane);
            }
            Some(Flow::Cold { .. }) => self.set_cold(pane, false),
            Some(Flow::Queued { .. }) => {}
            None => {}
        }
        self.sizes.remove(&pane);
        self.sources.remove(&pane);
        self.waiting.remove(&pane);
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
            self.drain_pane(pane);
        }
    }

    /// Hand as much of one pane's held output to its worker as its lane
    /// has room for, oldest first.
    fn drain_pane(&mut self, pane: TmuxPaneId) {
        let Some(Flow::Live { generation }) = self.flows.get(&pane) else {
            self.backlog.remove(&pane);
            return;
        };
        let generation = *generation;
        let attach = self.attaches.get(&pane).copied().unwrap_or_default();
        let worker = &self.workers[worker_of(pane, self.workers.len())];
        let Some((queue, bytes)) = self.backlog.get_mut(&pane) else {
            return;
        };
        while !queue.is_empty() {
            match worker.try_reserve() {
                Ok(slot) => {
                    let Some((record, at)) = queue.pop_front() else {
                        break;
                    };
                    *bytes -= held_cost(&record);
                    slot.send(WorkerMsg::Apply {
                        pane,
                        generation,
                        attach,
                        record,
                        at,
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
        // Diagnostics: when this record was decoded, for the owner to time
        // its route; nothing while they are off.
        let at = if perf::on() {
            self.read_at.map(|(_, decoded)| decoded)
        } else {
            self.read_at = None;
            if !self.inputs.is_empty() {
                self.inputs.clear();
            }
            None
        };
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
                        at,
                    });
                    return;
                }
                let (queue, bytes) = self.backlog.entry(pane).or_default();
                *bytes += held_cost(&record);
                queue.push_back((record, at));
                // Behind what is held, in order; whatever the lane has room
                // for goes now, so a pane that was held once (a look took
                // its worker for a moment) catches up at its worker's pace
                // instead of waiting for the next tick.
                self.drain_pane(pane);
                let held = self.backlog.get(&pane).map_or(0, |(_, b)| *b);
                if held > BACKLOG_BYTES {
                    perf::count(perf::Counter::Reattaches, 1);
                    self.start(pane, generation);
                    self.changed();
                }
            }
            Some(Flow::Cold { .. }) => {
                perf::count(perf::Counter::ColdDroppedBytes, record_size(&record) as u64);
            }
            // Pausing: already in the coming capture. Failed or unmirrored:
            // nothing to keep.
            _ => {}
        }
    }

    /// A notification from the client of session `from`. Pane output,
    /// pauses and sizes count only from the pane's source.
    async fn on_notification(&mut self, from: u32, n: Notification) {
        if affects_structure(&n) {
            self.changed();
        }
        match n {
            Notification::Output { pane, data }
            | Notification::ExtendedOutput { pane, data, .. } => {
                let p = TmuxPaneId(pane.0);
                if !self.is_source(p, from) {
                    return;
                }
                if !self.inputs.is_empty()
                    && let Some(sent) = self.inputs.remove(&p)
                    && let Some((read, _)) = self.read_at
                    && perf::on()
                {
                    perf::record(perf::Stage::Tmux, read.saturating_duration_since(sent));
                }
                self.record(p, Record::Bytes(data)).await;
            }
            Notification::Pause { pane } => {
                let p = TmuxPaneId(pane.0);
                if !self.is_source(p, from) {
                    return;
                }
                if let Some(Flow::Pausing { generation }) = self.flows.get(&p)
                    && let Some(conn) = self.conn_of(from).cloned()
                {
                    let generation = *generation;
                    let attach = self.attaches.get(&p).copied().unwrap_or_default();
                    self.flows.insert(
                        p,
                        Flow::Buffering {
                            generation,
                            records: Vec::new(),
                            continued: false,
                            captured: None,
                        },
                    );
                    let phase2 = conn.submit(phase2_batch(pane));
                    let back = self.to_self.clone();
                    tokio::spawn(async move {
                        let result = match phase2.await {
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
                                attach,
                                result,
                            })
                            .await;
                    });
                }
            }
            Notification::Continue { pane } => {
                let p = TmuxPaneId(pane.0);
                if !self.is_source(p, from) {
                    return;
                }
                if let Some(Flow::Buffering { continued, .. }) = self.flows.get_mut(&p) {
                    *continued = true;
                }
                self.try_finish(p).await;
            }
            Notification::LayoutChange { visible_layout, .. } => {
                if let WindowLayout::Tiled(node) = parse_layout(&visible_layout) {
                    for (p, rect) in node.panes() {
                        if !self.is_source(p, from) {
                            continue;
                        }
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
            // Server-wide: every client reports it; the main one is enough.
            Notification::PasteBufferChanged { name } if from == self.primary => {
                if let Some(tx) = &self.buffers {
                    tx.send_modify(|(n, last)| {
                        *n += 1;
                        *last = name;
                    });
                }
            }
            _ => {}
        }
    }
}

/// Memory a held record costs: its slot in the pane's queue plus the
/// allocation holding its bytes, as the journal counts it.
fn held_cost(r: &Record) -> usize {
    std::mem::size_of::<(Record, Option<std::time::Instant>)>() + crate::journal::heap_size(r)
}

fn record_size(r: &Record) -> usize {
    match r {
        Record::Bytes(b) => b.len(),
        Record::Resize { .. } => 0,
    }
}
