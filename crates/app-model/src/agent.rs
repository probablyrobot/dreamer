//! Agentic panes (spec 13.4): an app package bound to a provider and a
//! grant. The pane takes a prompt, runs it on the provider with Dreamer's
//! tools (`dream mcp`, narrowed to the package's `[agent.tools]`), and
//! keeps what happened inspectable: the prompt, the model's text, every
//! tool call and its result, and the outcome.
//!
//! Context is retrieved on demand by the model's tool calls, bounded by
//! the tools themselves; nothing streams other terminals to a model. With
//! no provider ready, an inspector answers in a scripted mode (a plain-text
//! search with `terminal.query`); the other kinds say what they need.

use std::path::{Path, PathBuf};
use std::time::Duration;

use providers::{Event, Kind, Request, State, Tools};
use serde_json::{Value, json};

use crate::manifest::{AgentKind, AgentSpec};
use crate::model::{App, Record, Said, Taken};

/// What a key did in an agent view.
pub(crate) enum Key {
    Handled,
    Submit(String),
    Cancel,
    Quit,
}

/// Choose the provider: the package's, else the first ready CLI. Without
/// one, the pane runs in no-model mode and says why.
pub(crate) async fn setup(app: &mut App) {
    let Some(spec) = app.manifest.agent.clone() else {
        return;
    };
    let candidates: Vec<Kind> = match spec.provider.as_deref().and_then(Kind::parse) {
        Some(k) => vec![k],
        None => providers::ORDER.to_vec(),
    };
    let mut why = String::new();
    for k in candidates {
        let p = providers::probe(k).await;
        if p.state == State::Ready {
            app.agent.provider = Some(k.name().to_owned());
            app.agent.mode = format!("{} ready, billed to {}", k.name(), k.billed_to());
            break;
        }
        if why.is_empty() {
            why = format!("{}: {}", k.name(), state_text(&p.state));
        }
    }
    if app.agent.provider.is_none() {
        app.agent.mode = match spec.kind {
            AgentKind::Inspector => format!("no model ({why}); scripted search"),
            AgentKind::Filter => {
                format!("no model ({why}); what you type is watched as plain text")
            }
            _ => format!("no model ({why})"),
        };
    }
    app.agent.grant = grant_text(&spec);
}

fn state_text(s: &State) -> String {
    match s {
        State::Ready => "ready".into(),
        State::NotInstalled => "not installed".into(),
        State::LoggedOut => "logged out".into(),
        State::MeteredLogin(w) | State::Incompatible(w) | State::Unknown(w) => w.clone(),
    }
}

/// The grant as the header shows it.
pub(crate) fn grant_text(spec: &AgentSpec) -> String {
    let t = &spec.tools;
    let methods = if t.methods.is_empty() {
        "no tools".to_owned()
    } else {
        t.methods.join(", ")
    };
    let panes = if t.all_panes {
        "all panes".to_owned()
    } else if t.panes.is_empty() {
        "no panes".to_owned()
    } else {
        t.panes
            .iter()
            .map(|p| format!("%{p}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let ro = if spec.kind.read_only() {
        "read-only; "
    } else {
        ""
    };
    // The limits a run stops at.
    let turns = spec
        .max_turns
        .map(|n| format!("; at most {n} turns"))
        .unwrap_or_default();
    let secs = spec.timeout_s.unwrap_or(300);
    format!("{ro}{methods}; {panes}{turns}; stops after {secs} s")
}

/// A key typed into an agent view: the prompt line, Enter to run, Esc to
/// cancel a run, Ctrl-c to quit. `q` is just a letter here.
pub(crate) fn key(app: &mut App, name: &str) -> Key {
    let a = &mut app.agent;
    match name {
        "Ctrl-c" => Key::Quit,
        "Esc" if a.running => Key::Cancel,
        "Enter" if !a.running && !a.input.trim().is_empty() => {
            let prompt = std::mem::take(&mut a.input);
            Key::Submit(prompt.trim().to_owned())
        }
        "Backspace" => {
            a.input.pop();
            Key::Handled
        }
        "Space" => {
            a.input.push(' ');
            Key::Handled
        }
        k if k.chars().count() == 1 => {
            a.input.push_str(k);
            Key::Handled
        }
        _ => Key::Handled,
    }
}

/// `dream` for the tools: beside this program when installed together,
/// else on `PATH`.
fn dream() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("dream")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("dream"))
}

fn preamble(spec: &AgentSpec) -> String {
    let role = match spec.kind {
        AgentKind::Inspector => {
            "You are an inspector pane in Dreamer, a terminal. Answer questions about the user's terminals using only the Dreamer tools, retrieving the least context that answers the question. You cannot type into panes or change anything."
        }
        AgentKind::Monitor => {
            "You are a monitor pane in Dreamer, a terminal. Explain the state of what you watch using only the Dreamer tools. You cannot type into panes or change anything."
        }
        AgentKind::Filter => {
            "You are a filter pane in Dreamer, a terminal. Turn the user's description into filters with watch_lines: plain text, or a regular expression when one is needed, on the panes they mean. To change a filter, stop it with stop_filter and install it again; list_filters shows what runs. Filters only read and mark lines; they never change what a program wrote. Use find_lines to try a pattern on recent output first. Say in one line what each filter watches. Matched lines appear in this pane as they happen."
        }
        AgentKind::Operator => {
            "You are an operator pane in Dreamer, a terminal. Carry out the user's request with the Dreamer tools. Before acting, say in one line what you will do and to which panes or windows. After each action, check its outcome. If an action fails, name the action and its actual target."
        }
    };
    match &spec.instructions {
        Some(more) => format!("{role}\n\n{more}"),
        None => role.to_owned(),
    }
}

/// The provider request for `prompt`: Dreamer's tools as an MCP server
/// narrowed to the package's `[agent.tools]`, on the same daemon.
pub(crate) fn request(spec: &AgentSpec, prompt: &str, socket: &Path) -> Request {
    let mut args = vec!["mcp".to_owned()];
    for m in &spec.tools.methods {
        args.extend(["--method".into(), m.clone()]);
    }
    for p in &spec.tools.panes {
        args.extend(["--pane".into(), format!("%{p}")]);
    }
    if spec.tools.all_panes {
        args.push("--all-panes".into());
    }
    let mut env = Vec::new();
    if let Some(dir) = socket.parent() {
        env.push(("DREAMER_RUNTIME_DIR".to_owned(), dir.display().to_string()));
    }
    Request {
        prompt: prompt.to_owned(),
        system: Some(preamble(spec)),
        // No methods: no tools at all, rather than an MCP server that may
        // do everything.
        tools: (!spec.tools.methods.is_empty()).then(|| Tools {
            program: dream(),
            args,
        }),
        max_turns: spec.max_turns,
        timeout: Some(Duration::from_secs(spec.timeout_s.unwrap_or(300))),
        env,
        ..Request::default()
    }
}

/// Apply a run's event; `true` when the run has ended.
pub(crate) fn apply(app: &mut App, e: Event) -> bool {
    let operator = app
        .manifest
        .agent
        .as_ref()
        .is_some_and(|s| s.kind == AgentKind::Operator);
    let a = &mut app.agent;
    match e {
        Event::Started { model, .. } => {
            if let Some(m) = model {
                let name = a.provider.clone().unwrap_or_default();
                let billed = Kind::parse(&name).map_or("", |k| k.billed_to());
                a.mode = format!("{name} ({m}), billed to {billed}");
            }
            false
        }
        Event::Text(t) => {
            a.say(Said::Agent, &t);
            false
        }
        Event::ToolCall { id, name, input } => {
            let name = name
                .strip_prefix("mcp__dreamer__")
                .unwrap_or(&name)
                .to_owned();
            // The plan: what the model said before its first action.
            if a.plan.is_none() {
                let said: Vec<&str> = a
                    .transcript
                    .iter()
                    .rev()
                    .take_while(|e| e.who != Said::You)
                    .filter(|e| e.who == Said::Agent)
                    .map(|e| e.text.trim())
                    .collect();
                if !said.is_empty() {
                    a.plan = Some(said.into_iter().rev().collect::<Vec<_>>().join(" "));
                }
            }
            a.say(Said::Tool, &format!("{name} {input}"));
            a.calls.push((id, name, input));
            false
        }
        Event::ToolResult { id, ok, text } => {
            if let Some(i) = a.calls.iter().position(|(c, _, _)| *c == id) {
                let (_, name, input) = a.calls.remove(i);
                if operator {
                    a.taken.extend(taken(&name, &input, ok, &text));
                }
            }
            let first: String = text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            a.say(Said::Result { ok }, &first);
            false
        }
        Event::Limit(l) => {
            a.say(Said::Note, &format!("limit: {l}"));
            false
        }
        Event::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            a.say(
                Said::Note,
                &format!("{input_tokens} tokens in, {output_tokens} out"),
            );
            false
        }
        Event::Done(answer) => {
            // The answer, unless the model's text since the prompt already
            // said it (a provider that streams sends both).
            let streamed = a
                .transcript
                .iter()
                .rev()
                .take_while(|e| e.who != Said::You)
                .any(|e| e.who == Said::Agent);
            if !streamed && !answer.is_empty() {
                a.say(Said::Agent, &answer);
            }
            a.running = false;
            a.calls.clear();
            true
        }
        Event::Failed(why) => {
            a.say(Said::Failed, &why.to_string());
            a.running = false;
            a.calls.clear();
            true
        }
    }
}

/// What a call aimed at, in a few words: its panes, window, program, or
/// id. The host is shown apart.
fn target(input: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(p) = input["pane"].as_u64() {
        parts.push(format!("%{p}"));
    }
    if let Some(ps) = input["panes"].as_array() {
        let ps: Vec<String> = ps
            .iter()
            .filter_map(Value::as_u64)
            .map(|p| format!("%{p}"))
            .collect();
        if !ps.is_empty() {
            parts.push(ps.join(" "));
        }
    }
    if let Some(w) = input["window"].as_str() {
        parts.push(format!("window {w:?}"));
    }
    if let Some(argv) = input["argv"].as_array() {
        let argv: Vec<String> = argv.iter().map(crate::model::text).collect();
        parts.push(argv.join(" "));
    }
    if let Some(id) = input["id"].as_str() {
        parts.push(id.to_owned());
    }
    parts.join(" ")
}

/// An action record's state as one line: the state and what was observed,
/// or why it failed; and whether it is still in progress.
fn action_state(r: &Value) -> (String, bool, bool) {
    let state = crate::model::text(&r["state"]);
    let detail = [&r["observed"], &r["reason"]]
        .into_iter()
        .map(crate::model::text)
        .find(|s| !s.is_empty())
        .map(|d| format!(": {d}"))
        .unwrap_or_default();
    let open = matches!(state.as_str(), "accepted" | "prepared" | "dispatching");
    let ok = !matches!(state.as_str(), "failed" | "cancelled" | "outcome_unknown");
    (format!("{state}{detail}"), open, ok)
}

/// A job record's state as one line, and whether it is still running.
fn job_state(r: &Value) -> (String, bool, bool) {
    let state = crate::model::text(&r["state"]);
    let code = r["exit_code"]
        .as_i64()
        .map(|c| format!(" (exit {c})"))
        .unwrap_or_default();
    (
        format!("{state}{code}"),
        state == "running",
        !matches!(state.as_str(), "failed" | "killed" | "lost"),
    )
}

/// What one tool result says the operator did. A failure names the tool
/// and its target; an action or job is kept with its id, to be read again
/// until it settles.
pub(crate) fn taken(tool: &str, input: &Value, ok: bool, text: &str) -> Vec<Taken> {
    let host = input["host"].as_str().map(str::to_owned);
    let step = |record, (state, open, ok): (String, bool, bool)| Taken {
        tool: tool.to_owned(),
        target: target(input),
        host: host.clone(),
        record,
        state,
        open,
        ok,
    };
    if !ok {
        let why = text.lines().next().unwrap_or("failed").to_owned();
        return vec![step(Record::None, (format!("failed: {why}"), false, false))];
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        // A read or a search: not an action to follow.
        return Vec::new();
    };
    if let Some(moves) = v["moves"].as_array() {
        return moves
            .iter()
            .map(|m| match m["id"].as_str() {
                Some(id) => step(Record::Action(id.to_owned()), action_state(m)),
                None => step(
                    Record::None,
                    (
                        format!("failed: {}", crate::model::text(&m["error"])),
                        false,
                        false,
                    ),
                ),
            })
            .collect();
    }
    let id = v["id"].as_str().map(str::to_owned);
    match id {
        Some(id) if v.get("argv").is_some() => vec![step(Record::Job(id), job_state(&v))],
        Some(id) if v.get("action").is_some() || v.get("observed").is_some() => {
            vec![step(Record::Action(id), action_state(&v))]
        }
        _ => Vec::new(),
    }
}

/// `method` with `params` here, or on the step's host through `hosts.call`.
async fn call_for(
    c: &client::Client,
    host: Option<&str>,
    method: &str,
    params: Value,
) -> Option<Value> {
    match host {
        None => crate::run::call(c, method, params).await.ok(),
        Some(h) => {
            let req = api::Request::from_json(method, params).ok()?;
            let r = crate::run::call(
                c,
                "hosts.call",
                json!({ "name": h, "method": method, "payload": req.encode() }),
            )
            .await
            .ok()?;
            let bytes: Vec<u8> = serde_json::from_value(r["payload"].clone()).ok()?;
            api::Response::decode(method, &bytes)
                .ok()
                .map(|r| r.to_json())
        }
    }
}

/// Read every open step again: an action until it completes or fails, a
/// job until it ends. True when something changed.
pub(crate) async fn refresh(app: &mut App, c: &client::Client) -> bool {
    let mut changed = false;
    for t in app.agent.taken.iter_mut().filter(|t| t.open) {
        let (method, id) = match &t.record {
            Record::Action(id) => ("actions.get", id.clone()),
            Record::Job(id) => ("jobs.get", id.clone()),
            Record::None => continue,
        };
        let Some(r) = call_for(c, t.host.as_deref(), method, json!({ "id": id })).await else {
            continue;
        };
        let (state, open, ok) = if method == "jobs.get" {
            job_state(&r)
        } else {
            action_state(&r)
        };
        if state != t.state || open != t.open {
            (t.state, t.open, t.ok) = (state, open, ok);
            changed = true;
        }
    }
    changed
}

/// Esc during a run: stop the jobs the operator started that still run.
pub(crate) async fn cancel_jobs(app: &mut App, c: &client::Client) {
    let running: Vec<(String, Option<String>)> = app
        .agent
        .taken
        .iter()
        .filter(|t| t.open)
        .filter_map(|t| match &t.record {
            Record::Job(id) => Some((id.clone(), t.host.clone())),
            _ => None,
        })
        .collect();
    for (id, host) in running {
        let done = call_for(c, host.as_deref(), "jobs.cancel", json!({ "id": id })).await;
        app.agent.say(
            Said::Note,
            &match done {
                Some(_) => format!("cancelled job {id}"),
                None => format!("could not cancel job {id}"),
            },
        );
    }
}

/// No-model inspector: the prompt as plain text, searched in the newest
/// lines of the panes the package may read.
pub(crate) async fn scripted(app: &mut App, c: &client::Client, prompt: &str) {
    let Some(spec) = app.manifest.agent.clone() else {
        return;
    };
    if spec.kind == AgentKind::Filter {
        scripted_filter(app, c, &spec, prompt).await;
        return;
    }
    if spec.kind != AgentKind::Inspector {
        app.agent.say(
            Said::Failed,
            &format!(
                "a {} needs a provider: {}",
                spec.kind.name(),
                app.agent.mode
            ),
        );
        return;
    }
    let params = json!({
        "pattern": prompt,
        "literal": true,
        "ignore_case": true,
        "panes": spec.tools.panes,
        "max_matches": 50,
    });
    match crate::run::call(c, "terminal.query", params).await {
        Err((_, e)) => app.agent.say(Said::Failed, &format!("terminal.query: {e}")),
        Ok(r) => {
            // Never the inspector's own pane: its transcript holds earlier
            // results, which would match again and snowball.
            let own = std::env::var("TMUX_PANE")
                .ok()
                .and_then(|p| p.trim_start_matches('%').parse::<u64>().ok());
            let matches: Vec<Value> = r["matches"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|m| own.is_none_or(|own| m["pane"].as_u64() != Some(own)))
                .collect();
            for m in &matches {
                app.agent.say(
                    Said::Result { ok: true },
                    &format!(
                        "%{} {}",
                        m["pane"].as_u64().unwrap_or(0),
                        m["text"].as_str().unwrap_or_default()
                    ),
                );
            }
            let searched = r["panes_searched"].as_u64().unwrap_or(0);
            let cut = if r["truncated"] == Value::Bool(true) {
                " (cut short)"
            } else {
                ""
            };
            app.agent.say(
                Said::Note,
                &format!("{} matching lines in {searched} panes{cut}", matches.len()),
            );
        }
    }
}

/// This pane's own id, from tmux.
fn own_pane() -> Option<u32> {
    std::env::var("TMUX_PANE")
        .ok()
        .and_then(|p| p.trim_start_matches('%').parse().ok())
}

/// No-model filter pane: `stop NAME` removes a filter; anything else is
/// watched as plain text (case-insensitive) on the panes the package may
/// read, never this one, with each matched line marked and shown here.
async fn scripted_filter(app: &mut App, c: &client::Client, spec: &AgentSpec, prompt: &str) {
    let prompt = prompt.trim();
    if let Some(name) = prompt.strip_prefix("stop ") {
        let name = name.trim();
        match crate::run::call(c, "filters.remove", json!({ "name": name })).await {
            Ok(_) => app.agent.say(Said::Note, &format!("stopped {name}")),
            Err((_, e)) => app.agent.say(Said::Failed, &format!("filters.remove: {e}")),
        }
        refresh_filters(app, c).await;
        return;
    }
    let own = own_pane();
    let panes: Vec<u32> = if spec.tools.all_panes {
        match crate::run::call(c, "tmux.topology", json!({})).await {
            Ok(t) => t["panes"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| p["id"].as_u64())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| u32::try_from(p).ok())
                .collect(),
            Err((_, e)) => {
                app.agent.say(Said::Failed, &format!("tmux.topology: {e}"));
                return;
            }
        }
    } else {
        spec.tools.panes.clone()
    };
    // Never this pane: what it shows are earlier matches, which would
    // match again.
    let panes: Vec<u32> = panes.into_iter().filter(|p| Some(*p) != own).collect();
    if panes.is_empty() {
        app.agent.say(Said::Failed, "no panes to watch");
        return;
    }
    let name = filter_name(prompt);
    let params = json!({
        "name": name,
        "panes": panes,
        "pattern": prompt,
        "literal": true,
        "ignore_case": true,
        "event": true,
        "decorate": "highlight",
        "grant": { "methods": ["taps.subscribe"], "panes": panes },
    });
    match crate::run::call(c, "filters.install", params).await {
        Ok(_) => app.agent.say(
            Said::Note,
            &format!(
                "{name} watches {} panes for \"{prompt}\"; `stop {name}` removes it",
                panes.len()
            ),
        ),
        Err((_, e)) => app
            .agent
            .say(Said::Failed, &format!("filters.install: {e}")),
    }
    refresh_filters(app, c).await;
}

/// A filter name from what was typed: `watch-` and its letters and digits.
fn filter_name(prompt: &str) -> String {
    let mut slug = String::new();
    for ch in prompt.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "watch".into()
    } else {
        format!("watch-{slug}")
    }
}

/// The filters this pane may manage, for its header and for which matches
/// it shows.
pub(crate) async fn refresh_filters(app: &mut App, c: &client::Client) {
    if app
        .manifest
        .agent
        .as_ref()
        .is_none_or(|a| a.kind != AgentKind::Filter)
    {
        return;
    }
    if let Ok(l) = crate::run::call(c, "filters.list", json!({})).await {
        app.agent.filters = l["filters"]
            .as_array()
            .map(|fs| {
                fs.iter()
                    .filter_map(|f| f["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
    }
}

/// A filter pane's derived stream: each line its filters matched (never
/// in this pane), and a filter that tripped. Whether anything was shown.
pub(crate) fn filter_events(app: &mut App, events: &[api::DaemonEvent]) -> bool {
    let own = own_pane();
    let mut shown = false;
    for e in events {
        let (Some(subject), state) = (e.subject.as_deref(), e.state.as_deref().unwrap_or(""))
        else {
            continue;
        };
        if !app.agent.filters.iter().any(|f| f == subject) {
            continue;
        }
        match e.kind.as_str() {
            "filters.matched" if e.pane.is_none_or(|p| Some(p) != own) => {
                let pane = e.pane.map(|p| format!("%{p} ")).unwrap_or_default();
                app.agent.say(
                    Said::Result { ok: true },
                    &format!("{pane}{subject}: {state}"),
                );
                shown = true;
            }
            "filters.tripped" => {
                app.agent
                    .say(Said::Failed, &format!("{subject} stopped: {state}"));
                shown = true;
            }
            _ => {}
        }
    }
    shown
}

/// An agentic pane in a preview: the header without probing providers.
pub(crate) fn setup_preview(app: &mut App) {
    if let Some(spec) = app.manifest.agent.clone() {
        app.agent.mode = "preview".into();
        app.agent.grant = grant_text(&spec);
    }
}

#[cfg(test)]
mod filter_tests {
    #[test]
    fn a_filter_name_comes_from_what_was_typed() {
        assert_eq!(super::filter_name("ERROR: disk"), "watch-error-disk");
        assert_eq!(super::filter_name("!!"), "watch");
        assert!(super::filter_name(&"x".repeat(100)).len() <= 30);
    }
}
