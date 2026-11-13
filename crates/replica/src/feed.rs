//! A pane feed for a view (spec 9.5, 16.2): a dedicated owner thread keeps
//! a replica current and publishes frames to whoever paints them.
//!
//! - A frame is built only when records were applied or a checkpoint was
//!   restored. An idle pane costs no wakeups: the owner waits on the
//!   connection with no timeout and the view waits on its wake channel.
//! - Frames coalesce: the newest replaces any unpainted one in a
//!   latest-value slot, and the wake channel holds at most one signal.
//! - A synchronized-output hold delays publishing for at most
//!   [`HOLD_LIMIT`]; then the newest frame is shown anyway, so a broken
//!   application cannot freeze the view.
//! - Scrolling is local: the view asks the owner to move the replica's
//!   viewport and gets a new frame without any daemon round trip. Typing
//!   returns a scrolled view to the bottom.
//! - Selection is local too: gestures go to the owner, which applies them
//!   to the replica, scrolls while a drag is held past an edge, and hands
//!   back the selected text on request.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use terminal_ghostty::core::FrameSnapshot;

use crate::{FollowError, FollowState, Follower, RetryPolicy};

pub const HOLD_LIMIT: Duration = Duration::from_secs(1);

pub use terminal_ghostty::Scroll;
pub use terminal_ghostty::links::Link;
pub use terminal_ghostty::selection::{CellPos, SelectEvent};

/// How often a drag held past an edge scrolls one row.
pub const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// Local view changes, applied by the owner to its replica.
#[derive(Debug)]
pub enum ViewCommand {
    Scroll(Scroll),
    /// Back to the bottom if scrolled (sent before typed input).
    Follow,
    /// A selection gesture event.
    Select(SelectEvent),
    ClearSelection,
    /// The selected text, if any.
    Copy(tokio::sync::oneshot::Sender<Option<String>>),
    /// The hyperlink at a viewport cell, if any.
    Link {
        col: u16,
        row: u16,
        reply: tokio::sync::oneshot::Sender<Option<Link>>,
    },
}

/// Where a feed stands, for the view to show honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedStatus {
    Connecting,
    Following,
    /// Lost the daemon; retrying.
    Reconnecting(String),
    PaneClosed,
    UnableToCatchUp(String),
}

#[derive(Debug, Default)]
pub struct FeedStats {
    /// Times the owner thread woke to process data.
    pub owner_wakeups: AtomicU64,
    /// Frames built from the engine.
    pub frames: AtomicU64,
    /// Wake signals sent to the view (coalesced).
    pub view_wakeups: AtomicU64,
    /// Holds that hit the limit and were shown anyway.
    pub holds_expired: AtomicU64,
}

#[derive(Debug)]
pub struct Feed {
    input: tokio::sync::mpsc::UnboundedSender<api::terminal_input::Event>,
    view: tokio::sync::mpsc::UnboundedSender<ViewCommand>,
    /// Input sent, and input dropped because the daemon was unreachable.
    pub inputs_sent: Arc<AtomicU64>,
    pub inputs_dropped: Arc<AtomicU64>,
    latest: Arc<Mutex<Option<Arc<FrameSnapshot>>>>,
    status: Arc<Mutex<FeedStatus>>,
    pub stats: Arc<FeedStats>,
    /// The pane's title and working directory, and bells rung by live
    /// output that the view has not shown yet.
    meta: Arc<Mutex<crate::PaneMeta>>,
    bells: Arc<AtomicU64>,
    clipboard: Arc<Mutex<Option<Vec<u8>>>>,
    /// One signal means "a newer frame is waiting"; never more than one.
    pub wake: async_channel::Receiver<()>,
}

impl Feed {
    /// Follow `pane` through the daemon at `socket` on a new owner thread;
    /// with no pane, the active pane of the first session. With `launch`,
    /// the daemon is started first if it is not running.
    pub fn start(
        socket: PathBuf,
        pane: Option<u32>,
        launch: Option<client::Launch>,
    ) -> std::io::Result<Feed> {
        let latest = Arc::new(Mutex::new(None));
        let meta = Arc::new(Mutex::new(crate::PaneMeta::default()));
        let bells = Arc::new(AtomicU64::new(0));
        let clipboard = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(FeedStatus::Connecting));
        let stats = Arc::new(FeedStats::default());
        let (wake_tx, wake) = async_channel::bounded(1);
        let (input, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (view, view_rx) = tokio::sync::mpsc::unbounded_channel();
        let inputs_sent = Arc::new(AtomicU64::new(0));
        let inputs_dropped = Arc::new(AtomicU64::new(0));
        let target = Arc::new(Target {
            client: client_id(),
            ..Target::default()
        });
        let sender = InputSender {
            socket: socket.clone(),
            target: target.clone(),
            sent: inputs_sent.clone(),
            dropped: inputs_dropped.clone(),
        };
        let owner = Owner {
            target,
            meta: meta.clone(),
            bells: bells.clone(),
            clipboard: clipboard.clone(),
            latest: latest.clone(),
            status: status.clone(),
            stats: stats.clone(),
            wake: wake_tx,
        };
        std::thread::Builder::new()
            .name(format!(
                "dreamer-pane-{}",
                pane.map_or("active".into(), |p| p.to_string())
            ))
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    owner.set(FeedStatus::UnableToCatchUp("no runtime".into()));
                    return;
                };
                rt.spawn(sender.run(input_rx));
                rt.block_on(owner.run(socket, pane, launch, view_rx));
            })?;
        Ok(Feed {
            input,
            view,
            inputs_sent,
            inputs_dropped,
            latest,
            status,
            stats,
            meta,
            bells,
            clipboard,
            wake,
        })
    }

    /// Send input to the pane: encoded by the daemon against the pane's
    /// state and refused if the pane's process has been replaced since
    /// this feed last saw it. Never queued for replay across a lost
    /// connection.
    pub fn send(&self, event: api::terminal_input::Event) {
        use api::terminal_input::Event;
        if matches!(event, Event::Key(_) | Event::Text(_) | Event::Paste(_)) {
            let _ = self.view.send(ViewCommand::Follow);
        }
        let _ = self.input.send(event);
    }

    /// Change the local view; the owner publishes the resulting frame.
    pub fn view(&self, command: ViewCommand) {
        let _ = self.view.send(command);
    }

    /// The OSC 8 link at a viewport cell and the cells it covers, asked of
    /// the owner thread. Hit testing only: opening is the caller's choice.
    pub async fn link_at(&self, col: u16, row: u16) -> Option<Link> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.view(ViewCommand::Link { col, row, reply });
        rx.await.ok().flatten()
    }

    /// The selected text (wrapped lines joined, trailing blanks trimmed),
    /// asked of the owner thread; never of the daemon.
    pub async fn selection_text(&self) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.view(ViewCommand::Copy(tx));
        rx.await.ok().flatten()
    }

    /// The pane's current title and working directory.
    pub fn meta(&self) -> crate::PaneMeta {
        self.meta.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Text the program wrote to the clipboard since the last call, if the
    /// daemon addressed it to this window (its focused client).
    pub fn take_clipboard(&self) -> Option<Vec<u8>> {
        self.clipboard.lock().ok().and_then(|mut c| c.take())
    }

    /// Bells rung since the last call (live output only).
    pub fn take_bells(&self) -> u64 {
        self.bells.swap(0, Ordering::Relaxed)
    }

    /// The newest unpainted frame, if any.
    pub fn take(&self) -> Option<Arc<FrameSnapshot>> {
        self.latest.lock().ok().and_then(|mut l| l.take())
    }

    pub fn status(&self) -> FeedStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(FeedStatus::Connecting)
    }
}

/// The pane being followed and its process generation (0 until known).
#[derive(Debug, Default)]
struct Target {
    /// This window's client id, sent with its subscription and its input.
    client: u64,
    pane: AtomicU64,
    generation: AtomicU64,
    known: std::sync::atomic::AtomicBool,
}

/// Sends input on its own connection, so input never waits for, or
/// interrupts, the follow loop.
struct InputSender {
    socket: PathBuf,
    target: Arc<Target>,
    sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl InputSender {
    async fn run(self, mut rx: tokio::sync::mpsc::UnboundedReceiver<api::terminal_input::Event>) {
        let mut client: Option<client::Client> = None;
        while let Some(event) = rx.recv().await {
            if !self.target.known.load(Ordering::Acquire) {
                // No pane yet: nothing to type into.
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if client.is_none() {
                client = client::Client::connect(&self.socket, "gui-input")
                    .await
                    .ok();
            }
            let Some(c) = client.as_ref() else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let pane = self.target.pane.load(Ordering::Acquire) as u32;
            let generation =
                Some(self.target.generation.load(Ordering::Acquire)).filter(|g| *g != 0);
            let result = match event {
                api::terminal_input::Event::Paste(p) => {
                    client::paste(c, pane, generation, &p.data).await.map(drop)
                }
                other => c
                    .call(api::Request::TerminalInput(api::TerminalInput {
                        pane,
                        generation,
                        endpoint: None,
                        client: Some(self.target.client),
                        event: Some(other),
                    }))
                    .await
                    .map(drop),
            };
            match result {
                Ok(()) => {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    // Input is never replayed after an uncertain failure
                    // (spec 12.4); the next input reconnects.
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    if matches!(
                        e,
                        client::ClientError::Wire(_) | client::ClientError::NotRunning(_)
                    ) {
                        client = None;
                    }
                }
            }
        }
    }
}

struct Owner {
    target: Arc<Target>,
    meta: Arc<Mutex<crate::PaneMeta>>,
    bells: Arc<AtomicU64>,
    clipboard: Arc<Mutex<Option<Vec<u8>>>>,
    latest: Arc<Mutex<Option<Arc<FrameSnapshot>>>>,
    status: Arc<Mutex<FeedStatus>>,
    stats: Arc<FeedStats>,
    wake: async_channel::Sender<()>,
}

impl Owner {
    fn set(&self, s: FeedStatus) {
        if let Ok(mut st) = self.status.lock() {
            *st = s;
        }
    }

    fn publish(&self, frame: Arc<FrameSnapshot>) {
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(frame);
        }
        // Full means a wake is already pending: the view takes the newest.
        if self.wake.try_send(()).is_ok() {
            self.stats.view_wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn connect(&self, socket: &std::path::Path, pane: u32) -> Follower {
        let mut backoff = Duration::from_millis(100);
        loop {
            match Follower::start_as(
                socket.to_path_buf(),
                pane,
                RetryPolicy::default(),
                Some(self.target.client),
            )
            .await
            {
                Ok(f) => return f,
                Err(e) => {
                    self.set(FeedStatus::Reconnecting(e.to_string()));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
        }
    }

    /// Show a message in place of a pane (no tmux server, daemon errors).
    fn message(&self, text: &str) {
        use terminal_ghostty::core::GridSize;
        use terminal_ghostty::{Engine, EngineConfig};
        let mut config = EngineConfig::new(GridSize { cols: 100, rows: 6 });
        config.capture_replies = false;
        let Ok(mut e) = Engine::new(config) else {
            return;
        };
        let body = format!("\x1b[2mdreamer\x1b[0m\r\n{}", text.replace('\n', "\r\n"));
        if e.feed(body.as_bytes()).is_ok()
            && let Ok(frame) = e.frame()
        {
            self.publish(frame);
        }
    }

    /// The active pane of the first session, asked of the daemon.
    async fn active_pane(&self, socket: &std::path::Path) -> Result<u32, String> {
        let c = client::Client::connect(socket, "gui")
            .await
            .map_err(|e| e.to_string())?;
        let api::Response::TmuxTopology(t) = c
            .call(api::Request::TmuxTopology(api::TopologyParams::default()))
            .await
            .map_err(|e| e.to_string())?
        else {
            return Err("unexpected reply".into());
        };
        let session = t.sessions.first().ok_or("tmux has no sessions")?;
        let window = session
            .active_window
            .and_then(|w| t.windows.iter().find(|x| x.id == w))
            .ok_or("tmux reports no active window")?;
        window
            .active_pane
            .ok_or_else(|| "tmux reports no active pane".into())
    }

    /// Apply a local view change; publish only if the frame changed.
    /// `autoscroll` is where a drag held past an edge continues from.
    fn view(&self, f: &mut Follower, command: ViewCommand, autoscroll: &mut Option<CellPos>) {
        use terminal_ghostty::selection::Autoscroll;
        let Some(Ok(before)) = f.frame() else {
            match command {
                ViewCommand::Copy(reply) => drop(reply.send(None)),
                ViewCommand::Link { reply, .. } => drop(reply.send(None)),
                _ => {}
            }
            return;
        };
        let Some(engine) = f.replica_mut().engine_mut() else {
            return;
        };
        let applied = match command {
            ViewCommand::Scroll(to) => engine.scroll_viewport(to).is_ok(),
            ViewCommand::Follow if before.view.rows_back() > 0 => {
                engine.scroll_viewport(Scroll::Bottom).is_ok()
            }
            ViewCommand::Follow => false,
            ViewCommand::Select(event) => {
                let ok = engine.select(event).is_ok();
                *autoscroll = match event {
                    SelectEvent::Drag { at } | SelectEvent::AutoscrollTick { at }
                        if engine.selection_autoscroll().ok() != Some(Autoscroll::None) =>
                    {
                        Some(at)
                    }
                    _ => None,
                };
                ok
            }
            ViewCommand::ClearSelection => engine.clear_selection().is_ok(),
            ViewCommand::Copy(reply) => {
                let _ = reply.send(engine.selection_text().ok().flatten());
                false
            }
            ViewCommand::Link { col, row, reply } => {
                let _ = reply.send(engine.link_at(col, row).ok().flatten());
                false
            }
        };
        if !applied {
            return;
        }
        if let Some(Ok(frame)) = f.frame()
            && !Arc::ptr_eq(&frame, &before)
        {
            self.publish(frame);
        }
    }

    async fn run(
        self,
        socket: PathBuf,
        pane: Option<u32>,
        launch: Option<client::Launch>,
        mut view_rx: tokio::sync::mpsc::UnboundedReceiver<ViewCommand>,
    ) {
        if let Some(l) = &launch
            && let Err(e) = client::ensure_running(l, "gui").await
        {
            self.set(FeedStatus::Reconnecting(e.to_string()));
            self.message(&format!("cannot start dreamerd: {e}"));
        }
        let pane = match pane {
            Some(p) => p,
            None => {
                let mut backoff = Duration::from_millis(200);
                loop {
                    match self.active_pane(&socket).await {
                        Ok(p) => break p,
                        Err(e) => {
                            self.set(FeedStatus::Reconnecting(e.clone()));
                            self.message(&format!(
                                "no tmux pane to show: {e}\nstart tmux (or a session) and this window will follow it"
                            ));
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(5));
                        }
                    }
                }
            }
        };
        let mut f = self.connect(&socket, pane).await;
        self.set(FeedStatus::Following);
        self.target.pane.store(u64::from(pane), Ordering::Release);
        let mut held_since: Option<tokio::time::Instant> = None;
        let mut autoscroll: Option<CellPos> = None;
        enum Wake {
            Pushed(Result<Option<client::Pushed>, FollowError>),
            View(ViewCommand),
            HoldExpired,
            Autoscroll(CellPos),
        }
        loop {
            // The view dropped its feed (a pane no longer shown): stop, and
            // with this thread its subscription and connections.
            if view_rx.is_closed() {
                return;
            }
            let step = if matches!(f.state, FollowState::Following) {
                let hold = async {
                    match held_since {
                        Some(t) => tokio::time::sleep_until(t + HOLD_LIMIT).await,
                        None => std::future::pending().await,
                    }
                };
                let tick = async {
                    match autoscroll {
                        Some(at) => {
                            tokio::time::sleep(AUTOSCROLL_TICK).await;
                            at
                        }
                        None => std::future::pending().await,
                    }
                };
                // `recv` is cancel-safe: losing the race drops nothing.
                let wake = tokio::select! {
                    r = f.recv() => Wake::Pushed(r),
                    c = view_rx.recv() => match c {
                        Some(c) => Wake::View(c),
                        // The view dropped its feed: stop following.
                        None => return,
                    },
                    () = hold => Wake::HoldExpired,
                    at = tick => Wake::Autoscroll(at),
                };
                match wake {
                    Wake::Pushed(Ok(first)) => f.apply(first).await,
                    Wake::Pushed(Err(e)) => Err(e),
                    Wake::View(c) => {
                        self.view(&mut f, c, &mut autoscroll);
                        continue;
                    }
                    Wake::Autoscroll(at) => {
                        let tick = ViewCommand::Select(SelectEvent::AutoscrollTick { at });
                        self.view(&mut f, tick, &mut autoscroll);
                        continue;
                    }
                    Wake::HoldExpired => {
                        // The application held output too long: show it.
                        self.stats.holds_expired.fetch_add(1, Ordering::Relaxed);
                        held_since = None;
                        if let Some(Ok(frame)) = f.frame() {
                            self.publish(frame);
                        }
                        continue;
                    }
                }
            } else {
                f.step().await
            };
            self.stats.owner_wakeups.fetch_add(1, Ordering::Relaxed);
            // Labels and bells reach the view with the next frame; with no
            // new records, the current frame is published again to wake it.
            let mut wake_view = false;
            if let Ok(s) = &step {
                if s.meta_changed {
                    if let Ok(mut m) = self.meta.lock() {
                        *m = f.meta.clone();
                    }
                    wake_view = true;
                }
                if s.bells > 0 {
                    self.bells.fetch_add(u64::from(s.bells), Ordering::Relaxed);
                    wake_view = true;
                }
                if let Some(text) = f.take_clipboard() {
                    if let Ok(mut c) = self.clipboard.lock() {
                        *c = Some(text);
                    }
                    wake_view = true;
                }
            }
            if wake_view
                && let Ok(s) = &step
                && s.records == 0
                && !s.restored
                && let Some(Ok(frame)) = f.frame()
            {
                self.publish(frame);
            }
            match step {
                Ok(s) if s.records > 0 || s.restored => {
                    if let Some((_, generation, _)) = f.replica().position() {
                        self.target.generation.store(generation, Ordering::Release);
                        self.target.known.store(true, Ordering::Release);
                    }
                    let Some(Ok(frame)) = f.frame() else { continue };
                    self.stats.frames.fetch_add(1, Ordering::Relaxed);
                    if frame.render_hold && !s.restored {
                        // Not shown yet; a later frame or the limit shows it.
                        held_since.get_or_insert_with(tokio::time::Instant::now);
                        continue;
                    }
                    held_since = None;
                    self.publish(frame);
                }
                Ok(_) => match &f.state {
                    FollowState::Following => {}
                    FollowState::PaneClosed => {
                        self.set(FeedStatus::PaneClosed);
                        return;
                    }
                    FollowState::UnableToCatchUp(why) => {
                        self.set(FeedStatus::UnableToCatchUp(why.clone()));
                        return;
                    }
                    FollowState::Disconnected => self.reconnect(&mut f).await,
                },
                Err(FollowError::Disconnected) => self.reconnect(&mut f).await,
                Err(e) => {
                    self.set(FeedStatus::Reconnecting(e.to_string()));
                    self.reconnect(&mut f).await;
                }
            }
        }
    }

    async fn reconnect(&self, f: &mut Follower) {
        let mut backoff = Duration::from_millis(100);
        loop {
            self.set(FeedStatus::Reconnecting(
                "the daemon connection ended".into(),
            ));
            tokio::time::sleep(backoff).await;
            if f.reconnect().await.is_ok() {
                self.set(FeedStatus::Following);
                return;
            }
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }
}

/// An id for this window as a client of the daemon: unique enough across
/// the windows and processes attached to one daemon.
fn client_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    (u64::from(std::process::id()) << 32) ^ nanos ^ (N.fetch_add(1, Ordering::Relaxed) << 48)
}
