//! A connection to `dreamerd` over its Unix socket: the wire handshake,
//! typed API calls, and starting the daemon when it is not running.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use prost::Message as _;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
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

#[derive(Debug)]
pub struct Client {
    reader: Reader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    limits: Limits,
    next_id: u64,
    pub welcome: Welcome,
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

impl Client {
    pub async fn connect(socket: &Path, role: &str) -> Result<Client, ClientError> {
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
        Ok(Client {
            reader,
            writer,
            limits,
            next_id: 1,
            welcome,
        })
    }

    /// One typed call. Events that arrive meanwhile are skipped; this
    /// client does not subscribe to any.
    pub async fn call(&mut self, request: api::Request) -> Result<api::Response, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let method = request.method();
        let frame = wire::Request {
            id,
            method: method.to_owned(),
            payload: request.encode(),
            deadline_ms: 0,
        };
        write_frame(
            &mut self.writer,
            Kind::Request,
            &frame.encode_to_vec(),
            self.limits.max_frame,
        )
        .await?;
        loop {
            let (kind, payload) = self.reader.next_frame().await?;
            match kind {
                Kind::Response => {
                    let r = wire::Response::decode(payload.as_slice()).map_err(WireError::from)?;
                    if r.id != id {
                        return Err(WireError::Unexpected("a response to another request").into());
                    }
                    if let Some(code) = r.error_code {
                        return Err(ClientError::Rpc(api::RpcError::new(code, r.error_message)));
                    }
                    return api::Response::decode(method, &r.payload)
                        .map_err(|e| ClientError::Rpc(e.into()));
                }
                Kind::Goodbye => return Err(WireError::Closed.into()),
                Kind::Event | Kind::Ping | Kind::Pong => continue,
                _ => return Err(WireError::Unexpected("frame instead of a response").into()),
            }
        }
    }
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
