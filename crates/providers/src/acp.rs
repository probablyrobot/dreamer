//! Any agent that speaks the Agent Client Protocol on its standard streams
//! (JSON-RPC 2.0, one record per line), as an agent session.
//!
//! Dreamer is the ACP client, at protocol version [`PROTOCOL_VERSION`]
//! (schema release 1.32.0). What the adapter uses:
//!
//! | Direction | Method | Use |
//! | --- | --- | --- |
//! | to the agent | `initialize` | the start check: protocol version, `loadSession`, `sessionCapabilities.resume` |
//! | to the agent | `session/new`, `session/resume`, `session/load` | a new session, or the agent's own one again |
//! | to the agent | `session/set_mode` | the configured mode (`agents.NAME.mode`), before any prompt |
//! | to the agent | `session/prompt` | a prompt; its answer (`stopReason`) ends the turn |
//! | to the agent | `session/cancel` (notification) | cancelling a turn |
//! | from the agent | `session/update` (notification) | text, thought, tool calls, plan, usage |
//! | from the agent | `session/request_permission` | approval requests |
//!
//! Dreamer offers no file system or terminal methods (its capabilities say
//! so), and refuses them if asked. It never calls `authenticate`: an agent
//! that needs a login is told to use its own. Dreamer's tools reach the
//! agent as one MCP server, `dream mcp`, in `session/new`. A prompt sent
//! while a turn runs waits for that turn to end.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{
    ApprovalRequest, Harness, Input, PlanEntry, Spec, StopReason, ToolCall, ToolStatus, Update,
    Usage,
};
use crate::stdio::{Proc, Rpc, error_text, id_text, is_response};

/// The ACP protocol version this client speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC's "method not found".
const NOT_FOUND: i64 = -32601;
/// ACP's "authentication required".
const AUTH_REQUIRED: i64 = -32000;

/// Start an ACP agent for a session. Must be called inside a Tokio runtime.
/// The session modes an agent's `session/new` (or `load`) answer offers.
fn offered_modes(result: &Value) -> Vec<String> {
    result["modes"]["availableModes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

pub async fn start(spec: &Spec) -> Result<Harness, String> {
    let version = crate::stdio::version(spec).await;
    let mut proc = Proc::spawn("the ACP agent", crate::stdio::command(spec, &[]))?;
    let tx = proc.sender();
    let mut rpc = Rpc::new(true);
    let mut early = Vec::new();
    let checked = async {
        let (id, init) = rpc.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
                "clientInfo": { "name": "dreamer", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let _ = tx.send(init);
        let r = proc
            .until("`initialize`", &mut early, |v| is_response(v, id))
            .await?;
        let caps = initialized(&r)?;
        let servers = mcp_servers(spec);
        let cwd = spec.cwd.display().to_string();
        let (method, resumed) = match &spec.resume {
            Some(_) if caps.resume => ("session/resume", true),
            Some(_) if caps.load => ("session/load", true),
            _ => ("session/new", false),
        };
        let mut params = json!({ "cwd": cwd, "mcpServers": servers });
        if resumed {
            params["sessionId"] = json!(spec.resume.clone().unwrap_or_default());
        }
        let (id, req) = rpc.request(method, params);
        let _ = tx.send(req);
        let r = proc
            .until(&format!("`{method}`"), &mut early, |v| is_response(v, id))
            .await?;
        if let Some(e) = r.get("error") {
            if e["code"].as_i64() == Some(AUTH_REQUIRED) {
                return Err(
                    "it needs a login; log it in with its own login command (Dreamer never authenticates for it)"
                        .to_owned(),
                );
            }
            return Err(format!(
                "`{method}` failed: {}",
                error_text(&r).unwrap_or_default()
            ));
        }
        let sid = if resumed {
            spec.resume.clone().unwrap_or_default()
        } else {
            r["result"]["sessionId"]
                .as_str()
                .ok_or("its `session/new` answer has no `sessionId`")?
                .to_owned()
        };
        // A loaded session's history arrives again as updates: already in
        // the transcript, so not repeated.
        if method == "session/load" {
            early.retain(|v| v["method"] != "session/update");
        }
        if let Some(mode) = &spec.mode {
            let offered = offered_modes(&r["result"]);
            if !offered.is_empty() && !offered.iter().any(|m| m == mode) {
                return Err(format!(
                    "it has no session mode `{mode}` (it offers {})",
                    offered.join(", ")
                ));
            }
            let (id, req) = rpc.request(
                "session/set_mode",
                json!({ "sessionId": sid, "modeId": mode }),
            );
            let _ = tx.send(req);
            let r = proc
                .until("`session/set_mode`", &mut early, |v| is_response(v, id))
                .await?;
            if r.get("error").is_some() {
                return Err(format!(
                    "`session/set_mode` to `{mode}` failed: {}",
                    error_text(&r).unwrap_or_default()
                ));
            }
        }
        Ok::<_, String>(sid)
    };
    let sid = match checked.await {
        Ok(s) => s,
        Err(why) => return Err(proc.fail(version.as_deref(), why).await),
    };
    let (updates_tx, updates) = mpsc::unbounded_channel();
    let _ = updates_tx.send(Update::HarnessSession {
        id: sid.clone(),
        file: None,
    });
    let pid = proc.pid;
    let mut state = Acp::new(sid.clone(), rpc);
    let control = proc.run(updates_tx, move |mut out, mut inputs, updates| async move {
        let apply = |o: Out| {
            for w in o.writes {
                let _ = tx.send(w);
            }
            for u in o.updates {
                let _ = updates.send(u);
            }
        };
        for v in early {
            apply(state.message(&v));
        }
        let mut inputs_open = true;
        loop {
            tokio::select! {
                v = out.recv() => match v {
                    Some(v) => apply(state.message(&v)),
                    None => break,
                },
                i = inputs.recv(), if inputs_open => match i {
                    Some(i) => apply(state.input(i)),
                    None => inputs_open = false,
                },
            }
        }
    });
    Ok(Harness {
        pid,
        version,
        session: Some((sid, None)),
        control,
        updates,
    })
}

/// What the agent said it can do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub load: bool,
    pub resume: bool,
}

/// Check an `initialize` answer.
pub fn initialized(r: &Value) -> Result<Capabilities, String> {
    if let Some(e) = error_text(r) {
        return Err(format!("`initialize` failed: {e}"));
    }
    let res = &r["result"];
    match res["protocolVersion"].as_u64() {
        Some(PROTOCOL_VERSION) => {}
        Some(v) => {
            return Err(format!(
                "it speaks ACP version {v}; Dreamer speaks version {PROTOCOL_VERSION}"
            ));
        }
        None => return Err("its `initialize` answer has no protocol version".to_owned()),
    }
    let caps = &res["agentCapabilities"];
    Ok(Capabilities {
        load: caps["loadSession"] == true,
        resume: caps["sessionCapabilities"]["resume"].is_object(),
    })
}

/// Dreamer's MCP server for the agent, with the session's environment.
fn mcp_servers(spec: &Spec) -> Value {
    let Some(t) = &spec.tools else {
        return json!([]);
    };
    let env: Vec<Value> = spec
        .env
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();
    json!([{
        "name": "dreamer",
        "command": t.program.display().to_string(),
        "args": t.args,
        "env": env,
    }])
}

/// What one record or input means: updates for the session and records to
/// write to the agent.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub updates: Vec<Update>,
    pub writes: Vec<Value>,
}

/// A permission option the agent offered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    id: String,
    name: String,
    allow: bool,
    kind: String,
}

/// The client side of one ACP session. Pure, so recorded records test it.
#[derive(Debug)]
pub struct Acp {
    session: String,
    rpc: Rpc,
    /// The `session/prompt` request of the running turn.
    turn: Option<u64>,
    queue: VecDeque<String>,
    /// Permission requests waiting, by the id shown to Dreamer.
    asks: HashMap<String, (Value, Vec<Choice>)>,
    /// Tool names by call id.
    tools: HashMap<String, String>,
    cancelled: bool,
}

/// The text of content blocks (or one block).
fn content_text(v: &Value) -> Option<String> {
    let blocks: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Null => return None,
        one => vec![one],
    };
    let mut parts = Vec::new();
    for b in blocks {
        // A tool call's content wraps a block, or is a diff.
        let b = if b["type"] == "content" {
            &b["content"]
        } else {
            b
        };
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or_default().to_owned()),
            Some("diff") => parts.push(format!("edited {}", b["path"].as_str().unwrap_or("?"))),
            Some("resource_link") => {
                parts.push(b["uri"].as_str().unwrap_or_default().to_owned());
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| crate::clip(&parts.join("\n")))
}

fn status(v: &Value) -> Option<ToolStatus> {
    Some(match v.as_str()? {
        "pending" | "in_progress" => ToolStatus::Updated,
        "completed" => ToolStatus::Finished,
        "failed" => ToolStatus::Failed,
        _ => return None,
    })
}

impl Acp {
    pub(crate) fn new(session: String, rpc: Rpc) -> Acp {
        Acp {
            session,
            rpc,
            turn: None,
            queue: VecDeque::new(),
            asks: HashMap::new(),
            tools: HashMap::new(),
            cancelled: false,
        }
    }

    fn prompt(&mut self, text: String, out: &mut Out) {
        let (id, req) = self.rpc.request(
            "session/prompt",
            json!({
                "sessionId": self.session,
                "prompt": [{ "type": "text", "text": text }],
            }),
        );
        self.turn = Some(id);
        self.cancelled = false;
        out.writes.push(req);
    }

    /// The turn ended: the next queued prompt starts.
    fn ended(&mut self, reason: StopReason, out: &mut Out) {
        self.turn = None;
        self.tools.clear();
        // Requests of a finished turn are void.
        self.asks.clear();
        out.updates.push(Update::TurnEnd(reason));
        if let Some(next) = self.queue.pop_front() {
            self.prompt(next, out);
        }
    }

    pub fn input(&mut self, i: Input) -> Out {
        let mut out = Out::default();
        match i {
            Input::Prompt(text) => {
                if self.turn.is_some() {
                    self.queue.push_back(text);
                } else {
                    self.prompt(text, &mut out);
                }
            }
            Input::Cancel => {
                self.queue.clear();
                if self.turn.is_some() {
                    self.cancelled = true;
                    out.writes.push(
                        self.rpc
                            .notify("session/cancel", json!({ "sessionId": self.session })),
                    );
                    // The protocol asks a client to answer open permission
                    // requests as cancelled.
                    for (_, (id, _)) in self.asks.drain() {
                        out.writes.push(
                            self.rpc
                                .result(&id, json!({ "outcome": { "outcome": "cancelled" } })),
                        );
                    }
                }
            }
            Input::Answer { id, allow, option } => {
                let Some((rid, choices)) = self.asks.remove(&id) else {
                    out.updates.push(Update::Error(format!(
                        "no approval request {id} is waiting"
                    )));
                    return out;
                };
                let picked = choices
                    .iter()
                    .filter(|c| c.allow == allow)
                    .find(|c| option.as_deref().is_some_and(|o| o == c.name || o == c.id))
                    .or_else(|| {
                        let once = if allow { "allow_once" } else { "reject_once" };
                        choices.iter().find(|c| c.kind == once)
                    })
                    .or_else(|| choices.iter().find(|c| c.allow == allow));
                let outcome = match picked {
                    Some(c) => json!({ "outcome": "selected", "optionId": c.id }),
                    // No option says no: the request is cancelled instead.
                    None => json!({ "outcome": "cancelled" }),
                };
                out.writes
                    .push(self.rpc.result(&rid, json!({ "outcome": outcome })));
            }
        }
        out
    }

    pub fn message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let method = v["method"].as_str();
        match (method, v.get("id")) {
            // A response: the running turn's prompt is the one that matters.
            (None, Some(id)) => {
                if id.as_u64().is_some() && id.as_u64() == self.turn {
                    if let Some(e) = error_text(v) {
                        out.updates.push(Update::Error(format!("the agent: {e}")));
                        self.ended(StopReason::Error, &mut out);
                    } else {
                        let reason = match v["result"]["stopReason"].as_str() {
                            Some("cancelled") => StopReason::Cancelled,
                            _ if self.cancelled => StopReason::Cancelled,
                            Some("refusal") => {
                                out.updates
                                    .push(Update::Error("the agent refused the prompt".into()));
                                StopReason::Error
                            }
                            _ => StopReason::EndTurn,
                        };
                        self.ended(reason, &mut out);
                    }
                }
            }
            (Some("session/update"), None) => self.update(&v["params"]["update"], &mut out),
            (Some("session/request_permission"), Some(id)) => {
                let p = &v["params"];
                let choices: Vec<Choice> = p["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| {
                        let kind = o["kind"].as_str().unwrap_or_default().to_owned();
                        Choice {
                            id: o["optionId"].as_str().unwrap_or_default().to_owned(),
                            name: o["name"].as_str().unwrap_or_default().to_owned(),
                            allow: kind.starts_with("allow"),
                            kind,
                        }
                    })
                    .collect();
                let call = &p["toolCall"];
                let key = id_text(id);
                let title = call["title"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        call["toolCallId"]
                            .as_str()
                            .and_then(|c| self.tools.get(c).cloned())
                    })
                    .unwrap_or_else(|| "Allow a tool call?".to_owned());
                let message = (!call["rawInput"].is_null()).then(|| call["rawInput"].to_string());
                out.updates.push(Update::Approval(ApprovalRequest {
                    id: key.clone(),
                    title,
                    message,
                    options: choices.iter().map(|c| c.name.clone()).collect(),
                    timeout_ms: None,
                }));
                self.asks.insert(key, (id.clone(), choices));
            }
            // File system, terminal, elicitation: Dreamer offers none.
            (Some(m), Some(id)) => out.writes.push(self.rpc.error(
                id,
                NOT_FOUND,
                &format!("Dreamer does not offer `{m}`"),
            )),
            _ => {}
        }
        out
    }

    fn update(&mut self, u: &Value, out: &mut Out) {
        let text = |u: &Value| content_text(&u["content"]).unwrap_or_default();
        match u["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => out.updates.push(Update::Text(text(u))),
            "agent_thought_chunk" => out.updates.push(Update::Thinking(text(u))),
            "tool_call" => {
                let id = u["toolCallId"].as_str().unwrap_or_default().to_owned();
                let name = u["name"]
                    .as_str()
                    .or(u["title"].as_str())
                    .unwrap_or("tool")
                    .to_owned();
                self.tools.insert(id.clone(), name.clone());
                out.updates.push(Update::Tool(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Started,
                    input: (!u["rawInput"].is_null()).then(|| u["rawInput"].clone()),
                    output: None,
                }));
                // A call reported already finished.
                if let Some(s @ (ToolStatus::Finished | ToolStatus::Failed)) = status(&u["status"])
                {
                    out.updates.push(Update::Tool(ToolCall {
                        id,
                        name,
                        status: s,
                        input: None,
                        output: content_text(&u["content"]),
                    }));
                }
            }
            "tool_call_update" => {
                let id = u["toolCallId"].as_str().unwrap_or_default().to_owned();
                let name = self
                    .tools
                    .get(&id)
                    .cloned()
                    .or_else(|| u["title"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "tool".to_owned());
                let output = content_text(&u["content"]).or_else(|| {
                    (!u["rawOutput"].is_null()).then(|| crate::clip(&u["rawOutput"].to_string()))
                });
                out.updates.push(Update::Tool(ToolCall {
                    id,
                    name,
                    status: status(&u["status"]).unwrap_or(ToolStatus::Updated),
                    input: (!u["rawInput"].is_null()).then(|| u["rawInput"].clone()),
                    output,
                }));
            }
            "plan" => out.updates.push(Update::Plan(
                u["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| PlanEntry {
                        content: e["content"].as_str().unwrap_or_default().to_owned(),
                        status: e["status"].as_str().unwrap_or("pending").to_owned(),
                        priority: e["priority"].as_str().map(str::to_owned),
                    })
                    .collect(),
            )),
            // The context window's use, and the session's cost so far.
            "usage_update" if u["cost"]["currency"] == "USD" => {
                out.updates.push(Update::Usage(Usage {
                    cost_usd: u["cost"]["amount"].as_f64(),
                    ..Usage::default()
                }));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acp() -> Acp {
        Acp::new("sess-1".into(), Rpc::new(true))
    }

    #[test]
    fn the_start_check_wants_version_one() {
        let ok = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,
            "agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}}},"authMethods":[]}});
        assert_eq!(
            initialized(&ok).expect("ok"),
            Capabilities {
                load: true,
                resume: true
            }
        );
        let v2 = json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}});
        assert!(initialized(&v2).expect_err("v2").contains("version 2"));
    }

    #[test]
    fn prompts_queue_behind_a_running_turn_and_updates_map_to_events() {
        let mut a = acp();
        let o = a.input(Input::Prompt("one".into()));
        assert_eq!(o.writes[0]["method"], "session/prompt");
        assert_eq!(o.writes[0]["params"]["prompt"][0]["text"], "one");
        let first = o.writes[0]["id"].clone();
        assert!(a.input(Input::Prompt("two".into())).writes.is_empty());
        let upd = |u: Value| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":u}});
        let mut got = Vec::new();
        for u in [
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hi"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read file","kind":"read","status":"pending","rawInput":{"path":"a"}}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"abc"}}]}),
            json!({"sessionUpdate":"plan","entries":[{"content":"Do it","priority":"high","status":"in_progress"}]}),
            json!({"sessionUpdate":"usage_update","used":10,"size":100,"cost":{"amount":0.5,"currency":"USD"}}),
            json!({"sessionUpdate":"available_commands_update","availableCommands":[]}),
        ] {
            got.extend(a.message(&upd(u)).updates);
        }
        assert_eq!(
            got,
            vec![
                Update::Thinking("hmm".into()),
                Update::Text("Hi".into()),
                Update::Tool(ToolCall {
                    id: "t1".into(),
                    name: "Read file".into(),
                    status: ToolStatus::Started,
                    input: Some(json!({"path":"a"})),
                    output: None,
                }),
                Update::Tool(ToolCall {
                    id: "t1".into(),
                    name: "Read file".into(),
                    status: ToolStatus::Finished,
                    input: None,
                    output: Some("abc".into()),
                }),
                Update::Plan(vec![PlanEntry {
                    content: "Do it".into(),
                    status: "in_progress".into(),
                    priority: Some("high".into()),
                }]),
                Update::Usage(Usage {
                    cost_usd: Some(0.5),
                    ..Usage::default()
                }),
            ]
        );
        // The turn ends; the queued prompt goes out.
        let o = a.message(&json!({"jsonrpc":"2.0","id":first,"result":{"stopReason":"end_turn"}}));
        assert_eq!(o.updates, vec![Update::TurnEnd(StopReason::EndTurn)]);
        assert_eq!(o.writes[0]["params"]["prompt"][0]["text"], "two");
    }

    #[test]
    fn permissions_pick_the_option_by_kind_and_cancel_answers_them() {
        let mut a = acp();
        a.input(Input::Prompt("go".into()));
        let ask = json!({"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{
            "sessionId":"sess-1","toolCall":{"toolCallId":"t1","title":"Run ls"},
            "options":[{"optionId":"a1","name":"Allow","kind":"allow_once"},
                       {"optionId":"a2","name":"Always","kind":"allow_always"},
                       {"optionId":"r1","name":"Reject","kind":"reject_once"}]}});
        let o = a.message(&ask);
        assert!(
            matches!(&o.updates[0], Update::Approval(r) if r.id == "7" && r.title == "Run ls" && r.options == ["Allow", "Always", "Reject"])
        );
        let o = a.input(Input::Answer {
            id: "7".into(),
            allow: true,
            option: Some("Always".into()),
        });
        assert_eq!(o.writes[0]["id"], 7);
        assert_eq!(o.writes[0]["result"]["outcome"]["optionId"], "a2");
        a.message(&ask);
        // Denying never picks an allow option, even when named.
        let o = a.input(Input::Answer {
            id: "7".into(),
            allow: false,
            option: Some("Allow".into()),
        });
        assert_eq!(o.writes[0]["result"]["outcome"]["optionId"], "r1");
        a.message(&ask);
        let o = a.input(Input::Cancel);
        assert_eq!(o.writes[0]["method"], "session/cancel");
        assert_eq!(o.writes[1]["result"]["outcome"]["outcome"], "cancelled");
        // Unoffered client methods are refused.
        let o =
            a.message(&json!({"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{}}));
        assert_eq!(o.writes[0]["error"]["code"], NOT_FOUND);
    }
}
