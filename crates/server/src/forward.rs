//! Forwards (`web.forward`): a port on a remote host's own loopback,
//! reached from a port on this machine's loopback through the SSH
//! connection the coordinator already holds. No new SSH connection, and
//! nothing listens on any network: this file is the only product code
//! that touches TCP, and only on the loopback (`cargo xtask policy`
//! checks both).
//!
//! - **The coordinator** ([`Forwards`]): `hosts.forward` binds an
//!   ephemeral port on 127.0.0.1. Each connection accepted there becomes a
//!   stream on the host's link (`forward.open`). Holds are counted per host
//!   port: the listener, and every connection through it, closes with the
//!   last hold, and a client connection's holds go when it closes.
//! - **The host** ([`Streams`]): `forward.open` connects to its own
//!   loopback port for the coordinator's connection; its streams close
//!   with that connection.
//!
//! Flow control. Data from the host's port is read only within the credit
//! the coordinator gives (`forward.ack`) as it passes data on, at most
//! [`WINDOW`] bytes per stream, and travels as bulk records on a lane the
//! connection's writer takes only when nothing else waits, so terminal
//! records and replies never queue behind it. Data to the host's port
//! goes one `forward.write` at a time per stream, answered once queued
//! (at most [`QUEUED`] chunks per stream).

use std::collections::HashMap;
use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use api::{RpcError, code};
use client::Client;
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

/// Bytes of a stream's incoming data that may be on the way at once.
pub const WINDOW: u32 = 128 << 10;
/// The most one read or one `forward.write` carries.
pub const CHUNK: usize = 16 << 10;
/// Chunks queued per stream for the host's port.
pub const QUEUED: usize = 8;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- the host's side --------------------------------------------------

/// Stream ids, unique within this daemon.
static STREAM_IDS: AtomicU64 = AtomicU64::new(0);

struct Stream {
    credit: Arc<Semaphore>,
    to_port: mpsc::Sender<(Vec<u8>, bool)>,
    tasks: [AbortHandle; 2],
}

/// One connection's streams to this host's loopback ports.
pub struct Streams {
    /// The connection's lowest lane: encoded `Bulk` records.
    lane: mpsc::Sender<Vec<u8>>,
    open: Mutex<HashMap<u64, Stream>>,
}

fn gone(stream: u64) -> RpcError {
    RpcError::new(code::NOT_FOUND, format!("no forward stream {stream}"))
}

impl Streams {
    pub fn new(lane: mpsc::Sender<Vec<u8>>) -> Streams {
        Streams {
            lane,
            open: Mutex::default(),
        }
    }

    /// Connect to `port` on this host's loopback: 127.0.0.1, else ::1.
    pub async fn open(&self, p: api::ForwardOpen) -> Result<api::ForwardStream, RpcError> {
        let port = u16::try_from(p.port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "a port from 1 to 65535"))?;
        let tcp = match TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
            Ok(s) => s,
            Err(e4) => TcpStream::connect((Ipv6Addr::LOCALHOST, port))
                .await
                .map_err(|_| {
                    RpcError::new(
                        code::UNAVAILABLE,
                        format!("nothing answers on port {port} of this host's loopback: {e4}"),
                    )
                })?,
        };
        let _ = tcp.set_nodelay(true);
        let id = STREAM_IDS.fetch_add(1, Ordering::Relaxed) + 1;
        let (rd, wr) = tcp.into_split();
        let credit = Arc::new(Semaphore::new(0));
        let (to_port, rx) = mpsc::channel(QUEUED);
        let read = tokio::spawn(from_port(id, rd, credit.clone(), self.lane.clone()));
        let write = tokio::spawn(into_port(wr, rx));
        lock(&self.open).insert(
            id,
            Stream {
                credit,
                to_port,
                tasks: [read.abort_handle(), write.abort_handle()],
            },
        );
        Ok(api::ForwardStream { stream: id })
    }

    /// Queue bytes for the port; waits while [`QUEUED`] chunks are waiting.
    pub async fn write(&self, p: api::ForwardData) -> Result<api::Empty, RpcError> {
        if p.data.len() > CHUNK {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!("at most {CHUNK} bytes per forward.write"),
            ));
        }
        let tx = lock(&self.open)
            .get(&p.stream)
            .map(|s| s.to_port.clone())
            .ok_or_else(|| gone(p.stream))?;
        tx.send((p.data, p.eof))
            .await
            .map_err(|_| RpcError::new(code::UNAVAILABLE, "the port closed the connection"))?;
        Ok(api::Empty {})
    }

    /// More credit for the stream's incoming data, never beyond [`WINDOW`].
    pub fn ack(&self, p: api::ForwardAck) -> Result<api::Empty, RpcError> {
        let open = lock(&self.open);
        let s = open.get(&p.stream).ok_or_else(|| gone(p.stream))?;
        let room = (WINDOW as usize).saturating_sub(s.credit.available_permits());
        s.credit.add_permits((p.bytes as usize).min(room));
        Ok(api::Empty {})
    }

    pub fn close(&self, p: api::ForwardStream) -> Result<api::Empty, RpcError> {
        let s = lock(&self.open)
            .remove(&p.stream)
            .ok_or_else(|| gone(p.stream))?;
        s.tasks.iter().for_each(AbortHandle::abort);
        Ok(api::Empty {})
    }

    /// The connection closed: every stream goes with it.
    pub fn close_all(&self) {
        for (_, s) in lock(&self.open).drain() {
            s.tasks.iter().for_each(AbortHandle::abort);
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// Encode `d` as a `forward.data` bulk record and put it on the lane.
async fn send(lane: &mpsc::Sender<Vec<u8>>, d: api::ForwardData) -> bool {
    let bulk = wire::Bulk {
        stream: d.stream,
        topic: api::topic::FORWARD_DATA.into(),
        data: d.encode_to_vec(),
    };
    lane.send(bulk.encode_to_vec()).await.is_ok()
}

/// Read the port within the coordinator's credit, until it ends.
async fn from_port(
    stream: u64,
    mut rd: OwnedReadHalf,
    credit: Arc<Semaphore>,
    lane: mpsc::Sender<Vec<u8>>,
) {
    let mut buf = vec![0u8; CHUNK];
    let error = loop {
        let Ok(first) = credit.acquire().await else {
            return;
        };
        first.forget();
        let extra = credit.available_permits().min(CHUNK - 1);
        if let Ok(p) = credit.try_acquire_many(extra as u32) {
            p.forget();
        }
        let n = 1 + extra;
        match rd.read(&mut buf[..n]).await {
            Ok(0) => break None,
            Ok(k) => {
                credit.add_permits(n - k);
                let d = api::ForwardData {
                    stream,
                    data: buf[..k].to_vec(),
                    ..api::ForwardData::default()
                };
                if !send(&lane, d).await {
                    return;
                }
            }
            Err(e) => break Some(e.to_string()),
        }
    };
    let end = api::ForwardData {
        stream,
        eof: true,
        error,
        ..api::ForwardData::default()
    };
    let _ = send(&lane, end).await;
}

/// Write what the coordinator sends into the port; `eof` ends its input.
async fn into_port(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<(Vec<u8>, bool)>) {
    while let Some((data, eof)) = rx.recv().await {
        if wr.write_all(&data).await.is_err() {
            return;
        }
        if eof {
            let _ = wr.shutdown().await;
            return;
        }
    }
}

// ---- the coordinator's side -------------------------------------------

/// A stream opened on a host's link: its data arrives on `data`, routed
/// there by the link's reader (and closed when the link ends).
pub struct Link {
    pub client: Arc<Client>,
    pub stream: u64,
    pub data: mpsc::UnboundedReceiver<api::ForwardData>,
}

/// Opens a stream to the forwarded port on the host's current link.
pub type Opener =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Link, String>> + Send>> + Send + Sync>;

struct Listener {
    local_port: u16,
    holds: usize,
    task: JoinHandle<()>,
}

struct Hold {
    key: (String, u16),
    conn: u64,
}

#[derive(Default)]
struct Holds {
    next: u64,
    listeners: HashMap<(String, u16), Listener>,
    holds: HashMap<u64, Hold>,
}

impl Holds {
    fn add(&mut self, key: (String, u16), conn: u64) -> api::HostForwarded {
        let l = self
            .listeners
            .get_mut(&key)
            .expect("a listener for the hold");
        l.holds += 1;
        let local_port = u32::from(l.local_port);
        self.next += 1;
        self.holds.insert(self.next, Hold { key, conn });
        api::HostForwarded {
            forward: self.next,
            local_port,
        }
    }

    /// Drop hold `id`. With the last hold of a port its listener task is
    /// aborted and returned: the port is closed once that task has ended.
    fn remove(&mut self, id: u64) -> Option<JoinHandle<()>> {
        let h = self.holds.remove(&id)?;
        let l = self.listeners.get_mut(&h.key)?;
        l.holds -= 1;
        if l.holds > 0 {
            return None;
        }
        let l = self.listeners.remove(&h.key)?;
        l.task.abort();
        crate::log::info(format!(
            "forward: closed 127.0.0.1:{} to `{}` port {}",
            l.local_port, h.key.0, h.key.1
        ));
        Some(l.task)
    }
}

/// The coordinator's forwards: loopback listeners, by host and port.
#[derive(Default)]
pub struct Forwards {
    inner: Mutex<Holds>,
}

impl std::fmt::Debug for Forwards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwards").finish_non_exhaustive()
    }
}

impl Forwards {
    /// Hold a forward to `port` on host `name` for connection `conn`,
    /// starting its listener if it has none.
    pub async fn hold(
        &self,
        name: &str,
        port: u16,
        conn: u64,
        open: Opener,
    ) -> Result<api::HostForwarded, RpcError> {
        let key = (name.to_owned(), port);
        {
            let mut h = lock(&self.inner);
            if h.listeners.contains_key(&key) {
                return Ok(h.add(key, conn));
            }
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| RpcError::new(code::INTERNAL, format!("no loopback port: {e}")))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
            .port();
        let mut h = lock(&self.inner);
        // Another hold may have started one meanwhile: share it.
        if !h.listeners.contains_key(&key) {
            crate::log::info(format!(
                "forward: 127.0.0.1:{local_port} to `{name}` port {port}"
            ));
            let task = tokio::spawn(accept(listener, open));
            h.listeners.insert(
                key.clone(),
                Listener {
                    local_port,
                    holds: 0,
                    task,
                },
            );
        }
        Ok(h.add(key, conn))
    }

    /// Give up hold `id` of connection `conn`. Returns once the port is
    /// closed, when this was its last hold.
    pub async fn release(&self, id: u64, conn: u64) -> Result<(), RpcError> {
        let closing = {
            let mut h = lock(&self.inner);
            if h.holds.get(&id).is_none_or(|x| x.conn != conn) {
                return Err(RpcError::new(
                    code::NOT_FOUND,
                    format!("this connection holds no forward {id}"),
                ));
            }
            h.remove(id)
        };
        if let Some(task) = closing {
            // Aborted: ends with a cancellation error once the listener
            // is dropped.
            let _ = task.await;
        }
        Ok(())
    }

    /// Connection `conn` closed: its holds go.
    pub fn drop_conn(&self, conn: u64) {
        let mut h = lock(&self.inner);
        let ids: Vec<u64> = h
            .holds
            .iter()
            .filter(|(_, x)| x.conn == conn)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            h.remove(id);
        }
    }
}

/// Accept on the listener until its last hold goes (the task is aborted,
/// and with it every connection through it).
async fn accept(listener: TcpListener, open: Opener) {
    let mut pipes = JoinSet::new();
    loop {
        tokio::select! {
            a = listener.accept() => match a {
                Ok((tcp, _)) => {
                    let open = open.clone();
                    pipes.spawn(async move {
                        match open().await {
                            Ok(link) => pipe(tcp, link).await,
                            Err(e) => crate::log::warn(format!("forward: {e}")),
                        }
                    });
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            },
            Some(_) = pipes.join_next(), if !pipes.is_empty() => {}
        }
    }
}

async fn call(client: &Client, r: api::Request) -> bool {
    client.call(r).await.is_ok()
}

/// A stream's other end, closed on the host however the pipe ends (also
/// when it is aborted with its listener), and the task sending to it.
struct Open {
    client: Arc<Client>,
    stream: u64,
    up: Option<JoinHandle<()>>,
}

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(up) = &self.up {
            up.abort();
        }
        let (client, stream) = (self.client.clone(), self.stream);
        tokio::spawn(async move {
            call(
                &client,
                api::Request::ForwardClose(api::ForwardStream { stream }),
            )
            .await
        });
    }
}

/// Carry one local connection over its stream until both directions end,
/// or either side fails.
async fn pipe(tcp: TcpStream, link: Link) {
    let Link {
        client,
        stream,
        mut data,
    } = link;
    let mut open = Open {
        client: client.clone(),
        stream,
        up: None,
    };
    let ack = |bytes: u32| api::Request::ForwardAck(api::ForwardAck { stream, bytes });
    if !call(&client, ack(WINDOW)).await {
        return;
    }
    let _ = tcp.set_nodelay(true);
    let (mut rd, mut wr) = tcp.into_split();
    let up_client = client.clone();
    let up = open.up.insert(tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let (data, eof) = match rd.read(&mut buf).await {
                Ok(0) | Err(_) => (Vec::new(), true),
                Ok(n) => (buf[..n].to_vec(), false),
            };
            let w = api::ForwardData {
                stream,
                data,
                eof,
                error: None,
            };
            if !call(&up_client, api::Request::ForwardWrite(w)).await || eof {
                return;
            }
        }
    }));
    let mut owed = 0u32;
    let finished = loop {
        let Some(d) = data.recv().await else {
            break false;
        };
        if !d.data.is_empty() {
            if wr.write_all(&d.data).await.is_err() {
                break false;
            }
            owed += d.data.len() as u32;
            if owed >= WINDOW / 4 {
                let (c, r) = (client.clone(), ack(owed));
                tokio::spawn(async move { call(&c, r).await });
                owed = 0;
            }
        }
        if d.eof {
            let _ = wr.shutdown().await;
            break true;
        }
    };
    if finished {
        let _ = up.await;
    }
}
