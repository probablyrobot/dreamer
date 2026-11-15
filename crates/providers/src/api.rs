//! The API adapters (spec 13.5): the Claude Messages API and the OpenAI
//! Chat Completions API, used only when configured. They keep the CLI
//! adapters' contract: the same events, cancellation, usage, and Dreamer's
//! tools, which they take from `dream mcp` and run in a tool loop
//! themselves.
//!
//! Requests go through the system `curl`, one per model turn, in a process
//! group of its own. The key and the request body are handed to curl on
//! its standard input (`--config -`), so the key never appears in a
//! process listing or on disk. Keys come from the environment or the OS
//! keychain, never from a manifest, skin, or the action ledger.

use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

use crate::{Event, Failure, Request, classify, clip, mcp};

/// Which API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Claude,
    OpenAi,
}

/// Where an API key comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// An environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`).
    Env(String),
    /// The OS keychain: macOS `security`, or Secret Service (`secret-tool`)
    /// on Linux, by service and account.
    Keychain { service: String, account: String },
    /// A key the caller already holds (never read from configuration).
    Given(String),
}

/// An API adapter's configuration. The model is configuration, never a
/// constant in code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub api: Api,
    pub model: String,
    pub key: KeySource,
    /// The API's base URL; the vendor's by default.
    pub base_url: Option<String>,
    pub max_tokens: u32,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Claude => "claude-api",
            Api::OpenAi => "openai-api",
        }
    }

    fn base(self) -> &'static str {
        match self {
            Api::Claude => "https://api.anthropic.com",
            Api::OpenAi => "https://api.openai.com/v1",
        }
    }
}

/// Read the key; its text is never logged or put in an event.
pub async fn key(source: &KeySource) -> Result<String, String> {
    let k = match source {
        KeySource::Given(k) => k.clone(),
        KeySource::Env(var) => std::env::var(var).map_err(|_| format!("{var} is not set"))?,
        KeySource::Keychain { service, account } => {
            let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
                (
                    "security",
                    vec!["find-generic-password", "-s", service, "-a", account, "-w"],
                )
            } else {
                (
                    "secret-tool",
                    vec!["lookup", "service", service, "account", account],
                )
            };
            let out = tokio::process::Command::new(program)
                .args(&args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .map_err(|e| format!("{program}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "no key in the keychain for service {service}, account {account}"
                ));
            }
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
    };
    if k.trim().is_empty() {
        return Err("the API key is empty".into());
    }
    Ok(k)
}

/// A string for curl's config syntax.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// curl's config for one request: URL, headers (with the key), and body.
fn curl_config(cfg: &ApiConfig, key: &str, body: &Value) -> String {
    let base = cfg.base_url.as_deref().unwrap_or(cfg.api.base());
    let base = base.trim_end_matches('/');
    let (url, auth) = match cfg.api {
        Api::Claude => (
            format!("{base}/v1/messages"),
            vec![
                format!("x-api-key: {key}"),
                "anthropic-version: 2023-06-01".to_owned(),
            ],
        ),
        Api::OpenAi => (
            format!("{base}/chat/completions"),
            vec![format!("authorization: Bearer {key}")],
        ),
    };
    let mut c = format!("url = {}\n", quoted(&url));
    for h in auth.iter().map(String::as_str).chain([
        "content-type: application/json",
        "accept: text/event-stream",
    ]) {
        c.push_str(&format!("header = {}\n", quoted(h)));
    }
    c.push_str(&format!("data-binary = {}\n", quoted(&body.to_string())));
    c
}

/// One turn's outcome.
#[derive(Debug, Default)]
struct Turn {
    /// Assistant content to append to the conversation.
    text: String,
    /// (id, name, input)
    calls: Vec<(String, String, Value)>,
    stop: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    /// The HTTP status, and an error body if one came instead of events.
    status: Option<u16>,
    error: Option<String>,
}

/// Parse one streamed line (`data: {...}`) into the turn, sending text.
fn line(
    api: Api,
    l: &str,
    t: &mut Turn,
    partial: &mut Vec<(String, String, String)>,
    tx: &mpsc::UnboundedSender<Event>,
) {
    if let Some(code) = l.strip_prefix("DREAMER-HTTP ") {
        t.status = code.trim().parse().ok();
        return;
    }
    let Some(data) = l.strip_prefix("data:").map(str::trim) else {
        // Not an event: an error body, kept for the reason.
        if l.trim_start().starts_with('{') || t.error.is_some() {
            t.error.get_or_insert_with(String::new).push_str(l);
        }
        return;
    };
    if data == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    match api {
        Api::Claude => match v["type"].as_str() {
            Some("message_start") => {
                let u = &v["message"]["usage"];
                t.input_tokens += [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|k| u[*k].as_u64())
                .sum::<u64>();
            }
            Some("content_block_start") => {
                let b = &v["content_block"];
                if b["type"] == "tool_use" {
                    partial.push((
                        b["id"].as_str().unwrap_or_default().to_owned(),
                        b["name"].as_str().unwrap_or_default().to_owned(),
                        String::new(),
                    ));
                }
            }
            Some("content_block_delta") => {
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let s = d["text"].as_str().unwrap_or_default();
                        t.text.push_str(s);
                        let _ = tx.send(Event::Text(s.to_owned()));
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = partial.last_mut() {
                            p.2.push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                t.stop = v["delta"]["stop_reason"].as_str().map(str::to_owned);
                t.output_tokens += v["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            Some("error") => {
                t.error = Some(v["error"]["message"].as_str().unwrap_or("error").to_owned());
            }
            _ => {}
        },
        Api::OpenAi => {
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                t.input_tokens += u["prompt_tokens"].as_u64().unwrap_or(0);
                t.output_tokens += u["completion_tokens"].as_u64().unwrap_or(0);
            }
            let Some(c) = v["choices"].get(0) else { return };
            let d = &c["delta"];
            if let Some(s) = d["content"].as_str() {
                t.text.push_str(s);
                let _ = tx.send(Event::Text(s.to_owned()));
            }
            for call in d["tool_calls"].as_array().into_iter().flatten() {
                let i = call["index"].as_u64().unwrap_or(0) as usize;
                while partial.len() <= i {
                    partial.push(Default::default());
                }
                let p = &mut partial[i];
                if let Some(id) = call["id"].as_str() {
                    p.0 = id.to_owned();
                }
                if let Some(n) = call["function"]["name"].as_str() {
                    p.1.push_str(n);
                }
                if let Some(a) = call["function"]["arguments"].as_str() {
                    p.2.push_str(a);
                }
            }
            if let Some(f) = c["finish_reason"].as_str() {
                t.stop = Some(f.to_owned());
            }
        }
    }
}

/// Run one request through curl; `None` when cancelled.
async fn turn(
    cfg: &ApiConfig,
    key: &str,
    body: &Value,
    tx: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<Turn>, Failure> {
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--no-buffer",
        "--config",
        "-",
        "--write-out",
        "\nDREAMER-HTTP %{http_code}\n",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0)
    .kill_on_drop(true);
    let mut child = crate::spawn(&mut cmd).map_err(|e| Failure::Failed(format!("curl: {e}")))?;
    let pgid = child.id().unwrap_or_default();
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(curl_config(cfg, key, body).as_bytes())
            .await
            .map_err(|e| Failure::Failed(format!("curl: {e}")))?;
    }
    let mut lines = BufReader::new(
        child
            .stdout
            .take()
            .ok_or(Failure::Failed("curl: no output".into()))?,
    )
    .lines();
    let mut t = Turn::default();
    let mut partial = Vec::new();
    loop {
        tokio::select! {
            l = lines.next_line() => match l {
                Ok(Some(l)) => line(cfg.api, &l, &mut t, &mut partial, tx),
                _ => break,
            },
            _ = cancel.changed() => {
                let _ = platform::terminate_group(pgid, true);
                let _ = child.wait().await;
                return Ok(None);
            }
        }
    }
    let status = child.wait().await;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let _ = e.read_to_string(&mut err).await;
    }
    if status.map(|s| !s.success()).unwrap_or(true) {
        return Err(Failure::Failed(format!("curl: {}", err.trim())));
    }
    let code = t.status.unwrap_or(0);
    if !(200..300).contains(&code) || t.error.is_some() {
        let body = t.error.clone().unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(body);
        let why = format!("HTTP {code}: {message}");
        return Err(match code {
            401 | 403 => Failure::LoggedOut(format!("the API key was refused ({why})")),
            429 => Failure::RateLimited(why),
            _ => classify(&why),
        });
    }
    t.calls = partial
        .into_iter()
        .filter(|p| !p.1.is_empty())
        .map(|(id, name, args)| {
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&args).unwrap_or(json!({}))
            };
            (id, name, input)
        })
        .collect();
    Ok(Some(t))
}

/// The first request's body.
fn first_body(cfg: &ApiConfig, req: &Request, tools: &[mcp::Tool]) -> Value {
    match cfg.api {
        Api::Claude => {
            let mut b = json!({
                "model": cfg.model,
                "max_tokens": cfg.max_tokens,
                "stream": true,
                "messages": [{ "role": "user", "content": req.prompt }],
            });
            if let Some(s) = &req.system {
                b["system"] = json!(s);
            }
            if !tools.is_empty() {
                b["tools"] = tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema }))
                    .collect();
            }
            b
        }
        Api::OpenAi => {
            let mut messages = Vec::new();
            if let Some(s) = &req.system {
                messages.push(json!({ "role": "system", "content": s }));
            }
            messages.push(json!({ "role": "user", "content": req.prompt }));
            let mut b = json!({
                "model": cfg.model,
                "max_completion_tokens": cfg.max_tokens,
                "stream": true,
                "stream_options": { "include_usage": true },
                "messages": messages,
            });
            if !tools.is_empty() {
                b["tools"] = tools
                    .iter()
                    .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } }))
                    .collect();
            }
            b
        }
    }
}

/// Append the model's turn and the tools' results to the conversation.
fn next_body(cfg: &ApiConfig, body: &mut Value, t: &Turn, results: &[(String, bool, String)]) {
    let messages = body["messages"].as_array_mut();
    let Some(messages) = messages else { return };
    match cfg.api {
        Api::Claude => {
            let mut content = Vec::new();
            if !t.text.is_empty() {
                content.push(json!({ "type": "text", "text": t.text }));
            }
            for (id, name, input) in &t.calls {
                content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
            }
            messages.push(json!({ "role": "assistant", "content": content }));
            let results: Vec<Value> = results
                .iter()
                .map(|(id, ok, text)| json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": !ok }))
                .collect();
            messages.push(json!({ "role": "user", "content": results }));
        }
        Api::OpenAi => {
            let calls: Vec<Value> = t
                .calls
                .iter()
                .map(|(id, name, input)| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }))
                .collect();
            messages.push(json!({ "role": "assistant", "content": if t.text.is_empty() { Value::Null } else { json!(t.text) }, "tool_calls": calls }));
            for (id, _, text) in results {
                messages.push(json!({ "role": "tool", "tool_call_id": id, "content": text }));
            }
        }
    }
}

/// Start a run on an API. Must be called inside a Tokio runtime.
pub fn start_api(cfg: &ApiConfig, req: &Request) -> crate::Run {
    let (tx, events) = mpsc::unbounded_channel();
    let (cancel, mut cancelled) = watch::channel(false);
    let cfg = cfg.clone();
    let req = req.clone();
    tokio::spawn(async move {
        let fail = |f: Failure| {
            let _ = tx.send(Event::Failed(f));
        };
        let key = match key(&cfg.key).await {
            Ok(k) => k,
            Err(why) => return fail(Failure::LoggedOut(why)),
        };
        let mut tools = None;
        let mut list = Vec::new();
        if let Some(t) = &req.tools {
            match mcp::Client::start(t, &req.env).await {
                Ok(mut c) => match c.list().await {
                    Ok(l) => {
                        list = l;
                        tools = Some(c);
                    }
                    Err(e) => return fail(Failure::Failed(format!("tools: {e}"))),
                },
                Err(e) => return fail(Failure::Failed(format!("tools: {e}"))),
            }
        }
        let _ = tx.send(Event::Started {
            session: None,
            model: Some(cfg.model.clone()),
        });
        let deadline = req.timeout.map(|t| tokio::time::Instant::now() + t);
        let mut body = first_body(&cfg, &req, &list);
        let (mut input, mut output) = (0, 0);
        let max_turns = req.max_turns.unwrap_or(16).max(1);
        let outcome = async {
            for _ in 0..max_turns {
                let t = match turn(&cfg, &key, &body, &tx, &mut cancelled).await? {
                    Some(t) => t,
                    None => return Err(Failure::Cancelled),
                };
                input += t.input_tokens;
                output += t.output_tokens;
                if t.calls.is_empty() {
                    return Ok(t.text);
                }
                let mut results = Vec::new();
                for (id, name, args) in &t.calls {
                    let _ = tx.send(Event::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: args.clone(),
                    });
                    let (ok, text) = match tools.as_mut() {
                        Some(c) => c.call(name, args).await,
                        None => (false, "no tools were given".into()),
                    };
                    let text = clip(&text);
                    let _ = tx.send(Event::ToolResult {
                        id: id.clone(),
                        ok,
                        text: text.clone(),
                    });
                    results.push((id.clone(), ok, text));
                }
                next_body(&cfg, &mut body, &t, &results);
            }
            Err(Failure::Failed(format!("stopped after {max_turns} turns")))
        };
        let result = match deadline {
            Some(d) => tokio::time::timeout_at(d, outcome)
                .await
                .unwrap_or(Err(Failure::TimedOut)),
            None => outcome.await,
        };
        let _ = tx.send(Event::Usage {
            input_tokens: input,
            output_tokens: output,
            cost_usd: None,
        });
        if let Some(pg) = tools.as_ref().and_then(mcp::Client::pgid) {
            let _ = platform::terminate_group(pg, false);
        }
        match result {
            Ok(text) => {
                let _ = tx.send(Event::Done(text));
            }
            Err(f) => fail(f),
        }
    });
    crate::Run::new(events, cancel, 0)
}
