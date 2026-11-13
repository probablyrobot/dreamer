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
}

/// What the view asks the daemon for.
#[derive(Debug)]
enum Command {
    Act(api::action::Kind),
    Size { window: u32, cols: u32, rows: u32 },
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
        let _ = self.actions.send(Command::Act(action));
    }

    /// Ask for a tmux window's size in cells. Applied only while this view
    /// owns the window's size; call it when the view's own size changes,
    /// never in reaction to a topology change.
    pub fn size_window(&self, window: u32, cols: u32, rows: u32) {
        let _ = self.actions.send(Command::Size { window, cols, rows });
    }
}

struct Owner {
    latest: Arc<Mutex<Option<api::TopologyUpdate>>>,
    status: Arc<Mutex<TopologyStatus>>,
    wake: async_channel::Sender<()>,
    client: u64,
}

impl Owner {
    fn set(&self, s: TopologyStatus) {
        if let Ok(mut st) = self.status.lock() {
            *st = s;
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
        if let Some(l) = &launch
            && let Err(e) = client::ensure_running(l, "gui").await
        {
            self.set(TopologyStatus::Reconnecting(e.to_string()));
        }
        let mut seen = 0u64;
        let mut backoff = Duration::from_millis(100);
        loop {
            if actions.is_closed() {
                return;
            }
            let why = match self.follow(&socket, &mut actions, &mut seen).await {
                Ok(()) => return,
                Err(e) => e,
            };
            self.set(TopologyStatus::Reconnecting(why));
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
        let Response::TmuxWatch(first) = c
            .call(Request::TmuxWatch(api::TopologyParams::default()))
            .await
            .map_err(|e| e.to_string())?
        else {
            return Err("unexpected reply to tmux.watch".into());
        };
        self.set(TopologyStatus::Following);
        // Revisions start over when the daemon restarts; take its first.
        *seen = 0;
        self.publish(first, seen);
        loop {
            tokio::select! {
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
                    Some(Command::Act(kind)) => {
                        let submit = api::SubmitAction {
                            action: Some(api::Action { kind: Some(kind) }),
                            ..api::SubmitAction::default()
                        };
                        // The outcome arrives as a topology change.
                        let _ = c.call(Request::ActionsSubmit(submit)).await;
                    }
                    Some(Command::Size { window, cols, rows }) => {
                        let _ = c
                            .call(Request::TmuxSizeWindow(api::SizeWindow {
                                window,
                                cols,
                                rows,
                                client: self.client,
                                ..api::SizeWindow::default()
                            }))
                            .await;
                    }
                    None => return Ok(()),
                },
            }
        }
    }
}
