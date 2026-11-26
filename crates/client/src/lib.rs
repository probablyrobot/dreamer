//! A connection to `dreamerd`: the wire handshake, typed API calls, and
//! starting the daemon when it is not running. The connection is normally
//! the daemon's Unix socket; [`Client::over`] runs it over any byte stream,
//! such as an SSH child's pipes to a remote `dream bridge --stdio`.

pub mod frames;
pub mod ssh;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
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

/// Calls waiting for their response, and whether the connection has
/// closed: kept under one lock, so a call can never wait on a connection
/// that already ended.
#[derive(Default)]
struct Calls {
    waiting: HashMap<u64, oneshot::Sender<wire::Response>>,
    closed: bool,
}

type Pending = Arc<Mutex<Calls>>;

/// A connection to the daemon. Calls may run concurrently; pushed data
/// arrives on [`Client::pushed`].
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

pub struct Client {
    writer: Arc<tokio::sync::Mutex<Writer>>,
    pending: Pending,
    limits: Limits,
    next_id: AtomicU64,
    pushed: Option<mpsc::Receiver<Pushed>>,
    pub welcome: Welcome,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("welcome", &self.welcome)
            .finish_non_exhaustive()
    }
}

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
        Client::connect_features(socket, role, pushed_capacity, &[]).await
    }

    /// Like [`Client::connect_with`], asking for optional wire `features`
    /// (`wire::feature`). [`Client::has_feature`] says which the daemon
    /// agreed to.
    pub async fn connect_features(
        socket: &Path,
        role: &str,
        pushed_capacity: usize,
        features: &[&str],
    ) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| ClientError::NotRunning(format!("{}: {e}", socket.display())))?;
        let (r, w) = stream.into_split();
        Client::over_features(r, w, role, pushed_capacity, HANDSHAKE_TIMEOUT, features).await
    }

    /// Handshake and run over any byte stream: the read and write halves of
    /// a connection that reaches a daemon, however it is carried.
    pub async fn over<R, W>(
        r: R,
        w: W,
        role: &str,
        pushed_capacity: usize,
    ) -> Result<Client, ClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Client::over_within(r, w, role, pushed_capacity, HANDSHAKE_TIMEOUT).await
    }

    /// Like [`Client::over`], allowing `handshake` for the peer's answer: a
    /// stream through SSH spends part of it connecting and authenticating.
    pub async fn over_within<R, W>(
        r: R,
        w: W,
        role: &str,
        pushed_capacity: usize,
        handshake: Duration,
    ) -> Result<Client, ClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Client::over_features(r, w, role, pushed_capacity, handshake, &[]).await
    }

    /// Like [`Client::over_within`], asking for optional wire `features`.
    pub async fn over_features<R, W>(
        r: R,
        w: W,
        role: &str,
        pushed_capacity: usize,
        handshake: Duration,
        features: &[&str],
    ) -> Result<Client, ClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut writer: Writer = Box::new(w);
        let mut me = Identity::current(role);
        me.features = features.iter().map(|f| (*f).to_owned()).collect();
        let mut reader = Reader::new(r, me.limits);
        let (welcome, limits) =
            tokio::time::timeout(handshake, client_handshake(&mut reader, &mut writer, &me))
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

    /// Whether both sides agreed on an optional wire feature.
    pub fn has_feature(&self, feature: &str) -> bool {
        self.welcome.features.iter().any(|f| f == feature)
    }

    /// The stream of pushed events and bulk data. Taken once.
    pub fn pushed(&mut self) -> Option<mpsc::Receiver<Pushed>> {
        self.pushed.take()
    }

    /// One typed call.
    pub async fn call(&self, request: api::Request) -> Result<api::Response, ClientError> {
        let method = request.method();
        let payload = self.call_raw(method, request.encode()).await?;
        api::Response::decode(method, &payload).map_err(|e| ClientError::Rpc(e.into()))
    }

    /// One call with the parameters already encoded, returning the encoded
    /// result: for forwarding a call without decoding it.
    pub async fn call_raw(&self, method: &str, payload: Vec<u8>) -> Result<Vec<u8>, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = wire::Request {
            id,
            method: method.to_owned(),
            payload,
            deadline_ms: 0,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut calls = self.pending.lock().map_err(|_| WireError::Closed)?;
            if calls.closed {
                return Err(WireError::Closed.into());
            }
            calls.waiting.insert(id, tx);
        }
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
        Ok(r.payload)
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: Reader<R>,
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
                    let waiter = pending
                        .lock()
                        .ok()
                        .and_then(|mut p| p.waiting.remove(&r.id));
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
    // Callers waiting for a response see the connection close, and later
    // calls fail at once instead of waiting for a reply that cannot come.
    if let Ok(mut p) = pending.lock() {
        p.closed = true;
        p.waiting.clear();
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
            client: None,
            lease: None,
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
    let mut child = start(launch)?;
    let deadline = tokio::time::Instant::now() + launch.wait;
    loop {
        match Client::connect(&launch.socket, role).await {
            Ok(c) => return Ok((c, true)),
            Err(ClientError::NotRunning(_)) if tokio::time::Instant::now() < deadline => {
                exited(&mut child)?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(ClientError::NotRunning(_)) => return Err(start_timeout()),
            Err(e) => return Err(e),
        }
    }
}

/// The daemon's socket as a plain byte stream, starting the daemon first
/// if nothing answers. Nothing is read or written: the caller (the stdio
/// bridge) carries a whole protocol session through it unchanged.
pub async fn ensure_running_raw(launch: &Launch) -> Result<(UnixStream, bool), ClientError> {
    let connect = || async {
        UnixStream::connect(&launch.socket)
            .await
            .map_err(|e| ClientError::NotRunning(format!("{}: {e}", launch.socket.display())))
    };
    if let Ok(s) = connect().await {
        return Ok((s, false));
    }
    let mut child = start(launch)?;
    let deadline = tokio::time::Instant::now() + launch.wait;
    loop {
        match connect().await {
            Ok(s) => return Ok((s, true)),
            Err(_) if tokio::time::Instant::now() < deadline => {
                exited(&mut child)?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => return Err(start_timeout()),
        }
    }
}

/// Start a detached daemon: no standard input or output of ours, so it
/// outlives whoever started it (a CLI call, or a bridge under SSH). Its
/// standard error is kept for the reason if it stops during startup; once
/// running it writes only to its log.
fn start(launch: &Launch) -> Result<std::process::Child, ClientError> {
    for socket in [
        launch.socket.clone(),
        launch.socket.with_file_name("dreamerd.rpc.sock"),
    ] {
        platform::check_socket_path(&socket).map_err(ClientError::Spawn)?;
    }
    Command::new(&launch.daemon)
        .arg("--background")
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .current_dir("/")
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("{}: {e}", launch.daemon.display())))
}

/// `Err` with the daemon's reason when it has stopped during startup. One
/// that found another daemon already running (exit 3) is not a failure:
/// that one answers.
fn exited(child: &mut std::process::Child) -> Result<(), ClientError> {
    let Ok(Some(status)) = child.try_wait() else {
        return Ok(());
    };
    if status.success() || status.code() == Some(3) {
        return Ok(());
    }
    let err = child.stderr.take().map(written).unwrap_or_default();
    let why = err
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("dreamerd: "))
        .or_else(|| err.lines().rev().find(|l| !l.trim().is_empty()))
        .map_or_else(|| format!("it exited with {status}"), str::to_owned);
    Err(ClientError::Spawn(why))
}

/// What an exited daemon wrote to its standard error. All of it is in the
/// pipe already, so only that is read, without waiting for the end: a
/// process another thread spawned while the pipe was being made can hold a
/// copy of its writing end (on macOS it is marked close-on-exec only after
/// it is made), and the end would come only when that process exits.
fn written(stderr: std::process::ChildStderr) -> String {
    use std::io::Read;
    let mut pipe = std::fs::File::from(std::os::fd::OwnedFd::from(stderr));
    let mut bytes = Vec::new();
    if rustix::io::ioctl_fionbio(&pipe, true).is_ok() {
        // Stops at the end or where the pipe is empty; keeps what it read.
        let _ = pipe.read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn start_timeout() -> ClientError {
    ClientError::Timeout("waiting for dreamerd to start; see its log for the reason")
}
