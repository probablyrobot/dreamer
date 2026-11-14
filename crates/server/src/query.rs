//! `terminal.query` (spec 12.8): which panes' newest lines match, here and
//! on connected hosts, within fixed bounds.
//!
//! Lines come from the canonical mirrors through the same cheap tail read
//! taps use, a few panes at a time, so a query never holds up input or
//! output. Every bound (lines per pane, matches, time) is enforced, and a
//! truncated answer says what was cut.

use std::sync::Arc;
use std::time::Duration;

use api::{QueryMatch, QueryResult, RpcError, code};
use matcher::{Matcher, Spec};
use prost::Message as _;

use crate::service::State;

/// Panes read at once.
const CONCURRENT: usize = 8;

fn matcher(
    pattern: &str,
    literals: &[String],
    literal: bool,
    ignore_case: bool,
) -> Result<Matcher, RpcError> {
    Matcher::new(&Spec {
        pattern: pattern.to_owned(),
        literals: literals.to_vec(),
        literal,
        ignore_case,
    })
    .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))
}

pub async fn query(
    state: &Arc<State>,
    host: Option<mirror::MirrorHost>,
    p: api::TerminalQuery,
) -> Result<QueryResult, RpcError> {
    let m = matcher(&p.pattern, &p.literals, p.literal, p.ignore_case)?;
    let title = p
        .title
        .as_deref()
        .map(|t| matcher(t, &[], false, false))
        .transpose()?;
    let lines = p.lines.unwrap_or(200).clamp(1, 2000) as usize;
    let max = p.max_matches.unwrap_or(100).clamp(1, 1000) as usize;
    let timeout = Duration::from_millis(u64::from(p.timeout_ms.unwrap_or(2000).clamp(1, 10_000)));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut out = QueryResult::default();

    if let Some(host) = host {
        let panes: Vec<u32> = if p.panes.is_empty() {
            host.status()
                .await
                .into_iter()
                .filter(|s| s.state == mirror::MirrorState::Live)
                .map(|s| s.pane.0)
                .collect()
        } else {
            p.panes.clone()
        };
        search_here(
            &host,
            &panes,
            &m,
            title.as_ref(),
            lines,
            max,
            deadline,
            &mut out,
        )
        .await;
    } else if !p.panes.is_empty() {
        return Err(RpcError::new(code::UNAVAILABLE, "tmux is not connected"));
    }

    let hosts: Vec<String> = if p.hosts.iter().any(|h| h == "*") {
        state
            .hosts
            .list()
            .await
            .hosts
            .into_iter()
            .filter(|h| h.state == api::LinkState::Live as i32)
            .map(|h| h.name)
            .collect()
    } else {
        p.hosts.clone()
    };
    for name in hosts {
        let left = max.saturating_sub(out.matches.len());
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left == 0 || remaining.is_zero() {
            out.truncated = true;
            out.notes
                .push(format!("host {name}: not searched (a bound was reached)"));
            continue;
        }
        let ask = api::TerminalQuery {
            panes: Vec::new(),
            hosts: Vec::new(),
            max_matches: Some(left as u32),
            timeout_ms: Some(remaining.as_millis().max(1) as u32),
            ..p.clone()
        };
        let call = state.hosts.call(api::HostCall {
            name: name.clone(),
            method: api::method::TERMINAL_QUERY.into(),
            payload: ask.encode_to_vec(),
        });
        match tokio::time::timeout_at(deadline + Duration::from_millis(500), call).await {
            Ok(Ok(reply)) => match QueryResult::decode(reply.payload.as_slice()) {
                Ok(r) => {
                    out.panes_searched += r.panes_searched;
                    out.truncated |= r.truncated;
                    out.notes
                        .extend(r.notes.into_iter().map(|n| format!("host {name}: {n}")));
                    out.matches.extend(r.matches.into_iter().map(|mut x| {
                        x.host = Some(name.clone());
                        x
                    }));
                }
                Err(e) => out.notes.push(format!("host {name}: {e}")),
            },
            Ok(Err(e)) => out.notes.push(format!("host {name}: {}", e.message)),
            Err(_) => {
                out.truncated = true;
                out.notes.push(format!("host {name}: no answer in time"));
            }
        }
    }
    if out.matches.len() > max {
        out.matches.truncate(max);
        out.truncated = true;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn search_here(
    host: &mirror::MirrorHost,
    panes: &[u32],
    m: &Matcher,
    title: Option<&Matcher>,
    lines: usize,
    max: usize,
    deadline: tokio::time::Instant,
    out: &mut QueryResult,
) {
    let mut queue = panes.iter().copied();
    let mut set = tokio::task::JoinSet::new();
    let spawn = |set: &mut tokio::task::JoinSet<_>, pane: u32| {
        let host = host.clone();
        set.spawn(async move {
            // The newest `lines` lines: as much history as the screen leaves.
            let r = host.tail(domain::TmuxPaneId(pane), lines, None).await;
            (pane, r)
        });
    };
    for pane in queue.by_ref().take(CONCURRENT) {
        spawn(&mut set, pane);
    }
    let mut unsearched = 0usize;
    loop {
        match tokio::time::timeout_at(deadline, set.join_next()).await {
            Ok(Some(Ok((pane, r)))) => {
                if let Some(next) = queue.next() {
                    spawn(&mut set, next);
                }
                let Some(Ok(Some(t))) = r else {
                    out.notes.push(format!("pane %{pane}: no live mirror"));
                    continue;
                };
                if title.is_some_and(|tm| !t.title.as_deref().is_some_and(|x| tm.is_match(x))) {
                    continue;
                }
                out.panes_searched += 1;
                let all: Vec<&String> = t.history.iter().chain(t.screen.iter()).collect();
                let window = &all[all.len().saturating_sub(lines)..];
                for (i, text) in window.iter().enumerate() {
                    if let Some(f) = m.find(text) {
                        if out.matches.len() >= max {
                            out.truncated = true;
                            if !out.notes.iter().any(|n| n.starts_with("max_matches")) {
                                out.notes.push(format!("max_matches: stopped at {max}"));
                            }
                            break;
                        }
                        out.matches.push(QueryMatch {
                            host: None,
                            pane,
                            line: i as u32,
                            text: (*text).clone(),
                            match_start: f.start as u32,
                            match_end: f.end as u32,
                        });
                    }
                }
            }
            Ok(Some(Err(_))) => {}
            Ok(None) => break,
            Err(_) => {
                unsearched = set.len() + queue.len();
                set.abort_all();
                break;
            }
        }
    }
    if unsearched > 0 {
        out.truncated = true;
        out.notes
            .push(format!("timeout: {unsearched} panes not searched in time"));
    }
}
