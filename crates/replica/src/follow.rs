//! Following a pane over a daemon connection.

use std::path::PathBuf;
use std::time::Duration;

use api::{Request, Response};
use client::{Client, ClientError, Pushed};
use prost::Message as _;
use tokio::sync::mpsc::Receiver;

use crate::{Outcome, Replica, ReplicaError};

/// Bounds on recovering by resnapshot (spec 11.6: no endless loops).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Resnapshots in a row before giving up as unable to catch up.
    pub max_resnapshots: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_resnapshots: 5,
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowState {
    Following,
    /// The daemon connection ended; call [`Follower::reconnect`].
    Disconnected,
    PaneClosed,
    /// Resnapshots kept failing; the view is stale and says so.
    UnableToCatchUp(String),
}

#[derive(Debug)]
pub enum FollowError {
    Client(ClientError),
    Replica(ReplicaError),
    Disconnected,
}

impl std::fmt::Display for FollowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FollowError::Client(e) => write!(f, "{e}"),
            FollowError::Replica(e) => write!(f, "{e}"),
            FollowError::Disconnected => write!(f, "the daemon connection ended"),
        }
    }
}

impl std::error::Error for FollowError {}

impl From<ClientError> for FollowError {
    fn from(e: ClientError) -> Self {
        FollowError::Client(e)
    }
}

/// What one [`Follower::step`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Step {
    pub records: usize,
    /// A checkpoint was restored (the whole view changed).
    pub restored: bool,
    /// Bells rung by live output during this step (never from replay,
    /// restore, or reconnect).
    pub bells: u32,
    /// The pane's title or working directory changed.
    pub meta_changed: bool,
}

/// A pane's labels: state from the daemon, current after every
/// (re)subscription, updated by live effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneMeta {
    pub title: Option<String>,
    pub cwd: Option<String>,
}

/// A replica kept current from a daemon. Not `Send`: the engine has one
/// owner thread.
#[derive(Debug)]
pub struct Follower {
    socket: PathBuf,
    replica: Replica,
    client: Option<Client>,
    pushed: Option<Receiver<Pushed>>,
    subscription: Option<u64>,
    policy: RetryPolicy,
    resnapshots: u32,
    pub state: FollowState,
    /// How the current subscription started (checkpoint or replay).
    pub last_mode: Option<api::SubscribeMode>,
    /// Checkpoints restored over this follower's life.
    pub checkpoints: u32,
    pub meta: PaneMeta,
    /// This client's id, sent with every subscription.
    client_id: Option<u64>,
    /// A clipboard write addressed to this client, not yet taken.
    clipboard: Option<Vec<u8>>,
    /// Effects of the current step, collected by `handle`.
    bells: u32,
    meta_changed: bool,
}

impl Follower {
    pub async fn start(
        socket: PathBuf,
        pane: u32,
        policy: RetryPolicy,
    ) -> Result<Follower, FollowError> {
        Follower::start_as(socket, pane, policy, None).await
    }

    /// Like [`Follower::start`], naming this client so the daemon can
    /// address effects (clipboard writes) to it.
    pub async fn start_as(
        socket: PathBuf,
        pane: u32,
        policy: RetryPolicy,
        client: Option<u64>,
    ) -> Result<Follower, FollowError> {
        let mut f = Follower {
            client_id: client,
            clipboard: None,
            socket,
            replica: Replica::new(pane),
            client: None,
            pushed: None,
            subscription: None,
            policy,
            resnapshots: 0,
            state: FollowState::Disconnected,
            last_mode: None,
            checkpoints: 0,
            meta: PaneMeta::default(),
            bells: 0,
            meta_changed: false,
        };
        f.reconnect().await?;
        Ok(f)
    }

    pub fn replica(&self) -> &Replica {
        &self.replica
    }

    /// Drop the daemon connection (a viewer going away); the replica stays.
    pub fn disconnect(&mut self) {
        self.client = None;
        self.pushed = None;
        self.subscription = None;
        self.state = FollowState::Disconnected;
    }

    /// Connect again and continue from where the replica is: a replay when
    /// the daemon still holds the records, a new checkpoint otherwise.
    pub async fn reconnect(&mut self) -> Result<(), FollowError> {
        let mut client = Client::connect(&self.socket, "replica").await?;
        self.pushed = client.pushed();
        self.client = Some(client);
        self.subscribe(true).await
    }

    async fn subscribe(&mut self, resume: bool) -> Result<(), FollowError> {
        let client = self.client.as_ref().ok_or(FollowError::Disconnected)?;
        if let Some(old) = self.subscription.take() {
            let _ = client
                .call(Request::TerminalUnsubscribe(api::Unsubscribe {
                    subscription: old,
                }))
                .await;
        }
        let pos = if resume {
            self.replica.position()
        } else {
            None
        };
        let req = api::TerminalSubscribe {
            pane: self.replica.pane,
            resume_epoch: pos.map(|p| p.0),
            resume_generation: pos.map(|p| p.1),
            resume_sequence: pos.map(|p| p.2),
            client: self.client_id,
            ..api::TerminalSubscribe::default()
        };
        let Response::TerminalSubscribe(s) = client.call(Request::TerminalSubscribe(req)).await?
        else {
            return Err(FollowError::Disconnected);
        };
        self.last_mode = api::SubscribeMode::try_from(s.mode).ok();
        self.subscription = Some(s.id);
        // Labels are state: the subscription carries their current values,
        // so a reconnect never needs old events.
        let meta = PaneMeta {
            title: s.title.clone(),
            cwd: s.cwd.clone(),
        };
        if meta != self.meta {
            self.meta = meta;
            self.meta_changed = true;
        }
        self.state = FollowState::Following;
        self.replica.begin(&s).map_err(FollowError::Replica)
    }

    /// Give up the current stream and start from a fresh checkpoint, with
    /// bounded retries and backoff.
    async fn resnapshot(&mut self, why: String) -> Result<(), FollowError> {
        self.resnapshots += 1;
        if self.resnapshots > self.policy.max_resnapshots {
            self.state = FollowState::UnableToCatchUp(why);
            return Ok(());
        }
        let backoff = self
            .policy
            .initial_backoff
            .saturating_mul(1 << (self.resnapshots - 1).min(16))
            .min(self.policy.max_backoff);
        tokio::time::sleep(backoff).await;
        self.subscribe(false).await
    }

    /// Apply what the daemon pushes for up to `wait` of quiet. Returns the
    /// records applied.
    pub async fn pump(&mut self, wait: Duration) -> Result<usize, FollowError> {
        let mut applied = 0;
        loop {
            if !matches!(self.state, FollowState::Following) {
                return Ok(applied);
            }
            let Some(pushed) = self.pushed.as_mut() else {
                return Err(FollowError::Disconnected);
            };
            let Ok(next) = tokio::time::timeout(wait, pushed.recv()).await else {
                return Ok(applied);
            };
            applied += self.handle(next).await?;
        }
    }

    /// Wait for the next thing the daemon pushes, with no timeout, apply it
    /// and everything already queued behind it, and return what changed.
    /// An idle pane costs no wakeups here.
    pub async fn step(&mut self) -> Result<Step, FollowError> {
        if !matches!(self.state, FollowState::Following) {
            return Ok(Step::default());
        }
        let first = self.recv().await?;
        self.apply(first).await
    }

    /// Wait for the next pushed item without applying it. Cancel-safe:
    /// dropping the future loses nothing, so it can race other work.
    pub async fn recv(&mut self) -> Result<Option<Pushed>, FollowError> {
        match self.pushed.as_mut() {
            Some(p) => Ok(p.recv().await),
            None => Err(FollowError::Disconnected),
        }
    }

    /// Apply an item from [`Self::recv`] and everything queued behind it.
    pub async fn apply(&mut self, first: Option<Pushed>) -> Result<Step, FollowError> {
        let checkpoints = self.checkpoints;
        self.bells = 0;
        let mut records = self.handle(first).await?;
        loop {
            if !matches!(self.state, FollowState::Following) {
                break;
            }
            let Some(p) = self.pushed.as_mut() else { break };
            match p.try_recv() {
                Ok(next) => records += self.handle(Some(next)).await?,
                Err(_) => break,
            }
        }
        Ok(Step {
            records,
            restored: self.checkpoints != checkpoints,
            bells: std::mem::take(&mut self.bells),
            meta_changed: std::mem::take(&mut self.meta_changed),
        })
    }

    /// The latest clipboard write addressed to this client, once.
    pub fn take_clipboard(&mut self) -> Option<Vec<u8>> {
        self.clipboard.take()
    }

    /// The replica, for local view state (see [`Replica::engine_mut`]).
    pub fn replica_mut(&mut self) -> &mut Replica {
        &mut self.replica
    }

    /// Move the replica's local viewport (see [`Replica::scroll`]).
    pub fn scroll(&mut self, to: terminal_ghostty::Scroll) -> Result<(), String> {
        self.replica.scroll(to)
    }

    /// The replica's current frame, when it holds an engine.
    pub fn frame(
        &mut self,
    ) -> Option<Result<std::sync::Arc<terminal_ghostty::core::FrameSnapshot>, String>> {
        self.replica.frame()
    }

    async fn handle(&mut self, next: Option<Pushed>) -> Result<usize, FollowError> {
        let mut applied = 0;
        let sub = self.subscription;
        match next {
            None | Some(Pushed::Closed { .. }) => {
                self.client = None;
                self.pushed = None;
                self.subscription = None;
                self.state = FollowState::Disconnected;
                return Err(FollowError::Disconnected);
            }
            Some(Pushed::Bulk { stream, data, .. }) if Some(stream) == sub => {
                let Ok(d) = api::CheckpointData::decode(data.as_slice()) else {
                    self.resnapshot("undecodable checkpoint".into()).await?;
                    return Ok(0);
                };
                match self.replica.checkpoint(&d) {
                    Ok(Outcome::Applied(n)) => {
                        applied += n;
                        self.checkpoints += 1;
                        self.resnapshots = 0;
                    }
                    Ok(_) => {}
                    Err(e) => self.resnapshot(e.to_string()).await?,
                }
            }
            Some(Pushed::Event { topic, payload, .. }) if topic == "terminal.records" => {
                let Ok(b) = api::RecordBatch::decode(payload.as_slice()) else {
                    self.resnapshot("undecodable records".into()).await?;
                    return Ok(0);
                };
                match self.replica.records(&b) {
                    Ok(Outcome::Applied(n)) => {
                        applied += n;
                        // Effects ride only with live records the replica
                        // applied, so a duplicate batch rings nothing.
                        for e in &b.effects {
                            self.bells = self.bells.saturating_add(e.bells);
                            if e.title.is_some() && e.title != self.meta.title {
                                self.meta.title.clone_from(&e.title);
                                self.meta_changed = true;
                            }
                            if e.clipboard.is_some() {
                                self.clipboard.clone_from(&e.clipboard);
                            }
                            if e.cwd.is_some() && e.cwd != self.meta.cwd {
                                self.meta.cwd.clone_from(&e.cwd);
                                self.meta_changed = true;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => self.resnapshot(e.to_string()).await?,
                }
            }
            Some(Pushed::Event { topic, payload, .. }) if topic == "terminal.end" => {
                let Ok(end) = api::SubscriptionEnd::decode(payload.as_slice()) else {
                    return Ok(0);
                };
                if Some(end.subscription) != sub {
                    return Ok(0);
                }
                self.subscription = None;
                match api::EndReason::try_from(end.reason) {
                    // Behind but maybe still retained: resume.
                    Ok(api::EndReason::Lagged) => self.subscribe(true).await?,
                    // A new process: its own checkpoint.
                    Ok(api::EndReason::Respawned) => self.subscribe(false).await?,
                    // The mirror was rebuilt: its journal is new, so start
                    // from a fresh checkpoint.
                    Ok(api::EndReason::Reattached) => self.subscribe(false).await?,
                    Ok(api::EndReason::PaneClosed) => self.state = FollowState::PaneClosed,
                    // The daemon reattaches tmux under a new epoch.
                    Ok(api::EndReason::ConnectionLost) => {
                        self.resnapshot("the daemon lost tmux".into()).await?;
                    }
                    _ => {}
                }
            }
            Some(_) => {}
        }
        Ok(applied)
    }
}
