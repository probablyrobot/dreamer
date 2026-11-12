//! A connection to `dreamerd` over its Unix socket: the wire handshake,
//! typed API calls, and starting the daemon when it is not running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message as _;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use wire::io::{HandshakeError, Reader, client_handshake, write_frame};
use wire::{Identity, Kind, Limits, Welcome, WireError};

#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening on the socket.
    NotRunning(String),
    Handshake(HandshakeError),
    Wire(WireError),
    /// The daemon answered with an error.
    Rpc(api::RpcError),
    Timeout(&'static str),
    Spawn(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning(why) => write!(f, "dreamerd is not running ({why})"),
            ClientError::Handshake(e) => write!(f, "{e}"),
            ClientError::Wire(e) => write!(f, "{e}"),
            ClientError::Rpc(e) => write!(f, "{e}"),
            ClientError::Timeout(what) => write!(f, "timed out {what}"),
            ClientError::Spawn(e) => write!(f, "could not start dreamerd: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::Wire(e)
    }
}

/// Something the daemon pushed without being asked (after a subscribe).
#[derive(Debug, Clone, PartialEq)]
pub enum Pushed {
    Event {
        topic: String,
        sequence: u64,
        payload: Vec<u8>,
    },
    Bulk {
        stream: u64,
        topic: String,
        data: Vec<u8>,
    },
    /// The connection ended; nothing else follows.
    Closed { error: Option<String> },
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<wire::Response>>>>;

/// A connection to the daemon. Calls may run concurrently; pushed data
/// arrives on [`Client::pushed`].
#[derive(Debug)]
pub struct Client {
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pending: Pending,
    limits: Limits,
    next_id: AtomicU64,
    pushed: Option<mpsc::Receiver<Pushed>>,
    pub welcome: Welcome,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

impl Client {
    pub async fn connect(socket: &Path, role: &str) -> Result<Client, ClientError> {
        Client::connect_with(socket, role, 1024).await
    }

    /// Like [`Client::connect`], holding at most `pushed_capacity` pushed
    /// items before the connection stops reading (back-pressure).
    pub async fn connect_with(
        socket: &Path,
        role: &str,
        pushed_capacity: usize,
    ) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| ClientError::NotRunning(format!("{}: {e}", socket.display())))?;
        let (r, mut writer) = stream.into_split();
        let me = Identity::current(role);
        let mut reader = Reader::new(r, me.limits);
        let (welcome, limits) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            client_handshake(&mut reader, &mut writer, &me),
        )
        .await
        .map_err(|_| ClientError::Timeout("waiting for the daemon's handshake"))?
        .map_err(ClientError::Handshake)?;
        let pending: Pending = Arc::default();
        let (push_tx, push_rx) = mpsc::channel(pushed_capacity.max(1));
        tokio::spawn(read_loop(reader, pending.clone(), push_tx));
        Ok(Client {
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            pending,
            limits,
            next_id: AtomicU64::new(1),
            pushed: Some(push_rx),
            welcome,
        })
    }

    /// The stream of pushed events and bulk data. Taken once.
    pub fn pushed(&mut self) -> Option<mpsc::Receiver<Pushed>> {
        self.pushed.take()
    }

    /// One typed call.
    pub async fn call(&self, request: api::Request) -> Result<api::Response, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let method = request.method();
        let frame = wire::Request {
            id,
            method: method.to_owned(),
            payload: request.encode(),
            deadline_ms: 0,
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| WireError::Closed)?
            .insert(id, tx);
        {
            let mut w = self.writer.lock().await;
            write_frame(
                &mut *w,
                Kind::Request,
                &frame.encode_to_vec(),
                self.limits.max_frame,
            )
            .await?;
        }
        let r = rx.await.map_err(|_| WireError::Closed)?;
        if let Some(code) = r.error_code {
            return Err(ClientError::Rpc(api::RpcError::new(code, r.error_message)));
        }
        api::Response::decode(method, &r.payload).map_err(|e| ClientError::Rpc(e.into()))
    }
}

async fn read_loop(
    mut reader: Reader<OwnedReadHalf>,
    pending: Pending,
    pushed: mpsc::Sender<Pushed>,
) {
    let error = loop {
        let (kind, payload) = match reader.next_frame().await {
            Ok(f) => f,
            Err(WireError::Closed) => break None,
            Err(e) => break Some(e.to_string()),
        };
        match kind {
            Kind::Response => match wire::Response::decode(payload.as_slice()) {
                Ok(r) => {
                    let waiter = pending.lock().ok().and_then(|mut p| p.remove(&r.id));
                    if let Some(w) = waiter {
                        let _ = w.send(r);
                    }
                }
                Err(e) => break Some(e.to_string()),
            },
            Kind::Event => match wire::Event::decode(payload.as_slice()) {
                Ok(e) => {
                    let _ = pushed
                        .send(Pushed::Event {
                            topic: e.topic,
                            sequence: e.sequence,
                            payload: e.payload,
                        })
                        .await;
                }
                Err(e) => break Some(e.to_string()),
            },
            Kind::Bulk => match wire::Bulk::decode(payload.as_slice()) {
                Ok(b) => {
                    let _ = pushed
                        .send(Pushed::Bulk {
                            stream: b.stream,
                            topic: b.topic,
                            data: b.data,
                        })
                        .await;
                }
                Err(e) => break Some(e.to_string()),
            },
            Kind::Goodbye => break None,
            Kind::Ping | Kind::Pong => {}
            _ => break Some("unexpected frame from the daemon".into()),
        }
    };
    // Callers waiting for a response see the connection close.
    if let Ok(mut p) = pending.lock() {
        p.clear();
    }
    let _ = pushed.send(Pushed::Closed { error }).await;
}

/// Paste into a pane, splitting data larger than one request into parts
/// that the daemon reassembles and encodes as one paste.
pub async fn paste(
    client: &Client,
    pane: u32,
    generation: Option<u64>,
    data: &[u8],
) -> Result<api::InputResult, ClientError> {
    const PART: usize = 512 << 10;
    static IDS: AtomicU64 = AtomicU64::new(1);
    let id = IDS.fetch_add(1, Ordering::Relaxed) ^ (u64::from(std::process::id()) << 32);
    let parts: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(PART).collect()
    };
    let mut last = api::InputResult::default();
    for (i, part) in parts.iter().enumerate() {
        let more = i + 1 < parts.len();
        let req = api::TerminalInput {
            pane,
            generation,
            endpoint: None,
            event: Some(api::terminal_input::Event::Paste(api::PasteInput {
                data: part.to_vec(),
                more,
                id,
            })),
        };
        match client.call(api::Request::TerminalInput(req)).await? {
            api::Response::TerminalInput(r) => last = r,
            _ => return Err(WireError::Unexpected("a reply to another method").into()),
        }
    }
    Ok(last)
}

/// How to start the daemon when it is not running.
#[derive(Debug, Clone)]
pub struct Launch {
    pub socket: PathBuf,
    pub daemon: PathBuf,
    /// Extra `dreamerd` arguments, such as the tmux endpoint.
    pub args: Vec<String>,
    /// Environment for the daemon on top of this process's own, such as
    /// `DREAMER_RUNTIME_DIR` and `DREAMER_STATE_DIR`.
    pub env: Vec<(String, String)>,
    pub wait: Duration,
}

impl Launch {
    pub fn new(args: Vec<String>) -> Launch {
        Launch {
            socket: platform::socket_path(),
            daemon: daemon_executable(),
            args,
            env: Vec::new(),
            wait: Duration::from_secs(10),
        }
    }
}

/// `DREAMER_DAEMON`, else `dreamerd` next to the running executable.
pub fn daemon_executable() -> PathBuf {
    if let Some(p) = std::env::var_os("DREAMER_DAEMON") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("dreamerd")))
        .unwrap_or_else(|| PathBuf::from("dreamerd"))
}

/// Connect, starting the daemon first if nothing answers. Returns the
/// client and whether this call started a daemon. Concurrent callers race
/// safely: the daemon's startup lock lets exactly one instance run.
pub async fn ensure_running(launch: &Launch, role: &str) -> Result<(Client, bool), ClientError> {
    match Client::connect(&launch.socket, role).await {
        Ok(c) => return Ok((c, false)),
        Err(ClientError::NotRunning(_)) => {}
        Err(e) => return Err(e),
    }
    Command::new(&launch.daemon)
        .arg("--background")
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/")
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("{}: {e}", launch.daemon.display())))?;
    let deadline = tokio::time::Instant::now() + launch.wait;
    loop {
        match Client::connect(&launch.socket, role).await {
            Ok(c) => return Ok((c, true)),
            Err(ClientError::NotRunning(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(ClientError::NotRunning(_)) => {
                return Err(ClientError::Timeout(
                    "waiting for dreamerd to start; see its log for the reason",
                ));
            }
            Err(e) => return Err(e),
        }
    }
}
