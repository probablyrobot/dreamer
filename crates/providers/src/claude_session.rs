//! Claude Code as an agent session: one long-lived `claude -p` with
//! stream-json input and output, written to and read from as JSON lines.
//!
//! Arguments: `-p --input-format stream-json --output-format stream-json
//! --verbose --include-partial-messages --permission-prompt-tool stdio
//! --permission-mode manual --strict-mcp-config --mcp-config <dreamer>
//! --allowedTools mcp__dreamer`, plus `--model` and `--resume <session>`.
//! Checked against the help of Claude Code 2.1.326.
//!
//! | Direction | Record | Use |
//! | --- | --- | --- |
//! | in | `control_request` `initialize` | the start check |
//! | in | `user` | a prompt (Claude Code queues one sent during a turn) |
//! | in | `control_request` `interrupt` | cancelling a turn |
//! | in | `control_response` | answering a `can_use_tool` request |
//! | out | `system` `init` | Claude Code's session id; a metered login stops the session |
//! | out | `stream_event` | text and thinking deltas |
//! | out | `assistant`, `user` | tool calls and their results; TodoWrite as the plan |
//! | out | `result` | usage, cost, and the end of the turn |
//! | out | `control_request` `can_use_tool` | approval requests |
//!
//! Dreamer's tools are `dream mcp` (the only MCP server), already bounded
//! by the session's grant, so they are allowed without asking. Claude
//! Code's own tools ask Dreamer each time (`manual`), whatever the user's
//! own permission mode.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{
    ApprovalRequest, Harness, Input, PlanEntry, Spec, StopReason, ToolCall, ToolStatus, Update,
    Usage,
};
use crate::stdio::Proc;

/// The arguments after the configured command.
pub fn arguments(spec: &Spec) -> Vec<String> {
    let mut mcp = json!({ "mcpServers": {} });
    if let Some(t) = &spec.tools {
        let env: serde_json::Map<String, Value> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        mcp["mcpServers"]["dreamer"] = json!({
            "command": t.program.display().to_string(),
            "args": t.args,
            "env": env,
        });
    }
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "manual",
        "--strict-mcp-config",
        "--mcp-config",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    a.push(mcp.to_string());
    if spec.tools.is_some() {
        a.extend(["--allowedTools".into(), "mcp__dreamer".into()]);
    }
    if let Some(m) = &spec.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(r) = &spec.resume {
        a.extend(["--resume".into(), r.clone()]);
    }
    a
}

/// Start Claude Code for a session. Must be called inside a Tokio runtime.
pub async fn start(spec: &Spec) -> Result<Harness, String> {
    let version = crate::stdio::version(spec).await;
    let mut proc = Proc::spawn("Claude Code", crate::stdio::command(spec, &arguments(spec)))?;
    let tx = proc.sender();
    let mut early = Vec::new();
    let _ = tx.send(json!({
        "type": "control_request",
        "request_id": "dreamer-init",
        "request": { "subtype": "initialize" },
    }));
    let checked = proc
        .until("`initialize`", &mut early, |v| {
            v["type"] == "control_response" && v["response"]["request_id"] == "dreamer-init"
        })
        .await
        .and_then(|r| match r["response"]["subtype"].as_str() {
            Some("success") => Ok(()),
            _ => Err(format!(
                "it refused `initialize`: {}",
                r["response"]["error"].as_str().unwrap_or("no reason given")
            )),
        });
    if let Err(why) = checked {
        return Err(proc.fail(version.as_deref(), why).await);
    }
    let (updates_tx, updates) = mpsc::unbounded_channel();
    let pid = proc.pid;
    let mut state = Claude::new(spec.resume.clone());
    let control = proc.run(updates_tx, move |mut out, mut inputs, updates| async move {
        let apply = |o: Out| {
            for w in o.writes {
                let _ = tx.send(w);
            }
            for u in o.updates {
                let _ = updates.send(u);
            }
            if o.stop {
                // A metered login: nothing more reaches the model.
                let _ = platform::terminate_group(pid, true);
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
        session: spec.resume.clone().map(|r| (r, None)),
        control,
        updates,
    })
}

/// What one record or input means.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub updates: Vec<Update>,
    pub writes: Vec<Value>,
    /// End the process now.
    pub stop: bool,
}

/// The client side of a Claude Code session. Pure, so recorded lines test
/// it.
#[derive(Debug, Default)]
pub struct Claude {
    session: Option<String>,
    /// Prompts sent whose `result` has not come.
    running: u32,
    interrupted: bool,
    /// Text or thinking deltas came for the current message.
    streamed: bool,
    /// Permission requests waiting: their tool input, by request id.
    asks: HashMap<String, Value>,
    tools: HashMap<String, String>,
    announced: HashSet<String>,
    next: u64,
}

fn tool_output(c: &Value) -> String {
    let text = match &c["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    crate::clip(&text)
}

impl Claude {
    pub fn new(session: Option<String>) -> Claude {
        Claude {
            session,
            ..Claude::default()
        }
    }

    pub fn input(&mut self, i: Input) -> Out {
        let mut out = Out::default();
        match i {
            Input::Prompt(text) => {
                self.running += 1;
                out.writes.push(json!({
                    "type": "user",
                    "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
                    "parent_tool_use_id": null,
                    "session_id": self.session.clone().unwrap_or_default(),
                }));
            }
            Input::Cancel => {
                if self.running > 0 {
                    self.interrupted = true;
                    self.next += 1;
                    out.writes.push(json!({
                        "type": "control_request",
                        "request_id": format!("dreamer-{}", self.next),
                        "request": { "subtype": "interrupt" },
                    }));
                }
            }
            Input::Answer { id, allow, .. } => {
                let Some(input) = self.asks.remove(&id) else {
                    out.updates.push(Update::Error(format!(
                        "no approval request {id} is waiting"
                    )));
                    return out;
                };
                let response = if allow {
                    json!({ "behavior": "allow", "updatedInput": input })
                } else {
                    json!({ "behavior": "deny", "message": "Denied in Dreamer." })
                };
                out.writes.push(json!({
                    "type": "control_response",
                    "response": { "subtype": "success", "request_id": id, "response": response },
                }));
            }
        }
        out
    }

    fn started(&mut self, c: &Value, out: &mut Out) {
        let id = c["id"].as_str().unwrap_or_default().to_owned();
        let name = c["name"].as_str().unwrap_or_default().to_owned();
        if !self.announced.insert(id.clone()) {
            return;
        }
        self.tools.insert(id.clone(), name.clone());
        if name == "TodoWrite" {
            out.updates.push(Update::Plan(
                c["input"]["todos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| PlanEntry {
                        content: t["content"].as_str().unwrap_or_default().to_owned(),
                        status: t["status"].as_str().unwrap_or("pending").to_owned(),
                        priority: None,
                    })
                    .collect(),
            ));
        }
        out.updates.push(Update::Tool(ToolCall {
            id,
            name,
            status: ToolStatus::Started,
            input: Some(c["input"].clone()),
            output: None,
        }));
    }

    pub fn message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        match v["type"].as_str().unwrap_or_default() {
            "system" if v["subtype"] == "init" => {
                let source = v["apiKeySource"].as_str().unwrap_or("none");
                if source != "none" {
                    out.updates.push(Update::Error(format!(
                        "Claude Code is using an API key ({source}), which would be metered; the session was stopped. Log Claude Code in to your subscription and remove the key."
                    )));
                    out.updates.push(Update::TurnEnd(StopReason::Error));
                    out.stop = true;
                    return out;
                }
                if let Some(id) = v["session_id"].as_str()
                    && self.session.as_deref() != Some(id)
                {
                    self.session = Some(id.to_owned());
                    out.updates.push(Update::HarnessSession {
                        id: id.to_owned(),
                        file: None,
                    });
                }
            }
            "stream_event" => {
                let e = &v["event"];
                match e["type"].as_str() {
                    Some("message_start") => self.streamed = false,
                    Some("content_block_delta") => {
                        let d = &e["delta"];
                        match d["type"].as_str() {
                            Some("text_delta") => {
                                self.streamed = true;
                                out.updates.push(Update::Text(
                                    d["text"].as_str().unwrap_or_default().into(),
                                ));
                            }
                            Some("thinking_delta") => {
                                self.streamed = true;
                                out.updates.push(Update::Thinking(
                                    d["thinking"].as_str().unwrap_or_default().into(),
                                ));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") if !self.streamed => {
                            out.updates
                                .push(Update::Text(c["text"].as_str().unwrap_or_default().into()));
                        }
                        Some("thinking") if !self.streamed => out.updates.push(Update::Thinking(
                            c["thinking"].as_str().unwrap_or_default().into(),
                        )),
                        Some("tool_use") => self.started(c, &mut out),
                        _ => {}
                    }
                }
            }
            "user" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] != "tool_result" {
                        continue;
                    }
                    let id = c["tool_use_id"].as_str().unwrap_or_default().to_owned();
                    out.updates.push(Update::Tool(ToolCall {
                        name: self.tools.get(&id).cloned().unwrap_or_default(),
                        id,
                        status: if c["is_error"] == true {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Finished
                        },
                        input: None,
                        output: Some(tool_output(c)),
                    }));
                }
            }
            "result" => {
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                if u.is_object() {
                    out.updates.push(Update::Usage(Usage {
                        input: n("input_tokens"),
                        output: n("output_tokens"),
                        cache_read: n("cache_read_input_tokens"),
                        cache_write: n("cache_creation_input_tokens"),
                        cost_usd: v["total_cost_usd"].as_f64(),
                    }));
                }
                let failed = v["is_error"] == true || v["subtype"] != "success";
                let reason = if std::mem::take(&mut self.interrupted) {
                    StopReason::Cancelled
                } else if failed {
                    let why = v["result"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or(v["subtype"].as_str())
                        .unwrap_or("the turn failed");
                    out.updates.push(Update::Error(why.to_owned()));
                    StopReason::Error
                } else {
                    StopReason::EndTurn
                };
                self.running = self.running.saturating_sub(1);
                self.tools.clear();
                self.announced.clear();
                self.asks.clear();
                out.updates.push(Update::TurnEnd(reason));
            }
            "control_request" => {
                let id = v["request_id"].as_str().unwrap_or_default().to_owned();
                let r = &v["request"];
                if r["subtype"] == "can_use_tool" {
                    let tool = r["tool_name"].as_str().unwrap_or("a tool");
                    out.updates.push(Update::Approval(ApprovalRequest {
                        id: id.clone(),
                        title: format!("Allow {tool}?"),
                        message: r["description"]
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| Some(crate::clip(&r["input"].to_string()))),
                        options: Vec::new(),
                        timeout_ms: None,
                    }));
                    self.asks.insert(id, r["input"].clone());
                } else {
                    // Hooks and SDK MCP servers: Dreamer registers none.
                    out.writes.push(json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": id,
                            "error": format!("Dreamer does not handle `{}`", r["subtype"].as_str().unwrap_or("?")),
                        },
                    }));
                }
            }
            "control_cancel_request" => {
                if let Some(id) = v["request_id"].as_str() {
                    self.asks.remove(id);
                }
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(c: &mut Claude, lines: &str) -> Out {
        let mut all = Out::default();
        for l in lines.lines().filter(|l| !l.trim().is_empty()) {
            let o = c.message(&serde_json::from_str(l).expect(l));
            all.updates.extend(o.updates);
            all.writes.extend(o.writes);
            all.stop |= o.stop;
        }
        all
    }

    #[test]
    fn a_turn_maps_to_deltas_a_tool_call_an_approval_and_its_end() {
        let mut c = Claude::default();
        let o = c.input(Input::Prompt("list".into()));
        assert_eq!(o.writes[0]["type"], "user");
        assert_eq!(o.writes[0]["message"]["content"][0]["text"], "list");
        let o = feed(
            &mut c,
            r#"
{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none","model":"x"}
{"type":"stream_event","event":{"type":"message_start"}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me look."}}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"tu1"}}
"#,
        );
        assert_eq!(
            o.updates,
            vec![
                Update::HarnessSession {
                    id: "s-1".into(),
                    file: None
                },
                Update::Thinking("hm".into()),
                Update::Text("Let me look.".into()),
                Update::Tool(ToolCall {
                    id: "tu1".into(),
                    name: "Bash".into(),
                    status: ToolStatus::Started,
                    input: Some(json!({"command":"ls"})),
                    output: None,
                }),
                Update::Approval(ApprovalRequest {
                    id: "req-1".into(),
                    title: "Allow Bash?".into(),
                    message: Some(r#"{"command":"ls"}"#.into()),
                    options: vec![],
                    timeout_ms: None,
                }),
            ]
        );
        let o = c.input(Input::Answer {
            id: "req-1".into(),
            allow: true,
            option: None,
        });
        assert_eq!(o.writes[0]["response"]["request_id"], "req-1");
        assert_eq!(o.writes[0]["response"]["response"]["behavior"], "allow");
        assert_eq!(
            o.writes[0]["response"]["response"]["updatedInput"]["command"],
            "ls"
        );
        let o = feed(
            &mut c,
            r#"
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"a.txt"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"Done","usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5},"total_cost_usd":0.01}
"#,
        );
        assert!(
            matches!(&o.updates[0], Update::Tool(t) if t.status == ToolStatus::Finished && t.name == "Bash" && t.output.as_deref() == Some("a.txt"))
        );
        assert!(matches!(&o.updates[1], Update::Usage(u) if u.output == 4 && u.cache_read == 5));
        assert_eq!(o.updates[2], Update::TurnEnd(StopReason::EndTurn));
    }

    #[test]
    fn a_metered_login_stops_and_an_interrupt_ends_the_turn_cancelled() {
        let mut c = Claude::default();
        let o = feed(
            &mut c,
            r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"ANTHROPIC_API_KEY"}"#,
        );
        assert!(o.stop);
        assert!(matches!(&o.updates[0], Update::Error(e) if e.contains("metered")));

        let mut c = Claude::default();
        c.input(Input::Prompt("slow".into()));
        let o = c.input(Input::Cancel);
        assert_eq!(o.writes[0]["request"]["subtype"], "interrupt");
        let o = feed(
            &mut c,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{}}"#,
        );
        assert_eq!(
            o.updates.last(),
            Some(&Update::TurnEnd(StopReason::Cancelled))
        );
        let o = feed(
            &mut c,
            r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}}"#,
        );
        assert_eq!(o.writes[0]["response"]["subtype"], "error");
    }
}
