//! A pane feed for a view: a dedicated owner thread keeps
//! a replica current and publishes frames to whoever paints them.
//!
//! - A frame is built only when records were applied or a checkpoint was
//!   restored. An idle pane costs no wakeups: the owner waits on the
//!   connection with no timeout and the view waits on its wake channel.
//! - Frames are paced by presentation, not by the records that arrive:
//!   while the view has not taken the last frame no new one is built, and
//!   frames are at most [`FRAME_INTERVAL`] apart. Records keep being
//!   applied in between, so the next frame shows all of them. The first
//!   frame after a pause, and the one after typed input, are built at once.
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

/// The shortest time between two frames while output streams: a 120 Hz
/// display's refresh.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(8_333);

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
    /// Search the mirror's history and screen. The owner only copies the
    /// text; the matching runs on a thread of its own.
    Search {
        query: crate::search::Query,
        ticket: u64,
        searches: crate::search::Searches,
        reply: tokio::sync::oneshot::Sender<SearchReply>,
    },
    /// Scroll so this row (from the top of history) is in view, near the
    /// middle.
    Reveal(u64),
    /// Scroll so this row (from the top of history) is the viewport's
    /// first, as far as history allows.
    Top(u64),
    /// The text of history and screen, one row per line (copy mode works
    /// on it).
    Text(tokio::sync::oneshot::Sender<Option<String>>),
}

/// A search's outcome: the matches, `None` when a newer search replaced
/// it, or why the pattern was refused.
pub type SearchReply = Result<Option<crate::search::Found>, String>;

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
    /// This view's searches; a new one stops the one before.
    searches: crate::search::Searches,
    target: Arc<Target>,
    /// The pane is the local daemon's (not a host's).
    local: bool,
    /// One signal means "a newer frame is waiting"; never more than one.
    pub wake: async_channel::Receiver<()>,
    /// Signalled when the view takes a frame: the owner may build the next.
    taken: Arc<tokio::sync::Notify>,
    /// Input was typed since the last frame: its echo is shown at once.
    typed: Arc<std::sync::atomic::AtomicBool>,
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
        Feed::start_via(socket, crate::Via::Local, pane, launch)
    }

    /// Like [`Feed::start`], following a pane `via` a host connected
    /// through the local daemon.
    pub fn start_via(
        socket: PathBuf,
        via: crate::Via,
        pane: Option<u32>,
        launch: Option<client::Launch>,
    ) -> std::io::Result<Feed> {
        let local = via == crate::Via::Local;
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
        let taken = Arc::new(tokio::sync::Notify::new());
        let typed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sender = InputSender {
            socket: socket.clone(),
            via: via.clone(),
            target: target.clone(),
            sent: inputs_sent.clone(),
            dropped: inputs_dropped.clone(),
        };
        let owner = Owner {
            input: input.clone(),
            via,
            target: target.clone(),
            meta: meta.clone(),
            bells: bells.clone(),
            clipboard: clipboard.clone(),
            latest: latest.clone(),
            status: status.clone(),
            stats: stats.clone(),
            wake: wake_tx,
            taken: taken.clone(),
            typed: typed.clone(),
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
            searches: crate::search::Searches::default(),
            target,
            local,
            wake,
            taken,
            typed,
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
            self.typed.store(true, Ordering::Release);
        }
        if let Event::Focus(f) = &event {
            self.target
                .focus
                .store(if f.gained { 1 } else { 2 }, Ordering::Release);
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

    /// Search the local mirror's history and screen, never the daemon's.
    /// A newer search, or [`Feed::cancel_search`], replaces this one.
    pub async fn search(&self, query: crate::search::Query) -> SearchReply {
        let ticket = self.searches.begin();
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.view(ViewCommand::Search {
            query,
            ticket,
            searches: self.searches.clone(),
            reply,
        });
        rx.await.unwrap_or(Ok(None))
    }

    /// The text of the local mirror's history and screen, one row per
    /// line, asked of the owner thread; never of the daemon.
    pub async fn text(&self) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.view(ViewCommand::Text(tx));
        rx.await.ok().flatten()
    }

    /// The local daemon's pane this feed follows, once known; `None` for
    /// a host's pane.
    pub fn pane(&self) -> Option<u32> {
        if !self.local {
            return None;
        }
        self.target
            .known
            .load(Ordering::Acquire)
            .then(|| self.target.pane.load(Ordering::Acquire) as u32)
    }

    pub fn cancel_search(&self) {
        self.searches.cancel();
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

    /// The newest unpainted frame, if any. Taking it lets the owner build
    /// the next one.
    pub fn take(&self) -> Option<Arc<FrameSnapshot>> {
        let frame = self.latest.lock().ok().and_then(|mut l| l.take());
        if frame.is_some() {
            self.taken.notify_one();
        }
        frame
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
    /// The view's focus as last reported: 0 never, 1 gained, 2 lost. Focus
    /// is state, not typing: the daemon must hear it once the pane is known
    /// and again after a reconnect, or clipboard writes go to no one.
    focus: std::sync::atomic::AtomicU8,
}

/// Sends input on its own connection, so input never waits for, or
/// interrupts, the follow loop.
struct InputSender {
    socket: PathBuf,
    via: crate::Via,
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
                    self.via.paste(c, pane, generation, &p.data).await
                }
                other => self
                    .via
                    .call(
                        c,
                        api::Request::TerminalInput(api::TerminalInput {
                            pane,
                            generation,
                            endpoint: None,
                            client: Some(self.target.client),
                            lease: None,
                            event: Some(other),
                        }),
                    )
                    .await
                    .map(drop),
            };
            match result {
                Ok(()) => {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    // Input is never replayed after an uncertain failure;
                    // the next input reconnects.
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
    via: crate::Via,
    /// The input sender's queue: to report focus again (see [`Target`]).
    input: tokio::sync::mpsc::UnboundedSender<api::terminal_input::Event>,
    target: Arc<Target>,
    meta: Arc<Mutex<crate::PaneMeta>>,
    bells: Arc<AtomicU64>,
    clipboard: Arc<Mutex<Option<Vec<u8>>>>,
    latest: Arc<Mutex<Option<Arc<FrameSnapshot>>>>,
    status: Arc<Mutex<FeedStatus>>,
    stats: Arc<FeedStats>,
    wake: async_channel::Sender<()>,
    taken: Arc<tokio::sync::Notify>,
    typed: Arc<std::sync::atomic::AtomicBool>,
}

/// When the owner builds its next frame (see the module notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Nothing new to show.
    Idle,
    Now,
    At(tokio::time::Instant),
    /// When the view takes the frame it has not taken yet.
    WhenTaken,
}

/// Paces frame building by presentation: the records applied since the
/// last frame are shown by one frame, built when the view can use it.
#[derive(Debug, Default)]
struct Pacer {
    /// Records were applied, or a checkpoint restored, since the last frame.
    dirty: bool,
    restored: bool,
    last: Option<tokio::time::Instant>,
}

impl Pacer {
    fn changed(&mut self, restored: bool) {
        self.dirty = true;
        self.restored |= restored;
    }

    /// A frame was built; returns whether it shows a restored checkpoint.
    fn built(&mut self, at: tokio::time::Instant) -> bool {
        self.dirty = false;
        self.last = Some(at);
        std::mem::take(&mut self.restored)
    }

    /// `unpresented`: the view has not taken the last published frame.
    /// `typed`: input was sent since the last frame.
    fn next(&self, now: tokio::time::Instant, unpresented: bool, typed: bool) -> Next {
        if !self.dirty {
            return Next::Idle;
        }
        if typed {
            return Next::Now;
        }
        if unpresented {
            return Next::WhenTaken;
        }
        match self.last.map(|t| t + FRAME_INTERVAL) {
            Some(at) if at > now => Next::At(at),
            _ => Next::Now,
        }
    }
}

impl Owner {
    /// Tell the daemon again that this view has the pane's focus, if it
    /// does: the report sent before the pane was known was dropped, and a
    /// daemon that restarted forgot it.
    fn report_focus(&self) {
        if self.target.focus.load(Ordering::Acquire) == 1 {
            let _ = self
                .input
                .send(api::terminal_input::Event::Focus(api::FocusInput {
                    gained: true,
                }));
        }
    }

    fn set(&self, s: FeedStatus) {
        if let Ok(mut st) = self.status.lock() {
            *st = s;
        }
    }

    /// The view has not taken the last published frame.
    fn unpresented(&self) -> bool {
        self.latest.lock().is_ok_and(|l| l.is_some())
    }

    /// Build a frame showing everything applied so far and publish it,
    /// unless a synchronized-output hold keeps it back.
    fn present(
        &self,
        f: &mut Follower,
        pacer: &mut Pacer,
        held_since: &mut Option<tokio::time::Instant>,
    ) {
        let now = tokio::time::Instant::now();
        let restored = pacer.built(now);
        self.typed.store(false, Ordering::Release);
        let Some(Ok(frame)) = f.frame() else { return };
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        if frame.render_hold && !restored {
            // Not shown yet; a later frame or the limit shows it.
            held_since.get_or_insert(now);
            return;
        }
        *held_since = None;
        self.publish(frame);
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
            match Follower::start_via(
                socket.to_path_buf(),
                self.via.clone(),
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
        let api::Response::TmuxTopology(t) = self
            .via
            .call(
                &c,
                api::Request::TmuxTopology(api::TopologyParams::default()),
            )
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
                ViewCommand::Copy(reply) | ViewCommand::Text(reply) => drop(reply.send(None)),
                ViewCommand::Link { reply, .. } => drop(reply.send(None)),
                ViewCommand::Search { reply, .. } => {
                    drop(reply.send(Err("the pane is not shown yet".into())));
                }
                _ => {}
            }
            return;
        };
        let Some(engine) = f.replica_mut().engine_mut() else {
            return;
        };
        let command_top = matches!(command, ViewCommand::Top(_));
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
            ViewCommand::Search {
                query,
                ticket,
                searches,
                reply,
            } => {
                match engine.screen_text() {
                    Err(e) => drop(reply.send(Err(e.to_string()))),
                    Ok(text) => {
                        // Matching never holds up input or frames.
                        std::thread::spawn(move || {
                            let found = crate::search::search(&text, &query, &searches, ticket);
                            let _ = reply.send(found);
                        });
                    }
                }
                false
            }
            ViewCommand::Text(reply) => {
                let _ = reply.send(engine.screen_text().ok());
                false
            }
            ViewCommand::Top(row) | ViewCommand::Reveal(row) => {
                let v = before.view;
                let top = if command_top {
                    row
                } else {
                    row.saturating_sub(v.len / 2)
                };
                let top = top.min(v.total_rows.saturating_sub(v.len));
                let delta = i64::try_from(top).unwrap_or(i64::MAX)
                    - i64::try_from(v.offset).unwrap_or(i64::MAX);
                delta != 0
                    && engine
                        .scroll_viewport(Scroll::Delta(delta as isize))
                        .is_ok()
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
        let mut pacer = Pacer::default();
        enum Wake {
            Pushed(Result<Option<client::Pushed>, FollowError>),
            View(ViewCommand),
            HoldExpired,
            Autoscroll(CellPos),
            Pace,
        }
        loop {
            // The view dropped its feed (a pane no longer shown): stop, and
            // with this thread its subscription and connections.
            if view_rx.is_closed() {
                return;
            }
            let next = pacer.next(
                tokio::time::Instant::now(),
                self.unpresented(),
                self.typed.load(Ordering::Acquire),
            );
            if next == Next::Now {
                self.present(&mut f, &mut pacer, &mut held_since);
                continue;
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
                let pace = async {
                    match next {
                        Next::At(t) => tokio::time::sleep_until(t).await,
                        Next::WhenTaken => self.taken.notified().await,
                        Next::Idle | Next::Now => std::future::pending().await,
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
                    () = pace => Wake::Pace,
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
                        pacer.built(tokio::time::Instant::now());
                        if let Some(Ok(frame)) = f.frame() {
                            self.publish(frame);
                        }
                        continue;
                    }
                    // Time for the next frame, or the view took the last.
                    Wake::Pace => continue,
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
            // A pending frame wakes the view anyway.
            if wake_view
                && !pacer.dirty
                && let Ok(s) = &step
                && s.records == 0
                && !s.restored
                && let Some(Ok(frame)) = f.frame()
            {
                self.publish(frame);
            }
            if let Ok(s) = &step
                && (s.records > 0 || s.restored)
            {
                if let Some((_, generation, _)) = f.replica().position() {
                    self.target.generation.store(generation, Ordering::Release);
                    if !self.target.known.swap(true, Ordering::AcqRel) {
                        self.report_focus();
                    }
                }
                // Built when the view can show it (top of the loop).
                pacer.changed(s.restored);
            }
            let following = matches!(&f.state, FollowState::Following);
            if !following || step.is_err() {
                // Whatever was applied is shown before the feed stops or
                // waits to reconnect.
                if pacer.dirty {
                    self.present(&mut f, &mut pacer, &mut held_since);
                }
            }
            match step {
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
                self.report_focus();
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant;

    /// Records arriving faster than frames can be shown: at most one frame
    /// per interval, and none while the view has not taken the last.
    #[test]
    fn frames_follow_presentation_not_the_records_that_arrive() {
        let start = Instant::now();
        let mut p = Pacer::default();
        assert_eq!(p.next(start, false, false), Next::Idle);

        // The first change after a pause is shown at once.
        p.changed(false);
        assert_eq!(p.next(start, false, false), Next::Now);
        p.built(start);

        // A thousand batches within one interval: no frame until it ends.
        let mut built = 0;
        for i in 0..1000u32 {
            let now = start + FRAME_INTERVAL * i / 1000;
            p.changed(false);
            if p.next(now, false, false) == Next::Now {
                p.built(now);
                built += 1;
            }
        }
        assert_eq!(built, 0);
        assert_eq!(
            p.next(start, false, false),
            Next::At(start + FRAME_INTERVAL)
        );

        // An untaken frame holds the next back, however long it waits.
        let later = start + FRAME_INTERVAL * 100;
        assert_eq!(p.next(later, true, false), Next::WhenTaken);
        assert_eq!(p.next(later, false, false), Next::Now);
        p.built(later);
        assert_eq!(p.next(later, false, false), Next::Idle);
    }

    #[test]
    fn typed_input_and_restores_are_not_held_back() {
        let start = Instant::now();
        let mut p = Pacer::default();
        p.changed(false);
        p.built(start);
        // The echo of a keystroke is shown at once, even mid-interval and
        // with a frame still untaken.
        p.changed(false);
        assert_eq!(p.next(start, true, true), Next::Now);
        // A restore is remembered until the frame that shows it.
        p.changed(true);
        p.changed(false);
        assert!(p.built(start));
        assert!(!p.built(start));
    }
}
