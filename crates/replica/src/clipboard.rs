//! tmux's paste buffers for a desktop: a thread follows
//! `clipboard.tmux_buffer` events and reads each new buffer's text for the
//! window to put on the system clipboard, and takes the window's own copies
//! to tmux as its newest buffer. The event never carries the text; it is
//! read here, on request, over the desktop's own connection.
//!
//! Whether either direction is used is the window's decision
//! (`clipboard.tmux` in the settings); this only carries the text.

use std::path::PathBuf;
use std::time::Duration;

use api::{Request, Response};
use client::Pushed;

/// What the window asks of the thread.
#[derive(Debug)]
enum Ask {
    /// Make this text tmux's newest buffer.
    Write(String),
    /// Read the newest buffer and answer.
    Newest(tokio::sync::oneshot::Sender<Option<String>>),
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.asks.send(Ask::Newest(tx)).ok()?;
        rx.await.ok().flatten()
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
                Some(Ask::Newest(reply)) => {
                    let _ = reply.send(read(&c, None).await);
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
