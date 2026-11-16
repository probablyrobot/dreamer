//! `dream agents sessions …`: agent sessions in the daemon, for scripts and
//! for trying a harness out.

use std::io::Write as _;

use api::agent_session_event::Kind as K;
use api::{Request, Response};
use prost::Message as _;

use crate::daemon::{DaemonArgs, call};
use crate::remote::session;

pub const USAGE: &str = "usage:
  dream agents sessions [--json]
  dream agents sessions create [--harness H] [--cwd DIR] [--model M] [--name N] [--json]
  dream agents sessions send ID TEXT [--wait]
  dream agents sessions events ID [--after N] [--follow] [--json]
  dream agents sessions approve|deny ID APPROVAL [--option O]
  dream agents sessions cancel|resume|close ID";

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

fn state_name(s: i32) -> &'static str {
    api::AgentSessionState::try_from(s).map_or("?", |s| s.name())
}

fn print_session(s: &api::AgentSession) {
    println!(
        "{} {} {} {}{}{}",
        s.id,
        state_name(s.state),
        s.harness,
        s.cwd,
        s.name
            .as_deref()
            .map(|n| format!(" \"{n}\""))
            .unwrap_or_default(),
        s.detail
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default()
    );
}

fn json<T: serde::Serialize>(v: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(v).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// One event as a line of text (text deltas run on without a newline).
fn show(e: &api::AgentSessionEvent, id: &str) {
    let mut out = std::io::stdout().lock();
    let _ = match &e.kind {
        Some(K::TextDelta(t)) => write!(out, "{}", t.text),
        Some(K::ThinkingDelta(t)) => write!(out, "\x1b[2m{}\x1b[0m", t.text),
        Some(K::UserMessage(t)) => writeln!(out, "\n> {}", t.text),
        Some(K::ToolCall(t)) => {
            let status = api::AgentToolStatus::try_from(t.status).map_or("?", |s| s.name());
            let detail = match (status, &t.input, &t.output) {
                ("started", Some(i), _) => format!(" {i}"),
                (_, _, Some(o)) => format!(": {}", o.lines().next().unwrap_or_default()),
                _ => String::new(),
            };
            writeln!(out, "\n[tool {} {} {status}]{detail}", t.name, t.id)
        }
        Some(K::ApprovalRequest(a)) => writeln!(
            out,
            "\n[approval {}] {}{}{}  (dream agents sessions approve|deny {id} {})",
            a.id,
            a.title,
            a.message
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default(),
            if a.options.is_empty() {
                String::new()
            } else {
                format!(" [{}]", a.options.join(", "))
            },
            a.id
        ),
        Some(K::ApprovalAnswer(a)) => writeln!(
            out,
            "[approval {} {} by {}]",
            a.id,
            if a.allowed { "allowed" } else { "denied" },
            a.by
        ),
        Some(K::Plan(p)) => {
            let _ = writeln!(out, "\n[plan]");
            for e in &p.entries {
                let _ = writeln!(out, "  {} {}", e.status, e.content);
            }
            Ok(())
        }
        Some(K::Usage(u)) => writeln!(
            out,
            "\n[usage in {} out {}{}]",
            u.input_tokens,
            u.output_tokens,
            u.cost_micro_usd
                .map(|c| format!(" ${:.6}", c as f64 / 1e6))
                .unwrap_or_default()
        ),
        Some(K::TurnEnd(t)) => writeln!(
            out,
            "[turn end: {}]",
            api::AgentStopReason::try_from(t.stop_reason).map_or("?", |s| s.name())
        ),
        Some(K::Error(m)) => writeln!(out, "\n[error] {}", m.message),
        Some(K::Status(s)) => writeln!(
            out,
            "[{}{}]",
            state_name(s.state),
            s.detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
        None => Ok(()),
    };
    let _ = out.flush();
}

fn session_reply(r: Response) -> Result<api::AgentSession, String> {
    match r {
        Response::AgentSessionsCreate(s)
        | Response::AgentSessionsSend(s)
        | Response::AgentSessionsCancel(s)
        | Response::AgentSessionsApprove(s)
        | Response::AgentSessionsDeny(s)
        | Response::AgentSessionsResume(s)
        | Response::AgentSessionsClose(s) => Ok(s),
        _ => Err("unexpected reply".into()),
    }
}

pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str);
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        None | Some("--json") => list(d, sub.is_some()),
        Some("create") => create(d, &rest),
        Some("send") => send(d, &rest),
        Some("events") => events(d, &rest),
        Some(verb @ ("approve" | "deny")) => answer(d, verb == "approve", &rest),
        Some(verb @ ("cancel" | "resume" | "close")) => {
            let [id] = rest.as_slice() else {
                return Err(USAGE.into());
            };
            let r = api::AgentSessionRef {
                session: id.clone(),
            };
            let request = match verb {
                "cancel" => Request::AgentSessionsCancel(r),
                "resume" => Request::AgentSessionsResume(r),
                _ => Request::AgentSessionsClose(r),
            };
            rt()?.block_on(async {
                let mut c = session(d).await?;
                print_session(&session_reply(call(&mut c, request).await?)?);
                Ok(())
            })
        }
        _ => Err(USAGE.into()),
    }
}

fn list(d: &DaemonArgs, as_json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let Response::AgentSessionsList(l) =
            call(&mut c, Request::AgentSessionsList(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        if as_json {
            return json(&l);
        }
        if l.sessions.is_empty() {
            println!("no agent sessions");
        }
        for s in &l.sessions {
            print_session(s);
        }
        Ok(())
    })
}

fn create(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let mut p = api::AgentSessionCreate::default();
    let mut as_json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| USAGE.to_owned());
        match a.as_str() {
            "--harness" => p.harness = Some(value()?),
            "--cwd" => {
                let dir = std::path::PathBuf::from(value()?);
                let dir =
                    std::fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                p.cwd = Some(dir.display().to_string());
            }
            "--model" => p.model = Some(value()?),
            "--name" => p.name = Some(value()?),
            "--json" => as_json = true,
            _ => return Err(USAGE.into()),
        }
    }
    if p.cwd.is_none() {
        p.cwd = std::env::current_dir()
            .ok()
            .map(|d| d.display().to_string());
    }
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let s = session_reply(call(&mut c, Request::AgentSessionsCreate(p)).await?)?;
        if as_json {
            return json(&s);
        }
        print_session(&s);
        Ok(())
    })
}

fn answer(d: &DaemonArgs, allow: bool, args: &[String]) -> Result<(), String> {
    let (id, approval, option) = match args {
        [id, approval] => (id, approval, None),
        [id, approval, flag, o] if flag == "--option" => (id, approval, Some(o.clone())),
        _ => return Err(USAGE.into()),
    };
    let p = api::AgentSessionAnswer {
        session: id.clone(),
        approval: approval.clone(),
        option,
    };
    let request = if allow {
        Request::AgentSessionsApprove(p)
    } else {
        Request::AgentSessionsDeny(p)
    };
    rt()?.block_on(async {
        let mut c = session(d).await?;
        print_session(&session_reply(call(&mut c, request).await?)?);
        Ok(())
    })
}

/// Follow `id` after `after` and print each event until `stop` says so
/// (or the session closes).
async fn follow(
    c: &mut client::Client,
    id: &str,
    after: u64,
    as_json: bool,
    stop: impl Fn(&api::AgentSessionEvent) -> bool,
) -> Result<(), String> {
    let mut pushed = c.pushed().ok_or("this connection already follows")?;
    let q = api::AgentSessionEventsQuery {
        session: id.to_owned(),
        after: Some(after),
        follow: true,
        limit: None,
    };
    let Response::AgentSessionsEvents(sub) = call(c, Request::AgentSessionsEvents(q)).await? else {
        return Err("unexpected reply".into());
    };
    let sub = sub.subscription.unwrap_or_default();
    while let Some(p) = pushed.recv().await {
        let client::Pushed::Event { topic, payload, .. } = p else {
            return Ok(());
        };
        if topic == api::topic::TERMINAL_END {
            let end = api::SubscriptionEnd::decode(payload.as_slice()).unwrap_or_default();
            if end.subscription == sub {
                return Ok(());
            }
            continue;
        }
        if topic != api::topic::AGENT_SESSIONS_EVENTS {
            continue;
        }
        let Ok(b) = api::AgentSessionEvents::decode(payload.as_slice()) else {
            continue;
        };
        for e in &b.events {
            if as_json {
                println!("{}", serde_json::to_string(e).map_err(|e| e.to_string())?);
            } else {
                show(e, id);
            }
            if stop(e) {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn send(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let (id, text, wait) = match args {
        [id, text] => (id, text, false),
        [id, text, w] if w == "--wait" => (id, text, true),
        _ => return Err(USAGE.into()),
    };
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let p = api::AgentSessionSend {
            session: id.clone(),
            text: text.clone(),
        };
        if !wait {
            print_session(&session_reply(
                call(&mut c, Request::AgentSessionsSend(p)).await?,
            )?);
            return Ok(());
        }
        // Where the transcript ends now: the turn's events come after.
        let Response::AgentSessionsList(l) =
            call(&mut c, Request::AgentSessionsList(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        let after = l
            .sessions
            .iter()
            .find(|s| s.id == *id)
            .map(|s| s.last_seq)
            .ok_or_else(|| format!("no agent session {id}"))?;
        session_reply(call(&mut c, Request::AgentSessionsSend(p)).await?)?;
        follow(&mut c, id, after, false, |e| {
            matches!(e.kind, Some(K::TurnEnd(_)))
                || matches!(&e.kind, Some(K::Status(s))
                    if s.state == api::AgentSessionState::Detached as i32)
        })
        .await
    })
}

fn events(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let mut id = None;
    let (mut after, mut follow_on, mut as_json) = (0u64, false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--after" => {
                after = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| USAGE.to_owned())?;
            }
            "--follow" => follow_on = true,
            "--json" => as_json = true,
            s if id.is_none() && !s.starts_with('-') => id = Some(s.to_owned()),
            _ => return Err(USAGE.into()),
        }
    }
    let id = id.ok_or_else(|| USAGE.to_owned())?;
    rt()?.block_on(async {
        let mut c = session(d).await?;
        if follow_on {
            return follow(&mut c, &id, after, as_json, |_| false).await;
        }
        loop {
            let q = api::AgentSessionEventsQuery {
                session: id.clone(),
                after: Some(after),
                follow: false,
                limit: None,
            };
            let Response::AgentSessionsEvents(b) =
                call(&mut c, Request::AgentSessionsEvents(q)).await?
            else {
                return Err("unexpected reply".into());
            };
            for e in &b.events {
                if as_json {
                    println!("{}", serde_json::to_string(e).map_err(|e| e.to_string())?);
                } else {
                    show(e, &id);
                }
                after = e.seq;
            }
            if !b.more {
                if !as_json {
                    println!();
                }
                return Ok(());
            }
        }
    })
}
