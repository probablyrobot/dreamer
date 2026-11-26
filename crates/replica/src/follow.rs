//! Following a pane over a daemon connection, on records or on screen
//! frames (`docs/mirrors.md`).
//!
//! On frames the follower keeps a [`ScreenView`] instead of an engine.
//! Switching follows the daemon's sequence rules, and never shows rows over
//! a hole:
//! - to frames: the replica goes on with the records up to the switch,
//!   and gives way to the screen when the full frame at that sequence
//!   arrives;
//! - to records: the screen stays until the checkpoint at the switch's
//!   sequence restores, and the records after it apply on top;
//! - a frame that does not apply (out of order, undecodable) ends the
//!   frames: the follower subscribes again on records, from a checkpoint.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use api::Request;
use client::{Client, ClientError, Pushed};
use prost::Message as _;
use tokio::sync::mpsc::Receiver;

use crate::screen::ScreenView;
use crate::{Outcome, Replica, ReplicaError};

/// Whether a follower's view may be fed screen frames instead of records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Frames {
    /// Records only: the connection does not offer `terminal.frames`.
    #[default]
    Off,
    /// The connection offers `terminal.frames`; a subscription starts on
    /// records and follows a switch.
    On,
    /// Every subscription asks for frames.
    Always,
}

/// A switch the daemon answered whose new feed has not started yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// The full frame numbered `frame`, at `sequence`, comes next.
    ToFrames { sequence: u64, frame: u64 },
    /// The checkpoint at `sequence` comes next.
    ToStream,
}

/// Bounds on recovering by resnapshot (no endless loops).
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
    /// Screen frames applied.
    pub frames: usize,
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
    /// The local daemon's tmux, or a host through its coordinator.
    via: crate::Via,
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
    frames_applied: usize,
    /// Whether frames may be asked for, and the screen while on them.
    frames: Frames,
    screen: Option<ScreenView>,
    switch: Option<Switch>,
    /// A frame did not apply: subscriptions stay on records until the
    /// next reconnect.
    frames_failed: bool,
    /// The current subscription, for a switch back to records.
    started: Option<api::Subscription>,
    /// Pushed data read while a switch waited for its reply.
    backlog: VecDeque<Option<Pushed>>,
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
        Follower::start_via(socket, crate::Via::Local, pane, policy, client).await
    }

    /// Like [`Follower::start_as`], following a pane `via` a host.
    pub async fn start_via(
        socket: PathBuf,
        via: crate::Via,
        pane: u32,
        policy: RetryPolicy,
        client: Option<u64>,
    ) -> Result<Follower, FollowError> {
        Follower::start_frames(socket, via, pane, policy, client, Frames::Off).await
    }

    /// Like [`Follower::start_via`], allowing screen frames as `frames`
    /// says.
    pub async fn start_frames(
        socket: PathBuf,
        via: crate::Via,
        pane: u32,
        policy: RetryPolicy,
        client: Option<u64>,
        frames: Frames,
    ) -> Result<Follower, FollowError> {
        let mut f = Follower {
            client_id: client,
            clipboard: None,
            socket,
            via,
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
            frames_applied: 0,
            frames,
            screen: None,
            switch: None,
            frames_failed: false,
            started: None,
            backlog: VecDeque::new(),
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
        self.switch = None;
        self.backlog.clear();
        self.state = FollowState::Disconnected;
    }

    /// Connect again and continue from where the replica is: a replay when
    /// the daemon still holds the records, a new checkpoint otherwise. On
    /// frames, a full frame.
    pub async fn reconnect(&mut self) -> Result<(), FollowError> {
        let features: &[&str] = match self.frames {
            Frames::Off => &[],
            Frames::On | Frames::Always => &[wire::feature::TERMINAL_FRAMES],
        };
        let mut client = Client::connect_features(&self.socket, "replica", 1024, features).await?;
        self.pushed = client.pushed();
        self.client = Some(client);
        self.switch = None;
        self.backlog.clear();
        self.frames_failed = false;
        self.subscribe(true).await
    }

    /// The connection may carry screen frames.
    pub fn frames_offered(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|c| c.has_feature(wire::feature::TERMINAL_FRAMES))
    }

    /// The view shows screen frames (no engine behind it).
    pub fn on_frames(&self) -> bool {
        self.screen.is_some()
    }

    /// The screen kept from frames, while on them.
    pub fn screen(&self) -> Option<&ScreenView> {
        self.screen.as_ref()
    }

    /// The pane process generation the view shows, once it shows one.
    pub fn generation(&self) -> Option<u64> {
        match &self.screen {
            Some(s) if s.number > 0 => self.started.as_ref().map(|s| s.generation),
            Some(_) => None,
            None => self.replica.position().map(|p| p.1),
        }
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
        let frames = self.frames == Frames::Always
            && !self.frames_failed
            && client.has_feature(wire::feature::TERMINAL_FRAMES);
        let req = api::TerminalSubscribe {
            pane: self.replica.pane,
            resume_epoch: pos.map(|p| p.0),
            resume_generation: pos.map(|p| p.1),
            resume_sequence: pos.map(|p| p.2),
            client: self.client_id,
            feed: if frames {
                api::TerminalFeed::Frames as i32
            } else {
                api::TerminalFeed::Stream as i32
            },
            ..api::TerminalSubscribe::default()
        };
        let s = self.via.subscribe(client, req).await?;
        self.switch = None;
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
        self.started = Some(s.clone());
        if s.mode == api::SubscribeMode::Frames as i32 {
            // The view shown so far stays until the full frame arrives.
            self.screen.get_or_insert_with(ScreenView::new);
            self.switch = Some(Switch::ToFrames {
                sequence: s.sequence,
                frame: s.frame,
            });
            return Ok(());
        }
        self.replica.begin(&s).map_err(FollowError::Replica)
    }

    /// Move the subscription to screen frames, or back to records, by the
    /// daemon's sequence rules (see the module notes). Returns once the
    /// daemon answered; the new feed follows through [`Self::step`].
    pub async fn set_feed(&mut self, frames: bool) -> Result<(), FollowError> {
        let (Some(client), Some(sub), Some(pushed)) = (
            self.client.as_ref(),
            self.subscription,
            self.pushed.as_mut(),
        ) else {
            return Err(FollowError::Disconnected);
        };
        let feed = if frames {
            api::TerminalFeed::Frames
        } else {
            api::TerminalFeed::Stream
        };
        let call = self.via.call(
            client,
            Request::TerminalSetFeed(api::SetFeed {
                subscription: sub,
                feed: feed as i32,
            }),
        );
        tokio::pin!(call);
        // Pushed data keeps being read meanwhile, so a full queue never
        // holds the reply back; it is handled after the reply, in order.
        let reply = loop {
            tokio::select! {
                r = &mut call => break r,
                p = pushed.recv() => {
                    let closed = p.is_none();
                    self.backlog.push_back(p);
                    if closed {
                        break (&mut call).await;
                    }
                }
            }
        };
        let api::Response::TerminalSetFeed(sw) = reply? else {
            return Err(FollowError::Client(ClientError::Wire(
                wire::WireError::Unexpected("a reply to another method"),
            )));
        };
        if sw.mode == api::SubscribeMode::Frames as i32 {
            self.screen.get_or_insert_with(ScreenView::new);
            self.switch = Some(Switch::ToFrames {
                sequence: sw.sequence,
                frame: sw.frame,
            });
            return Ok(());
        }
        let started = self.started.clone().ok_or(FollowError::Disconnected)?;
        self.replica
            .begin(&api::Subscription {
                mode: api::SubscribeMode::Checkpoint as i32,
                sequence: sw.sequence,
                ..started
            })
            .map_err(FollowError::Replica)?;
        self.switch = Some(Switch::ToStream);
        Ok(())
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
        if let Some(p) = self.backlog.pop_front() {
            return Ok(p);
        }
        match self.pushed.as_mut() {
            Some(p) => Ok(p.recv().await),
            None => Err(FollowError::Disconnected),
        }
    }

    /// Apply an item from [`Self::recv`] and everything queued behind it.
    pub async fn apply(&mut self, first: Option<Pushed>) -> Result<Step, FollowError> {
        let checkpoints = self.checkpoints;
        self.bells = 0;
        self.frames_applied = 0;
        let mut records = self.handle(first).await?;
        loop {
            if !matches!(self.state, FollowState::Following) {
                break;
            }
            if let Some(next) = self.backlog.pop_front() {
                records += self.handle(next).await?;
                continue;
            }
            let Some(p) = self.pushed.as_mut() else { break };
            match p.try_recv() {
                Ok(next) => records += self.handle(Some(next)).await?,
                Err(_) => break,
            }
        }
        Ok(Step {
            records,
            frames: std::mem::take(&mut self.frames_applied),
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

    /// The view's current frame: the screen's while on frames, else the
    /// replica's when it holds an engine.
    pub fn frame(
        &mut self,
    ) -> Option<Result<std::sync::Arc<terminal_ghostty::core::FrameSnapshot>, String>> {
        match self.screen.as_mut() {
            // Until its first frame, the replica's view stays.
            Some(s) if s.number > 0 => s.frame().map(Ok),
            _ => self.replica.frame(),
        }
    }

    /// Apply a screen frame; on frames that cannot go on, back to records
    /// from a checkpoint.
    async fn screen_frame(&mut self, data: &[u8]) -> Result<(), FollowError> {
        let f = match api::ScreenFrame::decode(data) {
            Ok(f) if Some(f.subscription) == self.subscription => f,
            Ok(_) => return Ok(()),
            Err(_) => return self.frames_failed("undecodable frame".into()).await,
        };
        let Some(screen) = self.screen.as_mut() else {
            return self
                .frames_failed("a frame with no switch to frames".into())
                .await;
        };
        if let Some(Switch::ToFrames { sequence, frame }) = self.switch
            && (!f.full || (f.number, f.sequence) != (frame, sequence))
        {
            let why = format!(
                "frame {} at {} arrived for the switch to frame {frame} at {sequence}",
                f.number, f.sequence
            );
            return self.frames_failed(why).await;
        }
        if let Err(e) = screen.apply(&f) {
            return self.frames_failed(e.to_string()).await;
        }
        if matches!(self.switch, Some(Switch::ToFrames { .. })) {
            // The screen took over: the engine and its records are done.
            self.switch = None;
            self.replica = Replica::new(self.replica.pane);
        }
        if f.title.is_some() && f.title != self.meta.title {
            self.meta.title.clone_from(&f.title);
            self.meta_changed = true;
        }
        self.frames_applied += 1;
        self.resnapshots = 0;
        Ok(())
    }

    /// Frames cannot go on: subscribe again on records, from a checkpoint.
    async fn frames_failed(&mut self, why: String) -> Result<(), FollowError> {
        self.frames_failed = true;
        self.switch = None;
        self.replica = Replica::new(self.replica.pane);
        self.resnapshot(why).await
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
            Some(Pushed::Bulk {
                stream,
                topic,
                data,
            }) if Some(stream) == sub && topic == api::topic::TERMINAL_FRAME => {
                self.screen_frame(&data).await?;
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
                        // Back on records: the screen gives way. Not to
                        // the stream's last checkpoint before a switch to
                        // frames, which is still waiting for its frame.
                        if !matches!(self.switch, Some(Switch::ToFrames { .. })) {
                            self.screen = None;
                            self.switch = None;
                        }
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
                    // The host's daemon restarted: its journal is new.
                    Ok(api::EndReason::HostRestarted) => self.subscribe(false).await?,
                    // The host is gone for now: reconnect (and resume) later.
                    Ok(api::EndReason::HostLost) => self.disconnect(),
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
