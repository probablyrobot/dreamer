//! Decorations for a view (spec 12.8): the marks filters put on lines of
//! the panes the view shows. A thread follows `decorations.changed` events
//! and lists a pane again when its marks change, keeping the newest list
//! per pane; the view is woken once per change.
//!
//! Marks are data to draw over the grid. Nothing here touches the
//! terminal stream or the mirror. Local panes only: a host's events are
//! not relayed through the coordinator.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{Request, Response};
use client::Pushed;

#[derive(Debug)]
pub struct DecorationsFeed {
    latest: Arc<Mutex<HashMap<u32, Vec<api::Decoration>>>>,
    panes: tokio::sync::watch::Sender<BTreeSet<u32>>,
    /// One signal means "some pane's marks changed".
    pub wake: async_channel::Receiver<()>,
}

impl DecorationsFeed {
    pub fn start(socket: PathBuf) -> std::io::Result<DecorationsFeed> {
        let latest = Arc::new(Mutex::new(HashMap::new()));
        let (wake_tx, wake) = async_channel::bounded(1);
        let (panes, panes_rx) = tokio::sync::watch::channel(BTreeSet::new());
        let l = latest.clone();
        std::thread::Builder::new()
            .name("dreamer-decorations".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(run(socket, l, wake_tx, panes_rx));
            })?;
        Ok(DecorationsFeed {
            latest,
            panes,
            wake,
        })
    }

    /// The panes the view shows; marks are kept for these only.
    pub fn watch(&self, panes: impl IntoIterator<Item = u32>) {
        let panes: BTreeSet<u32> = panes.into_iter().collect();
        self.panes.send_if_modified(|p| {
            let changed = *p != panes;
            *p = panes;
            changed
        });
    }

    /// A pane's current marks (empty when it has none, or is not watched).
    pub fn marks(&self, pane: u32) -> Vec<api::Decoration> {
        self.latest
            .lock()
            .ok()
            .and_then(|l| l.get(&pane).cloned())
            .unwrap_or_default()
    }
}

async fn run(
    socket: PathBuf,
    latest: Arc<Mutex<HashMap<u32, Vec<api::Decoration>>>>,
    wake: async_channel::Sender<()>,
    mut panes: tokio::sync::watch::Receiver<BTreeSet<u32>>,
) {
    let mut backoff = Duration::from_millis(200);
    loop {
        match follow(&socket, &latest, &wake, &mut panes).await {
            // The view dropped the feed.
            Ok(()) => return,
            Err(()) => {
                // Marks from a daemon we lost may no longer hold.
                if let Ok(mut l) = latest.lock()
                    && !l.is_empty()
                {
                    l.clear();
                    let _ = wake.try_send(());
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            r = panes.changed() => if r.is_err() { return },
        }
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// Follow until the connection fails (`Err`) or the view goes (`Ok`).
async fn follow(
    socket: &std::path::Path,
    latest: &Mutex<HashMap<u32, Vec<api::Decoration>>>,
    wake: &async_channel::Sender<()>,
    panes: &mut tokio::sync::watch::Receiver<BTreeSet<u32>>,
) -> Result<(), ()> {
    let mut c = client::Client::connect(socket, "gui-decorations")
        .await
        .map_err(|_| ())?;
    let mut pushed = c.pushed().ok_or(())?;
    c.call(Request::EventsSubscribe(api::EventSubscribe {
        kinds: vec!["decorations.changed".into()],
        ..api::EventSubscribe::default()
    }))
    .await
    .map_err(|_| ())?;
    // Subscribed first, then listed: a change in between is listed again.
    let mut watched = panes.borrow_and_update().clone();
    for &p in &watched {
        list(&c, p, latest, wake).await?;
    }
    loop {
        tokio::select! {
            p = pushed.recv() => match p {
                Some(Pushed::Event { topic, payload, .. }) => {
                    let Ok(api::Notification::EventsBatch(b)) =
                        api::Notification::decode(&topic, &payload)
                    else {
                        continue;
                    };
                    let again: BTreeSet<u32> = if b.gap.is_some() {
                        // Missed changes: list everything again.
                        watched.clone()
                    } else {
                        b.events
                            .iter()
                            .filter_map(|e| e.pane)
                            .filter(|p| watched.contains(p))
                            .collect()
                    };
                    for p in again {
                        list(&c, p, latest, wake).await?;
                    }
                }
                Some(Pushed::Bulk { .. }) => {}
                Some(Pushed::Closed { .. }) | None => return Err(()),
            },
            r = panes.changed() => {
                if r.is_err() {
                    return Ok(());
                }
                let now = panes.borrow_and_update().clone();
                if let Ok(mut l) = latest.lock() {
                    l.retain(|p, _| now.contains(p));
                }
                for &p in now.difference(&watched) {
                    list(&c, p, latest, wake).await?;
                }
                watched = now;
            }
        }
    }
}

/// List `pane`'s marks and publish them if they changed. A pane without a
/// live mirror (closed, or being rebuilt) has none for now.
async fn list(
    c: &client::Client,
    pane: u32,
    latest: &Mutex<HashMap<u32, Vec<api::Decoration>>>,
    wake: &async_channel::Sender<()>,
) -> Result<(), ()> {
    let marks = match c
        .call(Request::DecorationsList(api::DecorationQuery { pane }))
        .await
    {
        Ok(Response::DecorationsList(l)) => l.decorations,
        Ok(_) | Err(client::ClientError::Rpc(_)) => Vec::new(),
        Err(_) => return Err(()),
    };
    let changed = latest.lock().is_ok_and(|mut l| {
        let old = l.get(&pane);
        if old.map_or(marks.is_empty(), |o| *o == marks) {
            false
        } else {
            l.insert(pane, marks);
            true
        }
    });
    if changed {
        let _ = wake.try_send(());
    }
    Ok(())
}
