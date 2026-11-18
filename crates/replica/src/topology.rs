//! The tmux topology for a view (spec 15.1): a thread follows `tmux.watch`
//! and keeps the newest topology in a latest-value slot, waking the view
//! once per newer revision. Actions the view asks for (select a window or
//! pane) go to the daemon from the same thread; their effect is seen only
//! when the resulting topology arrives, never assumed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{Request, Response};
use client::Pushed;
use prost::Message;

/// Where the topology feed stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyStatus {
    Connecting,
    Following,
    /// Lost the daemon or tmux; retrying.
    Reconnecting(String),
    /// The daemon could not start, and why (its own reason, or the spawn
    /// error). Starting is tried again every few seconds.
    CannotStart(String),
}

/// How often a daemon that could not start is started again.
const RESTART_EVERY: Duration = Duration::from_secs(5);

/// What the view asks the daemon for.
#[derive(Debug)]
enum Command {
    /// An action, and where its settled record goes (if anywhere).
    Act(
        api::action::Kind,
        Option<async_channel::Sender<Option<api::ActionRecord>>>,
    ),
    Size {
        window: u32,
        cols: u32,
        rows: u32,
    },
    Drawer(api::DrawerRequest),
    WebOpen(api::WebOpen),
    WebState(api::WebState),
}

#[derive(Debug)]
pub struct TopologyFeed {
    latest: Arc<Mutex<Option<api::TopologyUpdate>>>,
    status: Arc<Mutex<TopologyStatus>>,
    actions: tokio::sync::mpsc::UnboundedSender<Command>,
    /// This view's client id, for window sizing ownership.
    pub client: u64,
    /// One signal means "a newer topology is waiting".
    pub wake: async_channel::Receiver<()>,
}

impl TopologyFeed {
    /// Follow the topology of the daemon at `socket` on a new thread, first
    /// starting the daemon with `launch` if it is not running.
    pub fn start(socket: PathBuf, launch: Option<client::Launch>) -> std::io::Result<TopologyFeed> {
        TopologyFeed::start_via(socket, crate::Via::Local, launch)
    }

    /// Like [`TopologyFeed::start`], following a host connected through
    /// the local daemon. The status also says when that host is not live
    /// (its last topology stays shown, marked stale).
    pub fn start_via(
        socket: PathBuf,
        via: crate::Via,
        launch: Option<client::Launch>,
    ) -> std::io::Result<TopologyFeed> {
        let latest = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(TopologyStatus::Connecting));
        let (wake_tx, wake) = async_channel::bounded(1);
        let (actions, actions_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            (u64::from(std::process::id()) << 32) ^ nanos ^ 0x5157
        };
        let owner = Owner {
            via,
            latest: latest.clone(),
            status: status.clone(),
            wake: wake_tx,
            client,
        };
        std::thread::Builder::new()
            .name("dreamer-topology".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    owner.set(TopologyStatus::Reconnecting("no runtime".into()));
                    return;
                };
                rt.block_on(owner.run(socket, launch, actions_rx));
            })?;
        Ok(TopologyFeed {
            latest,
            status,
            actions,
            client,
            wake,
        })
    }

    /// The newest topology not yet taken, if any.
    pub fn take(&self) -> Option<api::TopologyUpdate> {
        self.latest.lock().ok().and_then(|mut l| l.take())
    }

    pub fn status(&self) -> TopologyStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(TopologyStatus::Connecting)
    }

    /// Ask the daemon to run an action (for example select a window). Its
    /// result shows up as a newer topology.
    pub fn act(&self, action: api::action::Kind) {
        let _ = self.actions.send(Command::Act(action, None));
    }

    /// Like [`TopologyFeed::act`], also handing back the settled record
    /// (`None` when the daemon could not be asked), for example to learn
    /// the first pane of a new session.
    pub fn act_then(
        &self,
        action: api::action::Kind,
    ) -> async_channel::Receiver<Option<api::ActionRecord>> {
        let (tx, rx) = async_channel::bounded(1);
        let _ = self.actions.send(Command::Act(action, Some(tx)));
        rx
    }

    /// Show, focus, hide, or restart a session's harness drawer
    /// (`drawer.toggle`). Its result shows up as a newer topology.
    pub fn drawer(&self, request: api::DrawerRequest) {
        let _ = self.actions.send(Command::Drawer(request));
    }

    /// Open a web page in a new placeholder pane (`web.open`). The pane
    /// shows up as a newer topology.
    pub fn web_open(&self, request: api::WebOpen) {
        let _ = self.actions.send(Command::WebOpen(request));
    }

    /// Keep a web pane's page title and scroll position in its placeholder
    /// (`web.state`).
    pub fn web_state(&self, request: api::WebState) {
        let _ = self.actions.send(Command::WebState(request));
    }

    /// Ask for a tmux window's size in cells. Applied only while this view
    /// owns the window's size; call it when the view's own size changes,
    /// never in reaction to a topology change.
    pub fn size_window(&self, window: u32, cols: u32, rows: u32) {
        let _ = self.actions.send(Command::Size { window, cols, rows });
    }
}

struct Owner {
    via: crate::Via,
    latest: Arc<Mutex<Option<api::TopologyUpdate>>>,
    status: Arc<Mutex<TopologyStatus>>,
    wake: async_channel::Sender<()>,
    client: u64,
}

impl Owner {
    fn set(&self, s: TopologyStatus) {
        let changed = self.status.lock().map(|mut st| {
            let changed = *st != s;
            *st = s;
            changed
        });
        // The view shows the status; let it know.
        if changed.unwrap_or(false) {
            let _ = self.wake.try_send(());
        }
    }

    /// Mark the view stale while its host is not live, and current again
    /// once it is.
    async fn check_host(&self, c: &client::Client) {
        let Some(name) = self.via.host() else { return };
        let Ok(Response::HostsList(l)) = c.call(Request::HostsList(api::Empty {})).await else {
            return;
        };
        let Some(h) = l.hosts.iter().find(|h| h.name == name) else {
            return;
        };
        if h.state == api::LinkState::Live as i32 {
            self.set(TopologyStatus::Following);
        } else {
            let why = h.detail.clone().unwrap_or_else(|| "not connected".into());
            self.set(TopologyStatus::Reconnecting(format!("host {name}: {why}")));
        }
    }

    /// Publish `u` if it is newer than anything published before.
    fn publish(&self, u: api::TopologyUpdate, seen: &mut u64) {
        if u.revision <= *seen {
            return;
        }
        *seen = u.revision;
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(u);
        }
        let _ = self.wake.try_send(());
    }

    async fn run(
        self,
        socket: PathBuf,
        launch: Option<client::Launch>,
        mut actions: tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        // Why the daemon could not start, and when that was last tried.
        let mut cannot_start: Option<(String, tokio::time::Instant)> = None;
        let start = |l: &client::Launch| {
            let l = l.clone();
            async move {
                match client::ensure_running(&l, "gui").await {
                    Err(e @ client::ClientError::Spawn(_)) => {
                        Some((e.to_string(), tokio::time::Instant::now()))
                    }
                    _ => None,
                }
            }
        };
        if let Some(l) = &launch {
            cannot_start = start(l).await;
        }
        let mut seen = 0u64;
        let mut backoff = Duration::from_millis(100);
        loop {
            if actions.is_closed() {
                return;
            }
            if let (Some(l), Some((_, at))) = (&launch, &cannot_start)
                && at.elapsed() >= RESTART_EVERY
            {
                cannot_start = start(l).await;
            }
            let why = match self.follow(&socket, &mut actions, &mut seen).await {
                Ok(()) => return,
                Err(e) => e,
            };
            match &cannot_start {
                Some((reason, _)) => self.set(TopologyStatus::CannotStart(reason.clone())),
                None => self.set(TopologyStatus::Reconnecting(why)),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }

    /// Follow until the connection fails (`Err`) or the view goes (`Ok`).
    async fn follow(
        &self,
        socket: &std::path::Path,
        actions: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
        seen: &mut u64,
    ) -> Result<(), String> {
        let mut c = client::Client::connect(socket, "gui")
            .await
            .map_err(|e| e.to_string())?;
        let mut pushed = c.pushed().ok_or("no event stream")?;
        let first = self.via.watch(&c).await.map_err(|e| e.to_string())?;
        self.set(TopologyStatus::Following);
        // A host's link state, checked once a second: its topology stays
        // shown while it reconnects, marked stale.
        let mut host_check = tokio::time::interval(Duration::from_secs(1));
        // Revisions start over when the daemon restarts; take its first.
        *seen = 0;
        self.publish(first, seen);
        loop {
            tokio::select! {
                _ = host_check.tick(), if self.via.host().is_some() => {
                    self.check_host(&c).await;
                }
                p = pushed.recv() => match p {
                    Some(Pushed::Event { topic, payload, .. }) if topic == "tmux.topology" => {
                        if let Ok(u) = api::TopologyUpdate::decode(payload.as_slice()) {
                            self.publish(u, seen);
                        }
                    }
                    Some(Pushed::Closed { .. }) | None => {
                        return Err("the daemon connection ended".into());
                    }
                    Some(_) => {}
                },
                a = actions.recv() => match a {
                    Some(Command::Act(kind, reply)) => {
                        let submit = api::SubmitAction {
                            action: Some(api::Action { kind: Some(kind) }),
                            ..api::SubmitAction::default()
                        };
                        // The outcome arrives as a topology change.
                        let r = self.via.call(&c, Request::ActionsSubmit(submit)).await;
                        if let Some(reply) = reply {
                            let record = match r {
                                Ok(Response::ActionsSubmit(rec)) => Some(rec),
                                _ => None,
                            };
                            let _ = reply.try_send(record);
                        }
                    }
                    Some(Command::Size { window, cols, rows }) => {
                        let size = Request::TmuxSizeWindow(api::SizeWindow {
                            window,
                            cols,
                            rows,
                            client: self.client,
                            ..api::SizeWindow::default()
                        });
                        let _ = self.via.call(&c, size).await;
                    }
                    Some(Command::Drawer(request)) => {
                        // The outcome arrives as a topology change.
                        let _ = self.via.call(&c, Request::DrawerToggle(request)).await;
                    }
                    Some(Command::WebOpen(request)) => {
                        if let Err(e) = self.via.call(&c, Request::WebOpen(request)).await {
                            eprintln!("dreamer: the web page did not open: {e}");
                        }
                    }
                    Some(Command::WebState(request)) => {
                        if let Err(e) = self.via.call(&c, Request::WebState(request)).await {
                            eprintln!("dreamer: the web pane's state was not kept: {e}");
                        }
                    }
                    None => return Ok(()),
                },
            }
        }
    }
}
