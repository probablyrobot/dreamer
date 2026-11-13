//! The local daemon's remote hosts for a view: their latest list, and
//! connect, reconnect, and disconnect. Calls run on this feed's own thread,
//! never on the UI thread.
//!
//! The list is read once a second while the feed lives; a view keeps one
//! only while it shows hosts (the sidebar), so an idle window costs
//! nothing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{Request, Response};

#[derive(Debug)]
enum Command {
    /// Reconnect unless live: works for profiles and for hosts the daemon
    /// already knows.
    Connect(String),
    Reconnect(String),
    Disconnect(String),
}

#[derive(Debug)]
pub struct HostsFeed {
    latest: Arc<Mutex<Option<api::HostList>>>,
    error: Arc<Mutex<Option<String>>>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    /// One signal means "a newer list is waiting".
    pub wake: async_channel::Receiver<()>,
}

impl HostsFeed {
    pub fn start(socket: PathBuf) -> std::io::Result<HostsFeed> {
        let latest = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let (wake_tx, wake) = async_channel::bounded(1);
        let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
        let (l, e) = (latest.clone(), error.clone());
        std::thread::Builder::new()
            .name("dreamer-hosts".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(run(socket, l, e, wake_tx, rx));
            })?;
        Ok(HostsFeed {
            latest,
            error,
            commands,
            wake,
        })
    }

    /// The latest list, if any arrived.
    pub fn list(&self) -> Option<api::HostList> {
        self.latest.lock().ok().and_then(|l| l.clone())
    }

    /// Why the list could not be read (no daemon, ...), if it could not.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// Connect `name` unless it is live already.
    pub fn connect(&self, name: &str) {
        let _ = self.commands.send(Command::Connect(name.to_owned()));
    }

    pub fn reconnect(&self, name: &str) {
        let _ = self.commands.send(Command::Reconnect(name.to_owned()));
    }

    pub fn disconnect(&self, name: &str) {
        let _ = self.commands.send(Command::Disconnect(name.to_owned()));
    }
}

/// Connect `name` unless it is live, once, on a short-lived thread,
/// starting the daemon with `launch` if it is not running.
pub fn connect_once(launch: client::Launch, name: String) {
    let _ = std::thread::Builder::new()
        .name("dreamer-host-connect".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let Ok((c, _)) = client::ensure_running(&launch, "gui-hosts").await else {
                    return;
                };
                let live = match c.call(Request::HostsList(api::Empty {})).await {
                    Ok(Response::HostsList(l)) => l.hosts.iter().any(|h| {
                        h.name == name
                            && (h.state == api::LinkState::Live as i32
                                || h.state == api::LinkState::Connecting as i32)
                    }),
                    _ => false,
                };
                if !live {
                    let _ = c.call(Request::HostsReconnect(api::HostRef { name })).await;
                }
            });
        });
}

async fn run(
    socket: PathBuf,
    latest: Arc<Mutex<Option<api::HostList>>>,
    error: Arc<Mutex<Option<String>>>,
    wake: async_channel::Sender<()>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let mut client: Option<client::Client> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let command = tokio::select! {
            _ = tick.tick() => None,
            c = commands.recv() => match c {
                Some(c) => Some(c),
                // The view dropped the feed.
                None => return,
            },
        };
        if client.is_none() {
            client = client::Client::connect(&socket, "gui-hosts").await.ok();
        }
        let Some(c) = client.as_ref() else {
            set(&error, Some("the daemon is not running".into()));
            let _ = wake.try_send(());
            continue;
        };
        let acted = command.is_some();
        if let Some(command) = command {
            let request = match &command {
                Command::Connect(name) => {
                    let live = latest.lock().ok().and_then(|l| {
                        l.as_ref()?.hosts.iter().find(|h| &h.name == name).map(|h| {
                            h.state == api::LinkState::Live as i32
                                || h.state == api::LinkState::Connecting as i32
                        })
                    });
                    if live == Some(true) {
                        continue;
                    }
                    Request::HostsReconnect(api::HostRef { name: name.clone() })
                }
                Command::Reconnect(name) => {
                    Request::HostsReconnect(api::HostRef { name: name.clone() })
                }
                Command::Disconnect(name) => {
                    Request::HostsDisconnect(api::HostRef { name: name.clone() })
                }
            };
            // The outcome shows in the next list; a failure is in its detail.
            let _ = c.call(request).await;
        }
        match c.call(Request::HostsList(api::Empty {})).await {
            Ok(Response::HostsList(l)) => {
                set(&error, l.profiles_error.clone());
                let changed = latest
                    .lock()
                    .map(|mut cur| {
                        let changed = cur.as_ref() != Some(&l);
                        *cur = Some(l);
                        changed
                    })
                    .unwrap_or(false);
                if changed || acted {
                    let _ = wake.try_send(());
                }
            }
            Ok(_) => {}
            Err(e) => {
                client = None;
                set(&error, Some(e.to_string()));
                let _ = wake.try_send(());
            }
        }
    }
}

fn set(slot: &Mutex<Option<String>>, v: Option<String>) {
    if let Ok(mut s) = slot.lock() {
        *s = v;
    }
}
