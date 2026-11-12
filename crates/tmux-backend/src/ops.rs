//! Typed tmux operations (spec 6). Callers name panes, windows, and
//! sessions by id and pass exact argument vectors; command strings are
//! built only here, through the single encoder.

use domain::{TmuxPaneId, TmuxSessionId, TmuxWindowId};

use crate::command::{Batch, TmuxCommand};
use crate::connection::{CommandError, Connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// Side by side (`split-window -h`).
    Horizontal,
    /// One above the other (`split-window -v`).
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Cells(u32),
    Percent(u32),
}

/// What the program of a new pane is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launch {
    /// Exact argv; empty means the default shell.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

fn pane(p: TmuxPaneId) -> String {
    format!("%{}", p.0)
}

fn window(w: TmuxWindowId) -> String {
    format!("@{}", w.0)
}

fn session(s: TmuxSessionId) -> String {
    format!("${}", s.0)
}

fn first_line(r: &tmux_protocol::Response) -> String {
    r.body
        .first()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default()
}

/// tmux's message for a failed command, for reports.
pub fn failure_message(e: &CommandError) -> String {
    match e {
        CommandError::Failed { response, .. } => {
            let text = response
                .body
                .iter()
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                "tmux refused the command".into()
            } else {
                text
            }
        }
        other => other.to_string(),
    }
}

/// Split `target`; the new pane becomes active, as in tmux. Returns its id.
pub async fn split_pane(
    conn: &Connection,
    target: TmuxPaneId,
    split: Split,
    size: Option<Size>,
    launch: &Launch,
) -> Result<TmuxPaneId, CommandError> {
    let mut cmd = TmuxCommand::new("split-window")
        .flag(match split {
            Split::Horizontal => "-h",
            Split::Vertical => "-v",
        })
        .flag("-P")
        .opt_format("-F", "#{pane_id}")
        .opt("-t", pane(target));
    match size {
        Some(Size::Cells(c)) => cmd = cmd.opt("-l", c.to_string()),
        Some(Size::Percent(p)) => cmd = cmd.opt("-l", format!("{p}%")),
        None => {}
    }
    if let Some(cwd) = &launch.cwd {
        cmd = cmd.opt("-c", cwd.clone());
    }
    // One argument would go through `sh -c`; wrapping it keeps argv exact.
    let argv: Vec<String> = match launch.argv.as_slice() {
        [] => Vec::new(),
        [one] => vec!["/usr/bin/env".into(), one.clone()],
        many => many.to_vec(),
    };
    for a in argv {
        cmd = cmd.arg(a);
    }
    let r = conn.execute(cmd).await?;
    let line = r.first().map(first_line).unwrap_or_default();
    line.strip_prefix('%')
        .and_then(|n| n.parse().ok())
        .map(TmuxPaneId)
        .ok_or(CommandError::Closed)
}

pub async fn resize_pane(
    conn: &Connection,
    target: TmuxPaneId,
    cols: Option<u32>,
    rows: Option<u32>,
) -> Result<(), CommandError> {
    let mut cmd = TmuxCommand::new("resize-pane").opt("-t", pane(target));
    if let Some(c) = cols {
        cmd = cmd.opt("-x", c.to_string());
    }
    if let Some(r) = rows {
        cmd = cmd.opt("-y", r.to_string());
    }
    conn.execute(cmd).await.map(drop)
}

pub async fn select_pane(conn: &Connection, target: TmuxPaneId) -> Result<(), CommandError> {
    conn.execute(TmuxCommand::new("select-pane").opt("-t", pane(target)))
        .await
        .map(drop)
}

pub async fn select_window(conn: &Connection, target: TmuxWindowId) -> Result<(), CommandError> {
    conn.execute(TmuxCommand::new("select-window").opt("-t", window(target)))
        .await
        .map(drop)
}

/// Toggle zoom of the pane's window onto this pane.
pub async fn toggle_zoom(conn: &Connection, target: TmuxPaneId) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("resize-pane")
            .flag("-Z")
            .opt("-t", pane(target)),
    )
    .await
    .map(drop)
}

pub async fn rename_window(
    conn: &Connection,
    target: TmuxWindowId,
    name: &str,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("rename-window")
            .opt("-t", window(target))
            .literal_for_format(name),
    )
    .await
    .map(drop)
}

pub async fn rename_session(
    conn: &Connection,
    target: TmuxSessionId,
    name: &str,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("rename-session")
            .opt("-t", session(target))
            .literal_for_format(name),
    )
    .await
    .map(drop)
}

/// Bytes per `send-keys -H` command and commands per batch: a batch is
/// one bounded command line (16 KiB of input).
const SEND_CHUNK: usize = 256;
const SEND_BATCH: usize = 64;

/// Exact bytes to the pane's program (`send-keys -H`). Large input goes in
/// bounded batches, each acknowledged before the next is sent, so nothing
/// is reordered and no command line grows without bound. Returns the
/// number of batches.
pub async fn send_bytes(
    conn: &Connection,
    target: TmuxPaneId,
    bytes: &[u8],
) -> Result<u32, CommandError> {
    let mut batches = 0u32;
    for block in bytes.chunks(SEND_CHUNK * SEND_BATCH) {
        let mut batch = Batch::new();
        for chunk in block.chunks(SEND_CHUNK) {
            let mut cmd = TmuxCommand::new("send-keys")
                .flag("-H")
                .opt("-t", pane(target));
            for b in chunk {
                cmd = cmd.arg(format!("{b:02x}"));
            }
            batch = batch.push(cmd);
        }
        conn.execute(batch).await?;
        batches += 1;
    }
    Ok(batches)
}

/// tmux key names (`Enter`, `C-c`, `Up`).
pub async fn send_keys(
    conn: &Connection,
    target: TmuxPaneId,
    keys: &[String],
) -> Result<(), CommandError> {
    let mut cmd = TmuxCommand::new("send-keys").opt("-t", pane(target));
    for k in keys {
        cmd = cmd.arg(k.clone());
    }
    conn.execute(cmd).await.map(drop)
}

pub async fn kill_pane(conn: &Connection, target: TmuxPaneId) -> Result<(), CommandError> {
    conn.execute(TmuxCommand::new("kill-pane").opt("-t", pane(target)))
        .await
        .map(drop)
}

pub async fn kill_window(conn: &Connection, target: TmuxWindowId) -> Result<(), CommandError> {
    conn.execute(TmuxCommand::new("kill-window").opt("-t", window(target)))
        .await
        .map(drop)
}

/// Whether the pane's window is zoomed.
pub async fn window_zoomed(conn: &Connection, target: TmuxWindowId) -> Result<bool, CommandError> {
    let r = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .opt("-t", window(target))
                .format("#{window_zoomed_flag}"),
        )
        .await?;
    Ok(r.first().map(first_line).as_deref() == Some("1"))
}
