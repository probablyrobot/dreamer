//! Bound streams: the daemon streams a view tree's bound nodes name
//! (`bind`), followed by the renderer itself. An agent session's events
//! go into a [`Transcript`] and a tap's lines into a [`TapLines`], held
//! in [`Streams`] for the renderer to draw. The app's code never sees
//! them, and the tree does not carry them.
//!
//! Two followers use this:
//! - the app host, for its terminal rendering, on its own connection and
//!   so under the app's grant;
//! - the desktop, for its native rendering, on the user's connection with
//!   `for_pane` set: the daemon then checks the app's grant as well, so a
//!   desktop never shows an app a stream the app may not read.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use view_tree::Bind;
use view_tree::transcript::{self, Event, Transcript};

use crate::session::Ask;

/// Lines a tapped log keeps.
pub const TAP_LINES: usize = 2000;

/// A tapped pane's newest committed lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapLines {
    pub lines: VecDeque<String>,
    /// Lines the tap reported it missed, in all.
    pub missed: u64,
    pub revision: u64,
}

/// What the followed streams hold.
#[derive(Debug, Default)]
pub struct Data {
    pub sessions: HashMap<String, Transcript>,
    pub taps: HashMap<u32, TapLines>,
    /// Why a stream could not be followed, by bind.
    pub errors: HashMap<Bind, String>,
    /// Grows with every change.
    pub revision: u64,
}

impl Data {
    pub fn transcript(&self, session: &str) -> Option<&Transcript> {
        self.sessions.get(session)
    }

    /// Why the stream `b` is not shown, if it is not.
    pub fn error(&self, b: &Bind) -> Option<&str> {
        self.errors.get(b).map(String::as_str)
    }
}

/// The followed streams, shared between a follower and its renderer.
#[derive(Debug, Clone, Default)]
pub struct Streams(Arc<Mutex<Data>>);

impl Streams {
    /// Read what the streams hold.
    pub fn with<R>(&self, f: impl FnOnce(&Data) -> R) -> R {
        let d = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&d)
    }

    fn change<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let mut d = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut d)
    }

    /// The newest revision: compare to know whether to draw again.
    pub fn revision(&self) -> u64 {
        self.with(|d| d.revision)
    }

    /// Take one pushed notification. Whether it was a followed stream's.
    pub fn take(&self, n: &api::Notification) -> bool {
        match n {
            api::Notification::AgentSessionsEvents(b) => {
                if b.events.is_empty() {
                    return true;
                }
                self.change(|d| {
                    let t = d.sessions.entry(b.session.clone()).or_default();
                    let mut changed = false;
                    for e in &b.events {
                        if let Some(ev) = event(e) {
                            changed |= t.take(e.seq, ev);
                        }
                    }
                    if changed {
                        d.revision += 1;
                    }
                });
                true
            }
            api::Notification::TapsData(t) => {
                self.change(|d| {
                    let lines = d.taps.entry(t.pane).or_default();
                    if let Some(g) = &t.gap {
                        lines.missed += g.lines.unwrap_or(0);
                    }
                    lines.lines.extend(t.lines.iter().cloned());
                    while lines.lines.len() > TAP_LINES {
                        lines.lines.pop_front();
                    }
                    lines.revision += 1;
                    d.revision += 1;
                });
                true
            }
            _ => false,
        }
    }
}

/// A session event as the transcript takes it.
pub fn event(e: &api::AgentSessionEvent) -> Option<Event> {
    use api::agent_session_event::Kind as K;
    Some(match e.kind.as_ref()? {
        K::UserMessage(t) => Event::User(t.text.clone()),
        K::TextDelta(t) => Event::Text(t.text.clone()),
        K::ThinkingDelta(t) => Event::Thinking(t.text.clone()),
        K::ToolCall(t) => Event::Tool {
            id: t.id.clone(),
            name: t.name.clone(),
            status: match api::AgentToolStatus::try_from(t.status) {
                Ok(api::AgentToolStatus::Finished) => transcript::ToolStatus::Finished,
                Ok(api::AgentToolStatus::Failed) => transcript::ToolStatus::Failed,
                Ok(api::AgentToolStatus::Updated) => transcript::ToolStatus::Updated,
                _ => transcript::ToolStatus::Started,
            },
            input: t.input.clone(),
            output: t.output.clone(),
        },
        K::ApprovalRequest(a) => Event::Approval {
            id: a.id.clone(),
            title: a.title.clone(),
            message: a.message.clone(),
            options: a.options.clone(),
        },
        K::ApprovalAnswer(a) => Event::Answer {
            id: a.id.clone(),
            allowed: a.allowed,
            option: a.option.clone(),
            by: a.by.clone(),
        },
        K::Plan(p) => Event::Plan(
            p.entries
                .iter()
                .map(|s| transcript::Step {
                    content: s.content.clone(),
                    status: s.status.clone(),
                })
                .collect(),
        ),
        K::Usage(u) => Event::Usage(transcript::Usage {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_tokens,
            cache_write: u.cache_write_tokens,
            cost_micro_usd: u.cost_micro_usd,
        }),
        K::TurnEnd(t) => Event::TurnEnd(match api::AgentStopReason::try_from(t.stop_reason) {
            Ok(api::AgentStopReason::Cancelled) => transcript::Stop::Cancelled,
            Ok(api::AgentStopReason::Error) => transcript::Stop::Error,
            _ => transcript::Stop::EndTurn,
        }),
        K::Error(m) => Event::Error(m.message.clone()),
        K::Status(s) => Event::Status {
            state: match api::AgentSessionState::try_from(s.state) {
                Ok(api::AgentSessionState::Starting) => transcript::State::Starting,
                Ok(api::AgentSessionState::Idle) => transcript::State::Idle,
                Ok(api::AgentSessionState::Working) => transcript::State::Working,
                Ok(api::AgentSessionState::Waiting) => transcript::State::Waiting,
                Ok(api::AgentSessionState::Detached) => transcript::State::Detached,
                Ok(api::AgentSessionState::Closed) => transcript::State::Closed,
                _ => transcript::State::Unknown,
            },
            detail: s.detail.clone(),
        },
    })
}

/// Follows the streams a tree binds, on one connection.
#[derive(Debug, Default)]
pub struct Follower {
    /// Set when following for the app in this pane (a desktop).
    pub for_pane: Option<u32>,
    /// Streams followed, with their subscription.
    followed: HashMap<Bind, u64>,
}

impl Follower {
    pub fn new(for_pane: Option<u32>) -> Follower {
        Follower {
            for_pane,
            followed: HashMap::new(),
        }
    }

    /// The connection was lost: follow everything again on the next.
    pub fn reset(&mut self) {
        self.followed.clear();
    }

    /// Follow each stream `tree` binds that is not followed yet, and stop
    /// following those it no longer binds. A session is followed from the
    /// event after the last one held. `Err` means the connection is gone.
    pub async fn sync(
        &mut self,
        c: &client::Client,
        tree: &view_tree::Frame,
        streams: &Streams,
    ) -> Result<(), String> {
        let binds = tree.root.binds();
        let gone: Vec<Bind> = self
            .followed
            .keys()
            .filter(|b| !binds.contains(b))
            .cloned()
            .collect();
        for b in gone {
            if let Some(id) = self.followed.remove(&b) {
                let r = match b {
                    Bind::Session(_) => {
                        api::Request::TerminalUnsubscribe(api::Unsubscribe { subscription: id })
                    }
                    Bind::Tap(_) => {
                        api::Request::TapsUnsubscribe(api::Unsubscribe { subscription: id })
                    }
                };
                let _ = c.call(r).await;
            }
        }
        for b in binds {
            if self.followed.contains_key(&b) {
                continue;
            }
            let r = match &b {
                Bind::Session(s) => {
                    let after = streams.with(|d| d.sessions.get(s).map_or(0, |t| t.seq));
                    api::Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                        session: s.clone(),
                        after: Some(after),
                        follow: true,
                        limit: None,
                        for_pane: self.for_pane,
                    })
                }
                Bind::Tap(pane) => api::Request::TapsSubscribe(api::TapSubscribe {
                    level: api::TapLevel::Text as i32,
                    panes: vec![*pane],
                    every_pane: false,
                    interval_ms: Some(100),
                    for_pane: self.for_pane,
                }),
            };
            let answer = c.call(r).await;
            let result = match answer {
                Ok(api::Response::AgentSessionsEvents(e)) => Ok(e.subscription.unwrap_or(0)),
                Ok(api::Response::TapsSubscribe(t)) => Ok(t.id),
                Ok(_) => Err("an unexpected reply".to_owned()),
                Err(client::ClientError::Rpc(e)) => Err(e.message),
                Err(e) => return Err(e.to_string()),
            };
            // Followed or refused, it is not asked again until the tree
            // binds it anew or the connection is new.
            self.followed
                .insert(b.clone(), *result.as_ref().unwrap_or(&0));
            streams.change(|d| {
                let changed = match result {
                    Ok(_) => d.errors.remove(&b).is_some(),
                    Err(why) => d.errors.insert(b.clone(), why).is_none(),
                };
                if changed {
                    d.revision += 1;
                }
            });
        }
        Ok(())
    }
}

/// Do what a transcript or composer asked, on the app's connection: send
/// a prompt (references and attachments expanded), stop a turn, answer an
/// approval, or attach the selection. What happened, for the status line,
/// and whether the connection is gone.
pub async fn perform(
    app: &mut crate::App,
    c: &client::Client,
    streams: &Streams,
    ask: Ask,
) -> Option<String> {
    let call = |method: &'static str, params: Value| async move {
        crate::run::call(c, method, params).await
    };
    let (what, result) = match ask {
        Ask::Send {
            session,
            text,
            mut attached,
        } => {
            let mut notes = Vec::new();
            for r in crate::session::references(&text) {
                match reference(c, &r).await {
                    Ok(Some(a)) => attached.push(a),
                    Ok(None) => {}
                    Err(why) => notes.push(format!("{r}: {why}")),
                }
            }
            let prompt = crate::session::prompt(&text, &attached);
            let r = call(
                "agent.sessions.send",
                serde_json::json!({ "session": session, "text": prompt }),
            )
            .await;
            if !notes.is_empty() && r.is_ok() {
                app.status = Some(format!("sent; not attached: {}", notes.join("; ")));
                return None;
            }
            ("send", r)
        }
        Ask::Cancel { session } => (
            "stop",
            call(
                "agent.sessions.cancel",
                serde_json::json!({ "session": session }),
            )
            .await,
        ),
        Ask::Answer {
            session,
            allow,
            option,
        } => {
            let pending = streams.with(|d| {
                d.transcript(&session)
                    .and_then(|t| t.pending())
                    .map(|a| (a.id.clone(), a.options.clone()))
            });
            let Some((id, options)) = pending else {
                app.status = Some("no approval is waiting".into());
                return None;
            };
            let option = match (allow, option) {
                (true, Some(i)) => match options.get(i) {
                    Some(o) => Some(o.clone()),
                    // A yes or no question: its one way to allow.
                    None if i == 0 && options.is_empty() => None,
                    None => {
                        app.status = Some(format!("no option {}", i + 1));
                        return None;
                    }
                },
                (true, None) => options.first().cloned(),
                (false, _) => None,
            };
            let method = if allow {
                "agent.sessions.approve"
            } else {
                "agent.sessions.deny"
            };
            let r = call(
                method,
                serde_json::json!({ "session": session, "approval": id, "option": option }),
            )
            .await;
            (if allow { "allow" } else { "deny" }, r)
        }
        Ask::AttachSelection { node } => {
            let r = call("clipboard.read_tmux_buffer", serde_json::json!({})).await;
            match r {
                Ok(v) if v.get("found").and_then(Value::as_bool) == Some(true) => {
                    let text = v
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    app.attach(&node, "Selection", text);
                    app.status = None;
                    return None;
                }
                Ok(_) => {
                    app.status = Some("attach: nothing is selected".into());
                    return None;
                }
                Err(e) => ("attach the selection", Err(e)),
            }
        }
    };
    match result {
        Ok(_) => {
            app.status = None;
            None
        }
        Err((gone, why)) => {
            app.status = Some(format!("{what}: {why}"));
            gone.then_some(why)
        }
    }
}

/// A reference as an attachment: `@%N`, the pane's visible text; `@name`,
/// a collector's newest rows. `Ok(None)` when it names no collector (it
/// stays as typed).
async fn reference(c: &client::Client, r: &str) -> Result<Option<(String, String)>, String> {
    if let Some(pane) = crate::session::reference_pane(r) {
        // Reading the pane this app draws in, from inside it, waits on
        // its own drawing: refused rather than risked.
        if std::env::var("TMUX_PANE").is_ok_and(|p| p == format!("%{pane}")) {
            return Err("this is the app's own pane".into());
        }
        let v = crate::run::call(c, "terminal.read", serde_json::json!({ "pane": pane }))
            .await
            .map_err(|(_, e)| e)?;
        let lines: Vec<String> = v
            .get("lines")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(crate::model::text).collect())
            .unwrap_or_default();
        return Ok(Some((format!("Pane %{pane}"), lines.join("\n"))));
    }
    let name = r.trim_start_matches('@');
    let v = crate::run::call(
        c,
        "collect.query",
        serde_json::json!({ "name": name, "limit": 20 }),
    )
    .await;
    match v {
        Ok(v) => {
            let rows: Vec<String> = v
                .get("rows")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(crate::session::row_text).collect())
                .unwrap_or_default();
            Ok(Some((format!("Collector {name}"), rows.join("\n"))))
        }
        // Not a collector: the word stays as typed.
        Err((false, e)) if e.contains("no collector") || e.contains("not found") => Ok(None),
        Err((_, e)) => Err(e),
    }
}
