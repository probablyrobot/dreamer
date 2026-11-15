//! Running a CLI: a child process in its own process group, its output
//! read line by line into [`Event`]s, and cancellation or a timeout that
//! ends the whole group (the CLI and anything it started, such as the
//! MCP server).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::{mpsc, watch};

use crate::{Event, Failure, Kind, Request, arguments, locate, parser};

/// How long a cancelled group has to stop before it is killed.
const GRACE: Duration = Duration::from_secs(2);

/// A run in progress. Events end with exactly one `Done` or `Failed`.
#[derive(Debug)]
pub struct Run {
    pub events: mpsc::UnboundedReceiver<Event>,
    cancel: watch::Sender<bool>,
    /// The child's process group.
    pub pgid: u32,
}

impl Run {
    pub(crate) fn new(
        events: mpsc::UnboundedReceiver<Event>,
        cancel: watch::Sender<bool>,
        pgid: u32,
    ) -> Run {
        Run {
            events,
            cancel,
            pgid,
        }
    }

    /// Stop the run: its process group gets SIGTERM, then SIGKILL.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

/// Start `req` on `kind`'s CLI. Must be called inside a Tokio runtime.
pub fn start(kind: Kind, req: &Request) -> Result<Run, Failure> {
    let program = locate(kind).ok_or(Failure::NotInstalled)?;
    start_program(kind, &program, req)
}

/// [`start`] with the executable given.
pub fn start_program(kind: Kind, program: &Path, req: &Request) -> Result<Run, Failure> {
    if !program.is_file() {
        return Err(Failure::NotInstalled);
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(arguments(kind, req))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(d) = &req.cwd {
        cmd.current_dir(d);
    }
    cmd.envs(req.env.iter().map(|(k, v)| (k, v)));
    for v in kind.scrubbed() {
        cmd.env_remove(v);
    }
    let mut child = crate::spawn(&mut cmd)
        .map_err(|e| Failure::Failed(format!("{}: {e}", program.display())))?;
    let pgid = child.id().unwrap_or_default();
    let (tx, events) = mpsc::unbounded_channel();
    let (cancel, mut cancelled) = watch::channel(false);
    let mut parse = parser(kind, req);
    let timeout = req.timeout;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    tokio::spawn(async move {
        // Standard error is kept (its end) for a failure's reason.
        let err = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_end(&mut buf).await;
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let lines: Vec<&str> = text.lines().rev().take(8).collect();
            lines.into_iter().rev().collect::<Vec<_>>().join("\n")
        });
        let mut lines = stdout.map(|o| BufReader::new(o).lines());
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        let mut ended = false;
        let mut stop = None;
        loop {
            let next = async {
                match lines.as_mut() {
                    Some(l) => l.next_line().await,
                    None => Ok(None),
                }
            };
            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = next => match line {
                    Ok(Some(line)) => {
                        let mut halt = false;
                        for e in parse.line(&line, &mut halt) {
                            ended |= matches!(e, Event::Done(_) | Event::Failed(_));
                            let _ = tx.send(e);
                        }
                        if halt {
                            stop = Some(None);
                            break;
                        }
                    }
                    _ => break,
                },
                _ = cancelled.changed() => {
                    stop = Some(Some(Failure::Cancelled));
                    break;
                }
                () = sleep => {
                    stop = Some(Some(Failure::TimedOut));
                    break;
                }
            }
        }
        if let Some(why) = stop {
            let _ = platform::terminate_group(pgid, false);
            if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
                let _ = platform::terminate_group(pgid, true);
                let _ = child.wait().await;
            }
            if let Some(why) = why
                && !ended
            {
                let _ = tx.send(Event::Failed(why));
            }
            return;
        }
        let status = child.wait().await;
        let stderr = err.await.unwrap_or_default();
        if !ended {
            let why = match status {
                Ok(s) if s.success() => "the CLI ended without an answer".to_owned(),
                Ok(s) => format!("the CLI exited ({s})"),
                Err(e) => e.to_string(),
            };
            let detail = if stderr.trim().is_empty() {
                why
            } else {
                format!("{why}: {}", stderr.trim())
            };
            let _ = tx.send(Event::Failed(crate::classify(&detail)));
        }
    });
    Ok(Run {
        events,
        cancel,
        pgid,
    })
}
