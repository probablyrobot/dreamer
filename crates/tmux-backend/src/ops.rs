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
    /// Environment for the new pane's program (`-e NAME=VALUE`).
    pub env: Vec<(String, String)>,
    /// Create a window or pane without making it current (`-d`), so
    /// attached clients keep what they are looking at.
    pub background: bool,
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

/// Split `target`; the new pane becomes active, as in tmux, unless the
/// launch is in the background. Returns its id.
pub async fn split_pane(
    conn: &Connection,
    target: TmuxPaneId,
    split: Split,
    size: Option<Size>,
    launch: &Launch,
) -> Result<TmuxPaneId, CommandError> {
    let mut cmd = TmuxCommand::new("split-window").flag(match split {
        Split::Horizontal => "-h",
        Split::Vertical => "-v",
    });
    if launch.background {
        cmd = cmd.flag("-d");
    }
    let mut cmd = cmd
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
    for (k, v) in &launch.env {
        cmd = cmd.opt("-e", format!("{k}={v}"));
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

/// The process id of the program running in a pane.
pub async fn pane_pid(conn: &Connection, target: TmuxPaneId) -> Result<u32, CommandError> {
    let r = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .opt("-t", pane(target))
                .opt_format("-F", "#{pane_pid}"),
        )
        .await?;
    r.first()
        .map(first_line)
        .and_then(|l| l.trim().parse().ok())
        .ok_or(CommandError::Closed)
}

/// Create a window at the end of a session; returns its first pane.
pub async fn new_window(
    conn: &Connection,
    target: TmuxSessionId,
    name: Option<&str>,
    launch: &Launch,
) -> Result<TmuxPaneId, CommandError> {
    let mut cmd = TmuxCommand::new("new-window").flag("-P");
    if launch.background {
        cmd = cmd.flag("-d");
    }
    let mut cmd = cmd
        .opt_format("-F", "#{pane_id}")
        .opt("-t", format!("{}:", session(target)));
    if let Some(n) = name {
        cmd = cmd.opt("-n", n.to_owned());
    }
    if let Some(cwd) = &launch.cwd {
        cmd = cmd.opt("-c", cwd.clone());
    }
    for (k, v) in &launch.env {
        cmd = cmd.opt("-e", format!("{k}={v}"));
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

/// Size a window for this control client (`refresh-client -C @W:CxR`).
/// tmux's `window-size` option still decides how this client's size
/// combines with other clients'.
pub async fn size_window(
    conn: &Connection,
    target: TmuxWindowId,
    cols: u16,
    rows: u16,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("refresh-client").opt("-C", format!("{}:{cols}x{rows}", window(target))),
    )
    .await
    .map(drop)
}

/// Stop sizing a window: tmux sizes it from its other clients again
/// (`refresh-client -C @W:` clears this control client's size for it).
pub async fn clear_window_size(
    conn: &Connection,
    target: TmuxWindowId,
) -> Result<(), CommandError> {
    conn.execute(TmuxCommand::new("refresh-client").opt("-C", format!("{}:", window(target))))
        .await
        .map(drop)
}

/// Whether tmux leaves this control client out when sizing windows while
/// another client is attached (`refresh-client -f [!]ignore-size`). A
/// control client that ever sized a window otherwise counts with its own
/// default size even after its window sizes are cleared.
pub async fn set_ignore_size(conn: &Connection, ignore: bool) -> Result<(), CommandError> {
    let flag = if ignore {
        "ignore-size"
    } else {
        "!ignore-size"
    };
    conn.execute(TmuxCommand::new("refresh-client").opt("-f", flag))
        .await
        .map(drop)
}

/// Keep the pane, with its last output, when its program exits
/// (`remain-on-exit`), so a finished job can still be read.
pub async fn keep_on_exit(conn: &Connection, target: TmuxPaneId) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("set-option")
            .flag("-p")
            .opt("-t", pane(target))
            .arg("remain-on-exit")
            .arg("on"),
    )
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

/// Move a pane into window `target`, beside that window's active pane.
/// Focus stays where it was. A window left with no panes closes.
pub async fn join_pane(
    conn: &Connection,
    source: TmuxPaneId,
    target: TmuxWindowId,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("join-pane")
            .flag("-d")
            .opt("-s", pane(source))
            .opt("-t", window(target)),
    )
    .await
    .map(drop)
}

/// Move a pane into a new window of its own in the same session, named
/// `name`. Focus stays where it was. A pane already alone in its window
/// keeps that window, renamed (tmux prints nothing then, so the caller
/// reads the result back rather than parsing it).
pub async fn break_pane(
    conn: &Connection,
    source: TmuxPaneId,
    name: &str,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("break-pane")
            .flag("-d")
            .opt("-s", pane(source))
            .opt("-n", name.to_owned()),
    )
    .await
    .map(drop)
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

/// The pane user option that makes a pane a session's harness drawer: the
/// session id (`$N`). It lives in tmux, so a restarted daemon finds every
/// drawer again from the inventory.
pub const DRAWER_FOR: &str = "@dreamer_drawer_for";
/// The pane that was focused before the drawer was shown (`%N`), focused
/// again when it hides.
pub const DRAWER_RETURN: &str = "@dreamer_drawer_return";
/// The harness the drawer was started with.
pub const DRAWER_HARNESS: &str = "@dreamer_drawer_harness";
/// The session hidden drawers wait in, created when first needed. Clients
/// leave it out of their tabs and session lists.
pub const DRAWER_HOLDING: &str = "_dreamer_drawers";

/// Where a docked pane goes relative to the window's other panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bottom,
    Right,
    Left,
}

/// Move a pane into the window of pane `target`, across the whole window
/// on `side` (`join-pane -f`), taking `size` of it. Focus stays where it
/// was. A window left with no panes closes.
pub async fn dock_pane(
    conn: &Connection,
    source: TmuxPaneId,
    target: TmuxPaneId,
    side: Side,
    size: Option<Size>,
) -> Result<(), CommandError> {
    let mut cmd = TmuxCommand::new("join-pane").flag("-d").flag("-f");
    cmd = match side {
        Side::Bottom => cmd.flag("-v"),
        Side::Right => cmd.flag("-h"),
        Side::Left => cmd.flag("-h").flag("-b"),
    };
    match size {
        Some(Size::Cells(c)) => cmd = cmd.opt("-l", c.to_string()),
        Some(Size::Percent(p)) => cmd = cmd.opt("-l", format!("{p}%")),
        None => {}
    }
    conn.execute(cmd.opt("-s", pane(source)).opt("-t", pane(target)))
        .await
        .map(drop)
}

/// Move a pane into a new window at the end of session `target`, named
/// `name`. Focus stays where it was.
pub async fn break_pane_to(
    conn: &Connection,
    source: TmuxPaneId,
    target: TmuxSessionId,
    name: &str,
) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("break-pane")
            .flag("-d")
            .opt("-s", pane(source))
            .opt("-t", format!("{}:", session(target)))
            .opt("-n", name.to_owned()),
    )
    .await
    .map(drop)
}

/// Create a detached session named `name`; returns it and its first pane.
pub async fn new_session(
    conn: &Connection,
    name: &str,
    launch: &Launch,
) -> Result<(TmuxSessionId, TmuxPaneId), CommandError> {
    let mut cmd = TmuxCommand::new("new-session")
        .flag("-d")
        .flag("-P")
        .opt_format("-F", "#{session_id} #{pane_id}")
        .opt("-s", name.to_owned());
    if let Some(cwd) = &launch.cwd {
        cmd = cmd.opt("-c", cwd.clone());
    }
    for (k, v) in &launch.env {
        cmd = cmd.opt("-e", format!("{k}={v}"));
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
    let (s, p) = line.split_once(' ').ok_or(CommandError::Closed)?;
    let s = s.strip_prefix('$').and_then(|n| n.parse().ok());
    let p = p.strip_prefix('%').and_then(|n| n.parse().ok());
    match (s, p) {
        (Some(s), Some(p)) => Ok((TmuxSessionId(s), TmuxPaneId(p))),
        _ => Err(CommandError::Closed),
    }
}

/// Set a user option (`@name`) on a pane; an empty value unsets it.
pub async fn set_pane_option(
    conn: &Connection,
    target: TmuxPaneId,
    name: &'static str,
    value: &str,
) -> Result<(), CommandError> {
    debug_assert!(name.starts_with('@'));
    let cmd = TmuxCommand::new("set-option")
        .flag("-p")
        .opt("-t", pane(target));
    let cmd = if value.is_empty() {
        cmd.flag("-u").arg(name)
    } else {
        cmd.arg(name).arg(value.to_owned())
    };
    conn.execute(cmd).await.map(drop)
}

/// A pane's user option (`@name`); empty when unset.
pub async fn pane_option(
    conn: &Connection,
    target: TmuxPaneId,
    name: &'static str,
) -> Result<String, CommandError> {
    debug_assert!(name.starts_with('@'));
    let r = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .opt("-t", pane(target))
                .format(format!("#{{{name}}}")),
        )
        .await?;
    Ok(r.first().map(first_line).unwrap_or_default())
}

/// Keep a session when no client is attached to it, whatever the global
/// `destroy-unattached` is (set on that session only).
pub async fn keep_unattached(conn: &Connection, target: TmuxSessionId) -> Result<(), CommandError> {
    conn.execute(
        TmuxCommand::new("set-option")
            .opt("-t", session(target))
            .arg("destroy-unattached")
            .arg("off"),
    )
    .await
    .map(drop)
}

/// The working directory of the program in a pane, when tmux can tell.
pub async fn pane_current_path(
    conn: &Connection,
    target: TmuxPaneId,
) -> Result<Option<String>, CommandError> {
    let r = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .opt("-t", pane(target))
                .format("#{pane_current_path}"),
        )
        .await?;
    Ok(r.first().map(first_line).filter(|p| !p.is_empty()))
}

/// The tmux options that decide how a person's keys reach tmux and how
/// copy mode looks, as tmux reports them. Read only; never changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyOptions {
    /// `prefix`, as a tmux key name (`C-b`), or `None`.
    pub prefix: String,
    /// `prefix2`, as a tmux key name, or `None`.
    pub prefix2: String,
    /// `mode-keys`: `vi` or `emacs`.
    pub mode_keys: String,
    /// `mode-style`, as tmux reports it (`bg=yellow,fg=black`).
    pub mode_style: String,
}

fn show_global(name: &'static str, window: bool) -> TmuxCommand {
    let cmd = TmuxCommand::new("show-options").flag("-g");
    let cmd = if window { cmd.flag("-w") } else { cmd };
    cmd.flag("-q").flag("-v").arg(name)
}

/// Read the global `prefix`, `prefix2`, `mode-keys`, and `mode-style`.
pub async fn key_options(conn: &Connection) -> Result<KeyOptions, CommandError> {
    let r = conn
        .execute(
            Batch::new()
                .push(show_global("prefix", false))
                .push(show_global("prefix2", false))
                .push(show_global("mode-keys", true))
                .push(show_global("mode-style", true)),
        )
        .await?;
    let line = |i: usize| r.get(i).map(first_line).unwrap_or_default();
    Ok(KeyOptions {
        prefix: line(0),
        prefix2: line(1),
        mode_keys: line(2),
        mode_style: line(3),
    })
}

/// One of tmux's paste buffers, without its text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BufferInfo {
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// The start of the text, with control characters escaped by tmux.
    pub sample: String,
}

/// tmux's paste buffers, newest first.
pub async fn list_buffers(conn: &Connection) -> Result<Vec<BufferInfo>, CommandError> {
    // Two lists in one non-yielding batch, line for line: tmux may print
    // a control character in a format as `_`, so no field separator
    // other than the line itself is safe for names and samples.
    let list = |format: &'static str| TmuxCommand::new("list-buffers").opt_format("-F", format);
    let r = conn
        .execute(
            Batch::new()
                .push(list("#{buffer_size} #{buffer_name}"))
                .push(list("#{buffer_sample}")),
        )
        .await?;
    let lines = |i: usize| {
        r.get(i)
            .map(|r| {
                r.body
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let samples = lines(1);
    Ok(lines(0)
        .into_iter()
        .zip(samples.into_iter().chain(std::iter::repeat(String::new())))
        .filter_map(|(head, sample)| {
            let (size, name) = head.split_once(' ')?;
            Some(BufferInfo {
                name: name.to_owned(),
                size: size.parse().ok()?,
                sample,
            })
        })
        .collect())
}

/// What a key typed after the prefix left behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrefixOutcome {
    /// A mode the binding put the pane in (`copy-mode`, `tree-mode`, ...)
    /// and that was cancelled again: tmux draws modes only for ordinary
    /// clients, so a Dreamer view would not show it.
    pub mode_cancelled: Option<String>,
}

/// Type the prefix and then `key` (a tmux key name) as a person at a
/// client looking at pane `target` would, so tmux looks both up in its
/// key tables and the user's own prefix bindings run there.
///
/// The typing is done by a short-lived control client of its own,
/// attached to the pane (which makes it the active pane, as focusing it
/// in Dreamer does) with `ignore-size,no-output`. A binding that acts on
/// its client (`detach-client`, a prompt, a menu) therefore touches only
/// that client, never the daemon's. The prefix is the server's `prefix`,
/// or `prefix2` when `prefix` is `None`; neither is changed.
pub async fn prefix_key(
    config: &crate::ConnectionConfig,
    target: TmuxPaneId,
    key: &str,
) -> Result<PrefixOutcome, String> {
    let mut config = config.clone();
    config.session = pane(target);
    config.new_session_args = None;
    config.attach_flags = Some("ignore-size,no-output".into());
    let (conn, mut events) = Connection::spawn(config)
        .await
        .map_err(|e| format!("cannot attach a client to %{}: {e}", target.0))?;
    // Notifications must keep flowing for responses to arrive.
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let result = type_prefix_key(&conn, key).await;
    drop(conn);
    drain.abort();
    result
}

async fn type_prefix_key(conn: &Connection, key: &str) -> Result<PrefixOutcome, String> {
    let r = conn
        .execute(
            Batch::new()
                .push(show_global("prefix", false))
                .push(show_global("prefix2", false)),
        )
        .await
        .map_err(|e| failure_message(&e))?;
    let prefix = r
        .iter()
        .map(first_line)
        .find(|p| !p.is_empty() && p != "None")
        .ok_or("tmux has no prefix key (prefix and prefix2 are None)")?;
    conn.execute(
        TmuxCommand::new("send-keys")
            .flag("-K")
            .arg(prefix)
            .arg(key.to_owned()),
    )
    .await
    .map_err(|e| failure_message(&e))?;
    // The binding was queued on this client after send-keys answered, so
    // this runs once it is done. A binding that detached the client ends
    // the connection instead.
    let state = match conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .format("#{pane_id} #{pane_in_mode} #{pane_mode}"),
        )
        .await
    {
        Ok(r) => r.first().map(first_line).unwrap_or_default(),
        Err(CommandError::Closed) => return Ok(PrefixOutcome::default()),
        Err(e) => return Err(failure_message(&e)),
    };
    let mut parts = state.split(' ');
    let (Some(pane_id), Some("1"), mode) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(PrefixOutcome::default());
    };
    conn.execute(
        TmuxCommand::new("copy-mode")
            .flag("-q")
            .opt("-t", pane_id.to_owned()),
    )
    .await
    .map_err(|e| failure_message(&e))?;
    Ok(PrefixOutcome {
        mode_cancelled: Some(
            mode.filter(|m| !m.is_empty())
                .unwrap_or("a mode")
                .to_owned(),
        ),
    })
}
