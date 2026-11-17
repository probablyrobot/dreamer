//! tmux's paste buffers for a desktop: a thread follows
//! `clipboard.tmux_buffer` events and reads each new buffer's text for the
//! window to put on the system clipboard, and takes the window's own copies
//! to tmux as its newest buffer. The event never carries the text; it is
//! read here, on request, over the desktop's own connection.
//!
//! Whether either direction is used is the window's decision
//! (`clipboard.tmux` in the settings); this only carries the text.
//!
//! The same connection reads tmux's key options (the prefix and the copy
//! mode keys) and types keys after the prefix for the window's key layer,
//! so none of that waits on, or blocks, the window.

use std::path::PathBuf;
use std::time::Duration;

use api::{Request, Response};
use client::Pushed;

/// What the window asks of the thread.
#[derive(Debug)]
enum Ask {
    /// Make this text tmux's newest buffer.
    Write(String),
    /// Read a buffer (the newest when unnamed) and answer.
    Read(Option<String>, tokio::sync::oneshot::Sender<Option<String>>),
    /// List the buffers, newest first.
    List(tokio::sync::oneshot::Sender<Vec<api::TmuxBufferInfo>>),
    /// Read tmux's key options.
    Keys(tokio::sync::oneshot::Sender<Option<api::KeyOptions>>),
    /// Type the prefix and a key on a pane.
    PrefixKey {
        pane: u32,
        key: String,
        reply: tokio::sync::oneshot::Sender<Result<api::PrefixKeyResult, String>>,
    },
}

#[derive(Debug)]
pub struct TmuxBuffers {
    asks: tokio::sync::mpsc::UnboundedSender<Ask>,
    /// The text of each buffer tmux reports as changed (not Dreamer's own).
    pub copied: async_channel::Receiver<String>,
}

impl TmuxBuffers {
    pub fn start(socket: PathBuf) -> std::io::Result<TmuxBuffers> {
        let (copied_tx, copied) = async_channel::bounded(8);
        let (asks, asks_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("dreamer-tmux-buffers".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(run(socket, copied_tx, asks_rx));
            })?;
        Ok(TmuxBuffers { asks, copied })
    }

    /// Make `text` tmux's newest paste buffer (empty text is ignored).
    pub fn write(&self, text: String) {
        if !text.is_empty() {
            let _ = self.asks.send(Ask::Write(text));
        }
    }

    /// tmux's newest paste buffer, if there is one.
    pub async fn newest(&self) -> Option<String> {
        self.named(None).await
    }

    /// A paste buffer's text: `name`, or the newest.
    pub async fn named(&self, name: Option<String>) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.asks.send(Ask::Read(name, tx)).ok()?;
        rx.await.ok().flatten()
    }

    /// tmux's paste buffers, newest first, without their text.
    pub async fn list(&self) -> Vec<api::TmuxBufferInfo> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.asks.send(Ask::List(tx)).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// tmux's prefix, prefix2, mode-keys, and mode-style.
    pub async fn key_options(&self) -> Option<api::KeyOptions> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.asks.send(Ask::Keys(tx)).ok()?;
        rx.await.ok().flatten()
    }

    /// Type tmux's prefix and then `key` (a tmux key name) on `pane`, so
    /// tmux's own binding for it runs.
    pub async fn prefix_key(&self, pane: u32, key: String) -> Result<api::PrefixKeyResult, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.asks
            .send(Ask::PrefixKey { pane, key, reply })
            .map_err(|_| "the daemon connection is gone".to_owned())?;
        rx.await
            .unwrap_or_else(|_| Err("the daemon connection is gone".into()))
    }
}

async fn run(
    socket: PathBuf,
    copied: async_channel::Sender<String>,
    mut asks: tokio::sync::mpsc::UnboundedReceiver<Ask>,
) {
    let mut backoff = Duration::from_millis(200);
    loop {
        match follow(&socket, &copied, &mut asks).await {
            Ok(()) => return,
            Err(()) => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Follow until the connection fails (`Err`) or the window goes (`Ok`).
async fn follow(
    socket: &std::path::Path,
    copied: &async_channel::Sender<String>,
    asks: &mut tokio::sync::mpsc::UnboundedReceiver<Ask>,
) -> Result<(), ()> {
    let mut c = client::Client::connect(socket, "gui-clipboard")
        .await
        .map_err(|_| ())?;
    let mut pushed = c.pushed().ok_or(())?;
    c.call(Request::EventsSubscribe(api::EventSubscribe {
        kinds: vec!["clipboard.tmux_buffer".into()],
        ..api::EventSubscribe::default()
    }))
    .await
    .map_err(|_| ())?;
    loop {
        tokio::select! {
            p = pushed.recv() => match p {
                Some(Pushed::Event { topic, payload, .. }) => {
                    let Ok(api::Notification::EventsBatch(b)) =
                        api::Notification::decode(&topic, &payload)
                    else {
                        continue;
                    };
                    // Only the newest change matters to a clipboard.
                    if let Some(name) = b.events.iter().rev().find_map(|e| e.subject.clone())
                        && let Some(text) = read(&c, Some(name)).await
                    {
                        let _ = copied.try_send(text);
                    }
                }
                Some(_) => {}
                None => return Err(()),
            },
            a = asks.recv() => match a {
                Some(Ask::Write(text)) => {
                    let _ = c
                        .call(Request::ClipboardWriteTmuxBuffer(api::TmuxBufferWrite {
                            endpoint: None,
                            text,
                        }))
                        .await;
                }
                Some(Ask::Read(name, reply)) => {
                    let _ = reply.send(read(&c, name).await);
                }
                Some(Ask::List(reply)) => {
                    let list = match c
                        .call(Request::ClipboardListTmuxBuffers(
                            api::TmuxBufferListParams::default(),
                        ))
                        .await
                    {
                        Ok(Response::ClipboardListTmuxBuffers(l)) => l.buffers,
                        _ => Vec::new(),
                    };
                    let _ = reply.send(list);
                }
                Some(Ask::Keys(reply)) => {
                    let keys = match c
                        .call(Request::TmuxKeyOptions(api::KeyOptionsParams::default()))
                        .await
                    {
                        Ok(Response::TmuxKeyOptions(o)) => Some(o),
                        _ => None,
                    };
                    let _ = reply.send(keys);
                }
                Some(Ask::PrefixKey { pane, key, reply }) => {
                    let r = match c
                        .call(Request::TmuxPrefixKey(api::PrefixKey {
                            endpoint: None,
                            pane,
                            key,
                        }))
                        .await
                    {
                        Ok(Response::TmuxPrefixKey(r)) => Ok(r),
                        Ok(_) => Err("unexpected answer".into()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply.send(r);
                }
                None => return Ok(()),
            },
        }
    }
}

async fn read(c: &client::Client, name: Option<String>) -> Option<String> {
    match c
        .call(Request::ClipboardReadTmuxBuffer(api::TmuxBufferRead {
            endpoint: None,
            name,
        }))
        .await
    {
        Ok(Response::ClipboardReadTmuxBuffer(b)) if b.found => Some(b.text),
        _ => None,
    }
}
