//! Apply a workspace [`Plan`] to a tmux server through the typed connection.
//!
//! Every value from the workspace file travels as an encoded argument; no
//! command string is built from user text. Panes are tracked by the tmux
//! pane IDs that creation commands report, never by index.

use std::collections::BTreeMap;

use domain::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use workspace_config::{Direction, PanePlan, PaneRef, Plan, SizeArg, Start, WindowPlan};

use crate::command::{Batch, TmuxCommand};
use crate::connection::{CommandError, Connection};

/// What was created, for the caller to select or report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedWorkspace {
    pub session: TmuxSessionId,
    /// Windows in file order, each with its panes in file order.
    pub windows: Vec<(TmuxWindowId, Vec<TmuxPaneId>)>,
}

#[derive(Debug)]
pub enum ApplyError {
    /// A session with the plan's name already exists; nothing was changed.
    SessionExists(String),
    Command(CommandError),
    Unexpected(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionExists(s) => write!(f, "session `{s}` already exists"),
            Self::Command(e) => write!(f, "{e}"),
            Self::Unexpected(m) => write!(f, "unexpected tmux reply: {m}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<CommandError> for ApplyError {
    fn from(e: CommandError) -> Self {
        Self::Command(e)
    }
}

/// Program for a long-lived placeholder while `remain-on-exit` is set.
const HOLD: [&str; 2] = ["sleep", "2147483647"];

/// `/bin/sh -c` script that runs a kept pane's program (its arguments) and
/// exits with the program's status only once tmux has read everything the
/// program wrote.
///
/// tmux decides whether an exited pane still has output to read by asking
/// the pty how many bytes are waiting. On Linux a pty write reaches that
/// count through a kernel worker, so when tmux handles the exit first it
/// closes the pane with the last output still in flight: a short program in
/// a kept pane can leave "Pane is dead" over a blank screen (#236). A cursor
/// position query written after the program ends is answered only after
/// tmux has parsed all output before it, so waiting for the reply (at most
/// two seconds) closes that window.
///
/// Job control (`set -m`) runs the program as the terminal's foreground
/// process group, so `pane_current_command` still names it and Ctrl-C
/// reaches only the program. Ctrl-Z stays without effect, as it was when
/// tmux continued a stopped pane process at once: tmux would not continue a
/// job of this script, and a stopped job would end the script and the pane.
/// So the terminal has no suspend key, and the program starts with `TSTP`
/// ignored in case it sets one. The ignore is set in the job itself because
/// some shells (bash 3.2, macOS `/bin/sh`) reset it for their jobs.
const DRAIN: &str = r#"stty susp undef 2>/dev/null; set -m
/bin/sh -c 'trap "" TSTP; exec "$@"' sh "$@"; s=$?; set +m
if stty -echo -icanon min 0 time 20 2>/dev/null; then
  printf '\033[6n'
  while r=$(dd bs=32 count=1 2>/dev/null) && [ -n "$r" ]; do
    case $r in *R*) break ;; esac
  done
fi
exit $s"#;

/// The argv for a kept pane's program: `process` under [`DRAIN`].
fn drained(process: Vec<String>) -> Vec<String> {
    let mut argv = vec!["/bin/sh".into(), "-c".into(), DRAIN.into(), "sh".into()];
    argv.extend(process);
    argv
}

fn first_line(r: &tmux_protocol::Response) -> String {
    r.body
        .first()
        .map(|l| String::from_utf8_lossy(l).trim().to_owned())
        .unwrap_or_default()
}

fn parse_id<T>(text: &str, sigil: char, make: fn(u32) -> T) -> Result<T, ApplyError> {
    text.strip_prefix(sigil)
        .and_then(|d| d.parse().ok())
        .map(make)
        .ok_or_else(|| ApplyError::Unexpected(format!("expected an id, got `{text}`")))
}

/// argv for a pane's initial process, or `None` for the default shell.
/// tmux runs a single argument through the shell, so one-word programs are
/// wrapped in `/usr/bin/env` to keep `exec` shell-free.
fn argv(start: &Start) -> Option<Vec<String>> {
    match start {
        Start::Shell | Start::Run(_) => None,
        Start::Exec(argv) if argv.len() == 1 => Some(vec!["/usr/bin/env".into(), argv[0].clone()]),
        Start::Exec(argv) => Some(argv.clone()),
        Start::ShellCommand(cmd) => Some(vec!["/bin/sh".into(), "-c".into(), cmd.clone()]),
    }
}

/// `-c cwd -e K=V ...` plus the process (or the hold placeholder for panes
/// that must survive a fast exit).
fn with_process(mut cmd: TmuxCommand, pane: &PanePlan, env: &[(String, String)]) -> TmuxCommand {
    cmd = cmd.opt("-c", pane.cwd.to_string_lossy());
    for (k, v) in merged_env(env, &pane.env) {
        cmd = cmd.opt("-e", format!("{k}={v}"));
    }
    let process = if pane.keep && argv(&pane.start).is_some() {
        Some(HOLD.iter().map(|s| (*s).to_owned()).collect())
    } else {
        argv(&pane.start)
    };
    for a in process.unwrap_or_default() {
        cmd = cmd.arg(a);
    }
    cmd
}

/// Process arguments (after `tmux -C new-session`) that create the plan's
/// session with its first window and first pane exactly as `apply` would.
/// Use with [`crate::ConnectionConfig::new_session_args`] when no tmux
/// server is running yet, then call [`apply_adopted`].
pub fn new_session_args(plan: &Plan) -> Vec<String> {
    let w = &plan.windows[0];
    let pane = &w.panes[0];
    let mut args = vec!["-s".to_owned(), plan.session.clone()];
    if let Some(name) = &w.name {
        args.push("-n".into());
        args.push(name.replace('#', "##"));
    }
    args.push("-c".into());
    args.push(pane.cwd.to_string_lossy().into_owned());
    for (k, v) in merged_env(&plan.env, &pane.env) {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    let process = if pane.keep && argv(&pane.start).is_some() {
        Some(HOLD.iter().map(|s| (*s).to_owned()).collect())
    } else {
        argv(&pane.start)
    };
    if let Some(process) = process {
        args.push("--".into());
        args.extend(process);
    }
    args
}

fn merged_env<'a>(
    workspace: &'a [(String, String)],
    pane: &'a [(String, String)],
) -> BTreeMap<&'a str, &'a str> {
    let mut merged = BTreeMap::new();
    for (k, v) in workspace.iter().chain(pane.iter()) {
        merged.insert(k.as_str(), v.as_str());
    }
    merged
}

/// Finish a workspace whose session was created by the connection itself
/// (see [`new_session_args`]): adopt its first window, then create the rest.
pub async fn apply_adopted(conn: &Connection, plan: &Plan) -> Result<AppliedWorkspace, ApplyError> {
    let reply = conn
        .execute(
            TmuxCommand::new("display-message")
                .flag("-p")
                .format("#{session_id} #{window_id} #{pane_id} #{session_name}"),
        )
        .await?;
    let line = first_line(&reply[0]);
    let mut parts = line.splitn(4, ' ');
    let (Some(s), Some(win), Some(pane), Some(name)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ApplyError::Unexpected(line));
    };
    if name != plan.session {
        return Err(ApplyError::Unexpected(format!(
            "attached to `{name}`, expected `{}`",
            plan.session
        )));
    }
    let session = parse_id(s, '$', TmuxSessionId)?;
    let window = parse_id(win, '@', TmuxWindowId)?;
    let pane = parse_id(pane, '%', TmuxPaneId)?;
    let first = build_window(conn, plan, &plan.windows[0], pane).await?;
    create_rest(conn, plan, session, vec![(window, first)]).await
}

/// Create the plan's session and everything in it.
pub async fn apply(conn: &Connection, plan: &Plan) -> Result<AppliedWorkspace, ApplyError> {
    let exists = conn
        .execute(TmuxCommand::new("has-session").opt("-t", format!("={}", plan.session)))
        .await;
    if exists.is_ok() {
        return Err(ApplyError::SessionExists(plan.session.clone()));
    }

    // The first window comes with the session.
    let w = &plan.windows[0];
    let mut cmd = TmuxCommand::new("new-session")
        .flag("-d")
        .opt("-s", &plan.session)
        .flag("-P")
        .opt_format("-F", "#{session_id} #{window_id} #{pane_id}");
    if let Some(name) = &w.name {
        cmd = cmd.opt_literal_for_format("-n", name);
    }
    let (session, window, pane) = created(conn, with_process(cmd, &w.panes[0], &plan.env)).await?;
    let first = build_window(conn, plan, w, pane).await?;
    create_rest(conn, plan, session, vec![(window, first)]).await
}

/// Run a creation command and read the ids it prints.
async fn created(
    conn: &Connection,
    cmd: TmuxCommand,
) -> Result<(TmuxSessionId, TmuxWindowId, TmuxPaneId), ApplyError> {
    let reply = conn.execute(cmd).await?;
    let line = first_line(&reply[0]);
    let mut ids = line.split(' ');
    let (Some(s), Some(win), Some(pane)) = (ids.next(), ids.next(), ids.next()) else {
        return Err(ApplyError::Unexpected(line));
    };
    Ok((
        parse_id(s, '$', TmuxSessionId)?,
        parse_id(win, '@', TmuxWindowId)?,
        parse_id(pane, '%', TmuxPaneId)?,
    ))
}

/// Create windows after the first, then focus.
async fn create_rest(
    conn: &Connection,
    plan: &Plan,
    session: TmuxSessionId,
    mut windows: Vec<(TmuxWindowId, Vec<TmuxPaneId>)>,
) -> Result<AppliedWorkspace, ApplyError> {
    for w in plan.windows.iter().skip(1) {
        let mut cmd = TmuxCommand::new("new-window")
            .flag("-d")
            .opt("-t", format!("{session}:"))
            .flag("-P")
            .opt_format("-F", "#{session_id} #{window_id} #{pane_id}");
        if let Some(name) = &w.name {
            cmd = cmd.opt_literal_for_format("-n", name);
        }
        let (_, window, pane) = created(conn, with_process(cmd, &w.panes[0], &plan.env)).await?;
        let panes = build_window(conn, plan, w, pane).await?;
        windows.push((window, panes));
    }
    let focus = plan.focus_window.unwrap_or(0);
    conn.execute(TmuxCommand::new("select-window").opt("-t", windows[focus].0.to_string()))
        .await?;
    Ok(AppliedWorkspace { session, windows })
}

async fn build_window(
    conn: &Connection,
    plan: &Plan,
    w: &WindowPlan,
    first: TmuxPaneId,
) -> Result<Vec<TmuxPaneId>, ApplyError> {
    let mut ids: BTreeMap<PaneRef, TmuxPaneId> = BTreeMap::new();
    ids.insert(PaneRef(0), first);
    for step in &w.splits {
        let target = ids[&step.target];
        let mut cmd = TmuxCommand::new("split-window")
            .flag("-d")
            .flag("-b")
            .flag(match step.direction {
                Direction::Horizontal => "-h",
                Direction::Vertical => "-v",
            })
            .flag("-P")
            .opt_format("-F", "#{pane_id}")
            .opt("-t", target.to_string());
        cmd = match step.size {
            Some(SizeArg::Cells(c)) => cmd.opt("-l", c.to_string()),
            Some(SizeArg::Percent(p)) => cmd.opt("-l", format!("{p}%")),
            None => cmd,
        };
        let reply = conn
            .execute(with_process(cmd, &w.panes[step.new.0], &plan.env))
            .await?;
        ids.insert(step.new, parse_id(&first_line(&reply[0]), '%', TmuxPaneId)?);
    }

    // Per-pane settings, then processes that had to wait for them.
    let mut setup = Batch::new();
    let mut after = Batch::new();
    for (i, pane) in w.panes.iter().enumerate() {
        let id = ids[&PaneRef(i)].to_string();
        if let Some(title) = &pane.title {
            setup = setup.push(
                TmuxCommand::new("select-pane")
                    .opt("-t", &id)
                    .opt("-T", title),
            );
        }
        if pane.keep {
            setup = setup.push(
                TmuxCommand::new("set-option")
                    .flag("-p")
                    .opt("-t", &id)
                    .arg("remain-on-exit")
                    .arg("on"),
            );
            if let Some(process) = argv(&pane.start) {
                let mut respawn = TmuxCommand::new("respawn-pane")
                    .flag("-k")
                    .opt("-t", &id)
                    .opt("-c", pane.cwd.to_string_lossy());
                for (k, v) in merged_env(&plan.env, &pane.env) {
                    respawn = respawn.opt("-e", format!("{k}={v}"));
                }
                for a in drained(process) {
                    respawn = respawn.arg(a);
                }
                after = after.push(respawn);
            }
        }
        if let Start::Run(text) = &pane.start {
            after = after
                .push(
                    TmuxCommand::new("send-keys")
                        .opt("-t", &id)
                        .flag("-l")
                        .arg(text),
                )
                .push(TmuxCommand::new("send-keys").opt("-t", &id).arg("Enter"));
        }
    }
    if let Some(arrange) = w.arrange {
        after = after.push(
            TmuxCommand::new("select-layout")
                .opt("-t", ids[&PaneRef(0)].to_string())
                .arg(arrange.tmux_name()),
        );
    }
    let focus = w.focus_pane.unwrap_or(PaneRef(0));
    after = after.push(TmuxCommand::new("select-pane").opt("-t", ids[&focus].to_string()));
    if !setup.is_empty() {
        conn.execute(setup).await?;
    }
    if !after.is_empty() {
        conn.execute(after).await?;
    }
    Ok(w.declared.iter().map(|r| ids[r]).collect())
}
