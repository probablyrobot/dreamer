//! An asynchronous tmux control-mode connection (spec 7.1, 7.3).
//!
//! One `tmux -C` subprocess per connection. A reader task parses its stdout
//! and forwards notifications in order through a bounded channel (ordered
//! traffic applies backpressure; nothing is dropped). A scheduler task owns
//! stdin and runs one command batch at a time.
//!
//! A batch that does not finish within its deadline completes with
//! [`CommandError::Timeout`]: its outcome is unknown, and it is never
//! retried. The scheduler then drains the batch's late responses before
//! sending anything else, reports them as [`ConnEvent::LateOutcome`] so the
//! caller can reconcile, and resumes. If they never arrive within the drain
//! limit, the connection fails and the caller must reconnect with a new
//! epoch.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tmux_protocol::{Event, Notification, Parser, ProtocolError, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::command::Batch;

/// Which tmux server to talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    /// The user's default server.
    Default,
    /// `-L name`.
    Name(String),
    /// `-S path`.
    Path(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub program: PathBuf,
    pub socket: Socket,
    /// Passed as `-f`; `None` leaves the user's configuration alone.
    pub config_file: Option<PathBuf>,
    /// Session to attach the control client to.
    pub session: String,
    /// Deadline for one command batch.
    pub command_timeout: Duration,
    /// How long late responses are awaited after a timeout.
    pub drain_limit: Duration,
    /// Pending batches accepted before callers see `Busy`.
    pub queue_capacity: usize,
    /// Notifications buffered before the reader waits for the consumer.
    pub event_capacity: usize,
    /// Replace the child's environment with exactly these variables.
    pub env: Option<Vec<(String, String)>>,
    /// Instead of attaching to `session`, start the control client with
    /// `new-session` and these arguments (which create and attach a new
    /// session, starting the server if needed).
    pub new_session_args: Option<Vec<String>>,
    /// Client flags given to `attach-session -f` (for example
    /// `ignore-size,no-output`).
    pub attach_flags: Option<String>,
}

impl ConnectionConfig {
    pub fn new(program: impl Into<PathBuf>, socket: Socket, session: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            socket,
            config_file: None,
            session: session.into(),
            command_timeout: Duration::from_secs(5),
            drain_limit: Duration::from_secs(60),
            queue_capacity: 256,
            event_capacity: 4096,
            env: None,
            new_session_args: None,
            attach_flags: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Commands flow normally.
    Ready,
    /// A batch timed out; its late responses are being drained.
    Recovering,
    /// The connection is unusable; reconnect with a new epoch.
    Failed,
}

#[derive(Debug)]
pub enum CommandError {
    /// tmux rejected command `index` of the batch; later commands did not run.
    Failed { index: usize, response: Response },
    /// No complete answer in time. The batch may have run fully, partly, or
    /// not at all; reconcile instead of retrying.
    Timeout { batch_id: u64 },
    /// Too many batches already queued.
    Busy,
    /// The connection ended.
    Closed,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { index, response } => {
                let body: Vec<String> = response
                    .body
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect();
                write!(f, "tmux command {index} failed: {}", body.join(" / "))
            }
            Self::Timeout { batch_id } => {
                write!(f, "batch {batch_id} timed out; outcome unknown")
            }
            Self::Busy => write!(f, "command queue full"),
            Self::Closed => write!(f, "tmux connection closed"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Everything the connection reports besides command results.
#[derive(Debug)]
pub enum ConnEvent {
    Notification(Notification),
    /// Responses that arrived after their batch timed out.
    LateOutcome {
        batch_id: u64,
        responses: Vec<Response>,
    },
    /// Only while performance diagnostics are on (the `perf` crate): the
    /// notifications that follow, up to the next `Read`, came from one read
    /// of tmux's output, which returned at `read` and was decoded by
    /// `decoded`.
    Read {
        read: std::time::Instant,
        decoded: std::time::Instant,
    },
    /// The connection ended; no more events follow.
    Closed {
        error: Option<String>,
    },
}

struct Request {
    id: u64,
    line: String,
    expected: usize,
    reply: oneshot::Sender<Result<Vec<Response>, CommandError>>,
}

/// Handle to a running connection. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Connection {
    requests: mpsc::Sender<Request>,
    state: watch::Receiver<LinkState>,
    next_id: Arc<AtomicU64>,
}

impl Connection {
    /// Start `tmux -C attach-session -t <session>` and wait for the attach
    /// response.
    pub async fn spawn(
        config: ConnectionConfig,
    ) -> std::io::Result<(Self, mpsc::Receiver<ConnEvent>)> {
        let mut cmd = Command::new(&config.program);
        if let Some(env) = &config.env {
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        match &config.socket {
            Socket::Default => {}
            Socket::Name(n) => {
                cmd.arg("-L").arg(n);
            }
            Socket::Path(p) => {
                cmd.arg("-S").arg(p);
            }
        }
        if let Some(f) = &config.config_file {
            cmd.arg("-f").arg(f);
        }
        match &config.new_session_args {
            Some(args) => {
                cmd.args(["-C", "new-session"]).args(args);
            }
            None => {
                cmd.args(["-C", "attach-session"]);
                if let Some(flags) = &config.attach_flags {
                    cmd.arg("-f").arg(flags);
                }
                cmd.arg("-t").arg(&config.session);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;

        let (responses_tx, responses_rx) = mpsc::channel::<Response>(64);
        let (events_tx, events_rx) = mpsc::channel::<ConnEvent>(config.event_capacity);
        let (requests_tx, requests_rx) = mpsc::channel::<Request>(config.queue_capacity);
        let (state_tx, state_rx) = watch::channel(LinkState::Ready);

        tokio::spawn(reader(stdout, responses_tx, events_tx.clone()));
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(scheduler(
            child,
            stdin,
            responses_rx,
            requests_rx,
            events_tx,
            state_tx,
            config.command_timeout,
            config.drain_limit,
            ready_tx,
        ));
        match tokio::time::timeout(config.command_timeout, ready_rx).await {
            Ok(Ok(true)) => {}
            _ => return Err(std::io::Error::other("tmux control client did not attach")),
        }
        Ok((
            Self {
                requests: requests_tx,
                state: state_rx,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            events_rx,
        ))
    }

    pub fn state(&self) -> LinkState {
        *self.state.borrow()
    }

    /// Run a batch and return one response per command, in order.
    pub async fn execute(&self, batch: impl Into<Batch>) -> Result<Vec<Response>, CommandError> {
        let batch = batch.into();
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let (reply, rx) = oneshot::channel();
        let request = Request {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            line: batch.encode(),
            expected: batch.len(),
            reply,
        };
        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(CommandError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(CommandError::Closed),
        }
        rx.await.unwrap_or(Err(CommandError::Closed))
    }
}

async fn reader(
    mut stdout: tokio::process::ChildStdout,
    responses: mpsc::Sender<Response>,
    events: mpsc::Sender<ConnEvent>,
) {
    let mut parser = Parser::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::new();
    // The attach is answered first. After it, a block whose guard flags
    // lack 1 is output of a command this client did not send (a key
    // binding run on it, for example) and answers nothing we asked.
    let mut attached = false;
    let error: Option<String> = loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) => break parser.finish().err().map(|e| e.to_string()),
            Ok(n) => n,
            Err(e) => break Some(e.to_string()),
        };
        let read = perf::start();
        perf::count(perf::Counter::TmuxBytes, n as u64);
        let parsed: Result<(), ProtocolError> = parser.feed(&buf[..n], &mut out);
        if let Some(read) = read {
            let decoded = std::time::Instant::now();
            perf::record(perf::Stage::Decode, decoded - read);
            if out.iter().any(|e| matches!(e, Event::Notification(_)))
                && events
                    .send(ConnEvent::Read { read, decoded })
                    .await
                    .is_err()
            {
                return;
            }
        }
        for event in out.drain(..) {
            let delivered = match event {
                Event::Response(r) if attached && r.guard.flags & 1 == 0 => true,
                Event::Response(r) => {
                    attached = true;
                    responses.send(r).await.is_ok()
                }
                Event::Notification(n) => events.send(ConnEvent::Notification(n)).await.is_ok(),
            };
            if !delivered {
                return;
            }
        }
        if let Err(e) = parsed {
            break Some(e.to_string());
        }
    };
    let _ = events.send(ConnEvent::Closed { error }).await;
}

#[allow(clippy::too_many_arguments)]
async fn scheduler(
    mut child: Child,
    mut stdin: ChildStdin,
    mut responses: mpsc::Receiver<Response>,
    mut requests: mpsc::Receiver<Request>,
    events: mpsc::Sender<ConnEvent>,
    state: watch::Sender<LinkState>,
    timeout: Duration,
    drain_limit: Duration,
    ready: oneshot::Sender<bool>,
) {
    // The attach command is answered first.
    let attached = matches!(
        tokio::time::timeout(timeout, responses.recv()).await,
        Ok(Some(r)) if r.ok
    );
    let _ = ready.send(attached);
    if !attached {
        let _ = state.send(LinkState::Failed);
        return;
    }

    while let Some(req) = requests.recv().await {
        let mut line = req.line.into_bytes();
        line.push(b'\n');
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            let _ = req.reply.send(Err(CommandError::Closed));
            break;
        }
        let mut got: Vec<Response> = Vec::with_capacity(req.expected);
        let deadline = tokio::time::Instant::now() + timeout;
        let finished = loop {
            match tokio::time::timeout_at(deadline, responses.recv()).await {
                Ok(Some(r)) => {
                    let ok = r.ok;
                    got.push(r);
                    if !ok || got.len() == req.expected {
                        break true;
                    }
                }
                Ok(None) => {
                    let _ = state.send(LinkState::Failed);
                    let _ = req.reply.send(Err(CommandError::Closed));
                    return;
                }
                Err(_) => break false,
            }
        };
        if finished {
            let last_failed = got.last().is_some_and(|r| !r.ok);
            let result = if last_failed {
                let index = got.len() - 1;
                let response = got.pop().unwrap_or_else(unreachable_response);
                Err(CommandError::Failed { index, response })
            } else {
                Ok(got)
            };
            let _ = req.reply.send(result);
            continue;
        }

        // Timed out: unknown outcome. Never resend; drain the rest. The
        // state changes before the caller hears of the timeout, so a caller
        // never sees `Ready` after its own timeout.
        let _ = state.send(LinkState::Recovering);
        let _ = req
            .reply
            .send(Err(CommandError::Timeout { batch_id: req.id }));
        let drain_deadline = tokio::time::Instant::now() + drain_limit;
        let complete = loop {
            if got.last().is_some_and(|r| !r.ok) || got.len() == req.expected {
                break true;
            }
            match tokio::time::timeout_at(drain_deadline, responses.recv()).await {
                Ok(Some(r)) => got.push(r),
                Ok(None) | Err(_) => break false,
            }
        };
        let _ = events
            .send(ConnEvent::LateOutcome {
                batch_id: req.id,
                responses: got,
            })
            .await;
        if !complete {
            let _ = state.send(LinkState::Failed);
            let _ = child.start_kill();
            return;
        }
        let _ = state.send(LinkState::Ready);
    }
    let _ = stdin.shutdown().await;
    let _ = child.start_kill();
}

fn unreachable_response() -> Response {
    Response {
        guard: tmux_protocol::Guard {
            time: 0,
            number: 0,
            flags: 0,
        },
        ok: false,
        body: Vec::new(),
    }
}
