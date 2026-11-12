//! A blocking control-mode client for tests and the bootstrap prototype.
//!
//! A reader thread parses stdout with `tmux-protocol` and forwards events in
//! order over a channel. Commands are written by the caller; responses are
//! recognized in the event stream (tmux answers commands in order).

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tmux_protocol::{Event, Parser, ProtocolError, Response};

use crate::server::TmuxServer;

#[derive(Debug)]
pub enum ControlError {
    Io(io::Error),
    Protocol(ProtocolError),
    /// The client exited or its stream ended.
    Closed,
    Timeout,
    /// A command failed; the response body explains why.
    Command(Response),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "control client I/O: {e}"),
            Self::Protocol(e) => write!(f, "control protocol: {e}"),
            Self::Closed => write!(f, "control client closed"),
            Self::Timeout => write!(f, "timed out waiting for tmux"),
            Self::Command(r) => {
                let body: Vec<String> = r
                    .body
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect();
                write!(f, "tmux command failed: {}", body.join(" / "))
            }
        }
    }
}

impl std::error::Error for ControlError {}

impl From<io::Error> for ControlError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug)]
pub struct ControlClient {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<Result<Event, ProtocolError>>,
}

impl ControlClient {
    /// Attach a `tmux -C` client to `session` on an isolated server and wait
    /// for the attach command's own response.
    pub fn attach(server: &TmuxServer, session: &str) -> Result<Self, ControlError> {
        let mut child = server
            .command()
            .args(["-C", "attach-session", "-t", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or(ControlError::Closed)?;
        let mut stdout = child.stdout.take().ok_or(ControlError::Closed)?;
        let (tx, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("tmux-control-reader".into())
            .spawn(move || {
                let mut parser = Parser::default();
                let mut buf = vec![0u8; 64 * 1024];
                let mut out = Vec::new();
                loop {
                    let n = match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            if let Err(e) = parser.finish() {
                                let _ = tx.send(Err(e));
                            }
                            return;
                        }
                        Ok(n) => n,
                    };
                    let result = parser.feed(&buf[..n], &mut out);
                    for event in out.drain(..) {
                        if tx.send(Ok(event)).is_err() {
                            return;
                        }
                    }
                    if let Err(e) = result {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            })?;
        let mut client = Self {
            child,
            stdin,
            events,
        };
        // The attach itself is answered with an empty response block.
        loop {
            if let Event::Response(_) = client.recv(Duration::from_secs(10))? {
                break;
            }
        }
        Ok(client)
    }

    /// Write one command line (no trailing newline needed).
    pub fn send(&mut self, line: &str) -> Result<(), ControlError> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.stdin.write_all(&bytes)?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Next event, waiting at most `timeout`.
    pub fn recv(&mut self, timeout: Duration) -> Result<Event, ControlError> {
        match self.events.recv_timeout(timeout) {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(e)) => Err(ControlError::Protocol(e)),
            Err(RecvTimeoutError::Timeout) => Err(ControlError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(ControlError::Closed),
        }
    }

    /// Next event if one is already available.
    pub fn try_recv(&mut self) -> Result<Option<Event>, ControlError> {
        match self.events.try_recv() {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(e)) => Err(ControlError::Protocol(e)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(ControlError::Closed),
        }
    }

    /// Send a single command and wait for its response, passing every
    /// notification seen meanwhile to `on_note`.
    pub fn command(
        &mut self,
        line: &str,
        timeout: Duration,
        mut on_event: impl FnMut(Event),
    ) -> Result<Response, ControlError> {
        self.send(line)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left)? {
                Event::Response(r) if r.ok => return Ok(r),
                Event::Response(r) => return Err(ControlError::Command(r)),
                other => on_event(other),
            }
        }
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"detach-client\n");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
