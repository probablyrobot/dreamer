//! Terminal tools for agents: `terminal.list`, `terminal.watch`, and
//! `terminal.run`.
//!
//! They read the canonical mirrors the way taps do, at their own pace, so
//! a watch or a run never holds up a pane's input or output. Every wait is
//! bounded by a timeout.

use std::sync::Arc;
use std::time::Duration;

use api::{RpcError, WatchOutcome, code};
use matcher::{Matcher, Spec};
use tokio::time::Instant;

use crate::service::State;
use crate::taps::{TextState, WINDOW};

/// How often a watch or a run looks at the pane.
const LOOK: Duration = Duration::from_millis(50);
/// Watches end by this at the latest.
const MAX_WAIT_MS: u32 = 600_000;

fn unavailable(why: &str) -> RpcError {
    RpcError::new(code::UNAVAILABLE, why)
}

/// The caller's connection, for ownership.
fn caller_conn() -> Option<u64> {
    crate::leases::caller().map(|c| c.conn)
}

pub async fn list(
    state: &Arc<State>,
    host: Option<mirror::MirrorHost>,
) -> Result<api::TerminalList, RpcError> {
    let top = {
        let guard = state.endpoint().await?;
        let ep = guard
            .as_ref()
            .ok_or_else(|| unavailable("tmux is not connected"))?;
        workspace_host::topology(&ep.name, &ep.tracker)
    };
    let mirrors = match host {
        Some(h) => h.status().await,
        None => Vec::new(),
    };
    let conn = caller_conn();
    let mut terminals = Vec::with_capacity(top.panes.len());
    for p in &top.panes {
        let window = top.windows.iter().find(|w| w.id == p.window);
        let session = window
            .and_then(|w| w.sessions.first())
            .and_then(|s| top.sessions.iter().find(|x| x.id == *s));
        let m = mirrors.iter().find(|m| m.pane.0 == p.id);
        let live = m.and_then(|m| m.live.as_ref());
        let tier = match m {
            Some(m) if m.state == mirror::MirrorState::Cold => api::Tier::Cold,
            _ => match live.map(|l| l.tier) {
                Some(mirror::Tier::Hot) => api::Tier::Hot,
                Some(mirror::Tier::Warm) => api::Tier::Warm,
                None => api::Tier::Unspecified,
            },
        };
        terminals.push(api::TerminalInfo {
            pane: p.id,
            session: session.map_or(0, |s| s.id),
            session_name: session.map(|s| s.name.clone()).unwrap_or_default(),
            window: p.window,
            window_name: window.map(|w| w.name.clone()).unwrap_or_default(),
            window_index: window.map_or(0, |w| w.index),
            command: p.current_command.clone(),
            cwd: live.and_then(|l| l.cwd.clone()),
            title: p.title.clone(),
            cols: p.width,
            rows: p.height,
            active: p.active,
            dead: p.dead,
            tier: tier as i32,
            quiet_ms: live.map(|l| l.quiet.as_millis() as u64),
            owned: conn.is_some_and(|c| state.approvals.owns(c, p.id)),
        });
    }
    Ok(api::TerminalList { terminals })
}

fn matcher(pattern: &str, literal: bool, ignore_case: bool) -> Result<Matcher, RpcError> {
    Matcher::new(&Spec {
        pattern: pattern.to_owned(),
        literals: Vec::new(),
        literal,
        ignore_case,
    })
    .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))
}

/// The job that runs in `pane`, if any.
fn job_in(pane: u32) -> Option<api::JobRecord> {
    crate::jobs::list()
        .jobs
        .into_iter()
        .rev()
        .find(|j| j.pane == pane)
}

/// Whether the pane's program has ended: a job's record, else tmux's
/// `pane_dead`. `None` while it runs; `Some(None)` when the pane is gone.
fn ended(state: &State, pane: u32, job: Option<&str>) -> Option<Option<api::JobRecord>> {
    if let Some(id) = job
        && let Ok(j) = crate::jobs::get(id)
        && j.state != api::JobState::Running as i32
    {
        return Some(Some(j));
    }
    let top = state.topology.borrow().clone()?;
    let topology = top.topology.as_ref()?;
    match topology.panes.iter().find(|p| p.id == pane) {
        Some(p) if p.dead => Some(job.and_then(|id| crate::jobs::get(id).ok())),
        Some(_) => None,
        None => Some(None),
    }
}

pub async fn watch(
    state: &Arc<State>,
    host: mirror::MirrorHost,
    p: api::TerminalWatch,
) -> Result<api::WatchResult, RpcError> {
    let m = p
        .pattern
        .as_deref()
        .map(|t| matcher(t, p.literal, p.ignore_case))
        .transpose()?;
    let quiet = p
        .quiet_ms
        .map(|q| Duration::from_millis(u64::from(q.max(50))));
    if m.is_none() && quiet.is_none() && !p.exit {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "watch for a pattern, quiet_ms, or exit",
        ));
    }
    let timeout = Duration::from_millis(u64::from(
        p.timeout_ms.unwrap_or(30_000).clamp(1, MAX_WAIT_MS),
    ));
    let pane = domain::TmuxPaneId(p.pane);
    if !host.wait_live(&[pane], Duration::from_secs(3)).await {
        return Err(RpcError::new(
            code::NOT_FOUND,
            format!("pane %{} has no live mirror", p.pane),
        ));
    }
    let started = Instant::now();
    let deadline = started + timeout;
    let result = |outcome: WatchOutcome| api::WatchResult {
        outcome: outcome as i32,
        elapsed_ms: started.elapsed().as_millis() as u64,
        ..api::WatchResult::default()
    };
    let job = p.exit.then(|| job_in(p.pane)).flatten().map(|j| j.id);
    let first = match host.tail(pane, WINDOW, None).await {
        Some(Ok(Some(t))) => t,
        _ => return Ok(result(WatchOutcome::Closed)),
    };
    // Quiet counts from the pane's last output, which may be before the
    // watch started.
    let already_quiet = host
        .status()
        .await
        .into_iter()
        .find(|s| s.pane == pane)
        .and_then(|s| s.live.map(|l| l.quiet))
        .unwrap_or_default();
    let mut last_output = started.checked_sub(already_quiet).unwrap_or(started);
    let mut after = first.sequence;
    let cursor_line = |t: &mirror::TailResult| {
        t.screen
            .get(usize::from(t.cursor_row))
            .cloned()
            .unwrap_or_default()
    };
    let mut cursor = cursor_line(&first);
    let mut text = TextState::new(&first);
    loop {
        if p.exit
            && let Some(end) = ended(state, p.pane, job.as_deref())
        {
            let mut r = result(match end {
                None if job.is_none() => WatchOutcome::Closed,
                _ => WatchOutcome::Exited,
            });
            if let Some(j) = end {
                r.exit_code = j.exit_code;
                r.signal = j.signal;
            }
            return Ok(r);
        }
        match host.tail(pane, WINDOW, Some(after)).await {
            Some(Ok(Some(t))) => {
                after = t.sequence;
                last_output = Instant::now();
                if let Some(m) = &m {
                    let step = text.step(&t);
                    let now_cursor = cursor_line(&t);
                    // New committed lines, changed rows of an alternate
                    // screen, then the line being written (a prompt).
                    let mut candidates: Vec<String> = step.lines;
                    candidates.extend(step.screen.into_iter().map(|(_, row)| row));
                    if now_cursor != cursor && !t.alt_screen {
                        candidates.push(now_cursor.clone());
                    }
                    cursor = now_cursor;
                    for line in candidates {
                        if let Some(f) = m.find(&line) {
                            let mut r = result(WatchOutcome::Matched);
                            r.match_start = f.start as u32;
                            r.match_end = f.end as u32;
                            r.text = Some(line);
                            return Ok(r);
                        }
                    }
                }
            }
            Some(Ok(None)) => {}
            Some(Err(_)) | None => {
                // Being rebuilt, or gone.
                if !host.status().await.iter().any(|s| s.pane == pane) {
                    return Ok(result(if p.exit && job.is_some() {
                        WatchOutcome::Exited
                    } else {
                        WatchOutcome::Closed
                    }));
                }
            }
        }
        if let Some(q) = quiet
            && last_output.elapsed() >= q
        {
            return Ok(result(WatchOutcome::Quiet));
        }
        if Instant::now() >= deadline {
            return Ok(result(WatchOutcome::Timeout));
        }
        tokio::time::sleep(LOOK.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

/// The text the job wrapper prints after its program ends.
fn wrapper_line(line: &str) -> bool {
    line.starts_with("[job ") && line.ends_with(']')
}

pub async fn run(
    state: &Arc<State>,
    host: mirror::MirrorHost,
    p: api::TerminalRun,
) -> Result<api::RunResult, RpcError> {
    let timeout = Duration::from_millis(u64::from(
        p.timeout_ms.unwrap_or(60_000).clamp(1, MAX_WAIT_MS),
    ));
    let lines = p.lines.unwrap_or(200).clamp(1, 2000) as usize;
    let split = p.split.map(|pane| {
        let d = if p.direction == api::SplitDirection::Horizontal as i32 {
            tmux_backend::ops::Split::Horizontal
        } else {
            tmux_backend::ops::Split::Vertical
        };
        (pane, d)
    });
    let start = api::JobStart {
        argv: p.argv,
        cwd: p.cwd,
        env: p.env,
        name: p.name,
        session: p.session,
    };
    let job = crate::jobs::start_at(state, start, split).await?;
    if let Some(conn) = caller_conn() {
        state.approvals.created(conn, job.pane);
    }
    let deadline = Instant::now() + timeout;
    let mut record = job;
    while record.state == api::JobState::Running as i32 && Instant::now() < deadline {
        tokio::time::sleep(LOOK).await;
        record = crate::jobs::get(&record.id)?;
    }
    let finished = record.state != api::JobState::Running as i32;
    let pane = domain::TmuxPaneId(record.pane);
    // The wrapper's last line follows everything the program wrote: wait
    // for it, so the output is complete. It usually lands at once; on a
    // loaded or emulated machine the mirror can trail the exit by seconds.
    let read = |host: mirror::MirrorHost| async move {
        match host.read(pane, lines).await {
            Some(Ok(r)) => Some(r.lines),
            _ => None,
        }
    };
    let mut text = read(host.clone()).await.unwrap_or_default();
    if finished {
        let settle = Instant::now() + Duration::from_secs(10);
        while !text.iter().any(|l| wrapper_line(l)) && Instant::now() < settle {
            tokio::time::sleep(LOOK).await;
            text = read(host.clone()).await.unwrap_or_default();
        }
        if let Some(i) = text.iter().rposition(|l| wrapper_line(l)) {
            text.truncate(i);
        }
    }
    while text.last().is_some_and(|l| l.trim().is_empty()) {
        text.pop();
    }
    let truncated = text.len() > lines;
    if truncated {
        text.drain(..text.len() - lines);
    }
    Ok(api::RunResult {
        job: Some(record),
        finished,
        output: text,
        truncated,
    })
}
