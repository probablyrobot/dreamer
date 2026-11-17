//! A harness as a child process speaking JSON lines on its standard
//! streams: the plumbing the Claude Code, Codex, and ACP session adapters
//! share. Records are split on LF only. Standard error is kept (its last
//! lines) for a failure's reason.
//!
//! The child runs in its own process group, with the API key variables
//! removed from its environment. Closing it closes its standard input,
//! then sends SIGTERM and finally SIGKILL to its group.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::session::{Input, SCRUBBED, Spec, Update};

/// How long a harness has to answer its start check.
pub(crate) const START: Duration = Duration::from_secs(30);
/// How long a harness has to end after its input closes, and after SIGTERM.
const GRACE: Duration = Duration::from_secs(3);
/// Standard error lines kept for a failure's reason.
const STDERR_LINES: usize = 12;

type Tail = Arc<Mutex<VecDeque<String>>>;

/// The command for `spec`: its program, its arguments, then `extra`, in its
/// directory, with its environment and without any API key variable.
pub(crate) fn command(spec: &Spec, extra: &[String]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .args(extra)
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().map(|(k, v)| (k, v)));
    for v in SCRUBBED {
        cmd.env_remove(v);
    }
    cmd
}

/// What `program --version` says, if it answers quickly.
pub(crate) async fn version(spec: &Spec) -> Option<String> {
    let mut c = tokio::process::Command::new(&spec.program);
    c.args(&spec.args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for v in SCRUBBED {
        c.env_remove(v);
    }
    let child = crate::spawn(&mut c).ok()?;
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// A running child: JSON records it writes arrive on `out`, records sent
/// with [`Proc::send`] go to its standard input.
pub(crate) struct Proc {
    pub pid: u32,
    /// What the harness is called in messages.
    pub name: &'static str,
    out: Option<mpsc::UnboundedReceiver<Value>>,
    tx: mpsc::UnboundedSender<Value>,
    tail: Tail,
    child: tokio::process::Child,
    stop_writer: Option<oneshot::Sender<()>>,
    reader: JoinHandle<()>,
}

impl Proc {
    pub(crate) fn spawn(
        name: &'static str,
        mut cmd: tokio::process::Command,
    ) -> Result<Proc, String> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = crate::spawn(&mut cmd).map_err(|e| format!("{name}: {e}"))?;
        let pid = child.id().unwrap_or_default();
        let stdin = child.stdin.take().ok_or("no standard input")?;
        let stdout = child.stdout.take().ok_or("no standard output")?;
        let tail: Tail = Arc::default();
        if let Some(e) = child.stderr.take() {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(e).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if let Ok(mut t) = tail.lock() {
                        if t.len() == STDERR_LINES {
                            t.pop_front();
                        }
                        t.push_back(l);
                    }
                }
            });
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let (stop_writer, mut stopped) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                tokio::select! {
                    v = rx.recv() => {
                        let Some(v) = v else { break };
                        let mut line = v.to_string();
                        line.push('\n');
                        if stdin.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = stdin.flush().await;
                    }
                    _ = &mut stopped => break,
                }
            }
            // Dropping standard input asks the harness to end.
        });
        let (out_tx, out) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut out = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match out.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Ok(v) = serde_json::from_slice::<Value>(line)
                    && out_tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Ok(Proc {
            pid,
            name,
            out: Some(out),
            tx,
            tail,
            child,
            stop_writer: Some(stop_writer),
            reader,
        })
    }

    pub(crate) fn sender(&self) -> mpsc::UnboundedSender<Value> {
        self.tx.clone()
    }

    fn stderr(&self) -> String {
        self.tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    /// Read records until one `matches`, within the start time; the others
    /// are kept in `early`, in order.
    pub(crate) async fn until(
        &mut self,
        what: &str,
        early: &mut Vec<Value>,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value, String> {
        let name = self.name;
        let out = self.out.as_mut().ok_or("the output is taken")?;
        let deadline = tokio::time::Instant::now() + START;
        loop {
            match tokio::time::timeout_at(deadline, out.recv()).await {
                Err(_) => return Err(format!("{name} did not answer {what} in time")),
                Ok(None) => return Err(format!("{name} ended before answering {what}")),
                Ok(Some(v)) if matches(&v) => return Ok(v),
                Ok(Some(v)) => early.push(v),
            }
        }
    }

    /// End the process at once after a failed start; the reason, with what
    /// it wrote on standard error.
    pub(crate) async fn fail(mut self, version: Option<&str>, why: String) -> String {
        let _ = platform::terminate_group(self.pid, true);
        let _ = self.child.wait().await;
        let stderr = self.stderr();
        let v = version.unwrap_or("of unknown version");
        let mut msg = format!("{} {v} cannot run a Dreamer session: {why}", self.name);
        if !stderr.trim().is_empty() {
            msg.push_str(&format!(" ({})", crate::clip(stderr.trim())));
        }
        msg
    }

    /// Hand the process to a driver: `drive` gets its records, the inputs
    /// sent through the returned control, and the update channel. A
    /// supervisor ends the process when the control closes, and sends
    /// `Update::Exited` after the driver is done.
    pub(crate) fn run<D, F>(
        mut self,
        updates: mpsc::UnboundedSender<Update>,
        drive: D,
    ) -> crate::session::Control
    where
        D: FnOnce(
            mpsc::UnboundedReceiver<Value>,
            mpsc::UnboundedReceiver<Input>,
            mpsc::UnboundedSender<Update>,
        ) -> F,
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (input, inputs) = mpsc::unbounded_channel::<Input>();
        let (close, closed) = oneshot::channel::<()>();
        let out = self
            .out
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1);
        let driver = tokio::spawn(drive(out, inputs, updates.clone()));
        tokio::spawn(async move {
            let pid = self.pid;
            let status = tokio::select! {
                s = self.child.wait() => Some(s),
                _ = closed => None,
            };
            let status = match status {
                Some(s) => s,
                None => {
                    if let Some(s) = self.stop_writer.take() {
                        let _ = s.send(());
                    }
                    match tokio::time::timeout(GRACE, self.child.wait()).await {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = platform::terminate_group(pid, false);
                            match tokio::time::timeout(GRACE, self.child.wait()).await {
                                Ok(s) => s,
                                Err(_) => {
                                    let _ = platform::terminate_group(pid, true);
                                    self.child.wait().await
                                }
                            }
                        }
                    }
                }
            };
            // Whatever it started in its group goes with it.
            let _ = platform::terminate_group(pid, true);
            // A process outside the group may still hold its output open.
            let reader = self.reader;
            let abort = reader.abort_handle();
            if tokio::time::timeout(GRACE, reader).await.is_err() {
                abort.abort();
            }
            let abort = driver.abort_handle();
            if tokio::time::timeout(GRACE, driver).await.is_err() {
                abort.abort();
            }
            let stderr = self
                .tail
                .lock()
                .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            let mut why = match status {
                Ok(s) => format!("{} exited ({s})", self.name),
                Err(e) => format!("{}: {e}", self.name),
            };
            if !stderr.trim().is_empty() {
                why.push_str(&format!(": {}", crate::clip(stderr.trim())));
            }
            let _ = updates.send(Update::Exited(why));
        });
        crate::session::Control {
            input,
            close: Some(close),
        }
    }
}

/// JSON-RPC 2.0 records, numbered from 1. Codex's app server leaves out the
/// `jsonrpc` member; ACP agents expect it.
#[derive(Debug)]
pub(crate) struct Rpc {
    next: u64,
    tagged: bool,
}

impl Rpc {
    pub(crate) fn new(tagged: bool) -> Rpc {
        Rpc { next: 1, tagged }
    }

    fn tag(&self, mut v: Value) -> Value {
        if self.tagged {
            v["jsonrpc"] = json!("2.0");
        }
        v
    }

    /// A request and its id.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> (u64, Value) {
        let id = self.next;
        self.next += 1;
        (
            id,
            self.tag(json!({ "id": id, "method": method, "params": params })),
        )
    }

    pub(crate) fn notify(&self, method: &str, params: Value) -> Value {
        self.tag(json!({ "method": method, "params": params }))
    }

    pub(crate) fn result(&self, id: &Value, result: Value) -> Value {
        self.tag(json!({ "id": id, "result": result }))
    }

    pub(crate) fn error(&self, id: &Value, code: i64, message: &str) -> Value {
        self.tag(json!({ "id": id, "error": { "code": code, "message": message } }))
    }
}

/// The response to request `id`.
pub(crate) fn is_response(v: &Value, id: u64) -> bool {
    v.get("method").is_none() && v["id"].as_u64() == Some(id)
}

/// A JSON-RPC error's message.
pub(crate) fn error_text(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    let msg = e["message"].as_str().unwrap_or("no reason given");
    Some(match e["data"].as_str() {
        Some(d) => format!("{msg} ({d})"),
        None => msg.to_owned(),
    })
}

/// A request id as approval requests carry it.
pub(crate) fn id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
