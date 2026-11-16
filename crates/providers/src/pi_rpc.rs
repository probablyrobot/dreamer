//! pi's RPC mode (`pi --mode rpc`) as an agent session.
//!
//! pi reads one JSON command per line on standard input and writes
//! responses (`type: "response"`, matched by `id`) and session events as
//! JSON lines on standard output. Records are split on LF only: U+2028 and
//! U+2029 are valid inside JSON strings. Standard error is diagnostics.
//!
//! What the adapter relies on, from pi's RPC reference:
//!
//! | Direction | Record | Use |
//! | --- | --- | --- |
//! | in | `get_state` | the start check; the harness's own session id and file |
//! | in | `abort` | the start check; cancelling a turn |
//! | in | `prompt` (`streamingBehavior: followUp` while a run is going) | a prompt |
//! | in | `extension_ui_response` | answering a `confirm` or `select` |
//! | out | `message_update` (`text_delta`, `thinking_delta`, `toolcall_end`) | text, thinking, a tool call's input |
//! | out | `tool_execution_start`, `_update`, `_end` | a tool call's progress and output |
//! | out | `message_end` | usage, a provider error |
//! | out | `agent_start`, `agent_settled` | a turn's start and end |
//! | out | `auto_retry_end`, `extension_error`, `compaction_end` | errors |
//! | out | `extension_ui_request` (`confirm`, `select`) | approval requests |
//!
//! At start the adapter sends `get_state` and `abort` and refuses a pi
//! whose answers lack what it needs, saying which. A resumed session is
//! started with `--session <file or id>`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::session::{
    ApprovalRequest, Control, Harness, Input, SCRUBBED, Spec, StopReason, ToolCall, ToolStatus,
    Update, Usage,
};

/// How long pi has to answer the start check.
const START: Duration = Duration::from_secs(30);
/// How long pi has to end after its input closes, and after SIGTERM.
const GRACE: Duration = Duration::from_secs(3);
/// Standard error lines kept for a failure's reason.
const STDERR_LINES: usize = 12;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// Start `pi --mode rpc` for a session and check its RPC surface. Must be
/// called inside a Tokio runtime.
pub async fn start(spec: &Spec) -> Result<Harness, String> {
    let version = version(spec).await;
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args).args(["--mode", "rpc"]);
    if let Some(m) = &spec.model {
        cmd.args(["--model", m]);
    }
    if let Some(r) = &spec.resume {
        cmd.args(["--session", r]);
    }
    cmd.current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        // Dreamer manages pi's updates.
        .env("PI_SKIP_VERSION_CHECK", "1");
    if let Some(d) = &spec.agent_dir {
        cmd.env("PI_CODING_AGENT_DIR", d);
    }
    for v in SCRUBBED {
        cmd.env_remove(v);
    }
    let mut child =
        crate::spawn(&mut cmd).map_err(|e| format!("{}: {e}", spec.program.display()))?;
    let pid = child.id().unwrap_or_default();
    let stdin = child.stdin.take().ok_or("pi has no standard input")?;
    let stdout = child.stdout.take().ok_or("pi has no standard output")?;
    let stderr = child.stderr.take();

    let (updates_tx, updates) = mpsc::unbounded_channel();
    let (input, mut inputs) = mpsc::unbounded_channel::<Input>();
    let (close, closed) = oneshot::channel::<()>();
    let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<Value>();
    let pending: Pending = Arc::default();
    let busy = Arc::new(AtomicBool::new(false));
    let dialogs: Arc<Mutex<HashMap<String, Dialog>>> = Arc::default();
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let next_id = Arc::new(AtomicU64::new(1));

    // Standard error: kept for a failure's reason.
    if let Some(e) = stderr {
        let tail = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(e).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if let Ok(mut t) = tail.lock() {
                    if t.len() == STDERR_LINES {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            }
        });
    }

    // The writer owns standard input; it closes when the session closes.
    let (stop_writer, mut writer_stopped) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut stdin = stdin;
        loop {
            tokio::select! {
                v = lines_rx.recv() => {
                    let Some(v) = v else { break };
                    let mut line = v.to_string();
                    line.push('\n');
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                _ = &mut writer_stopped => break,
            }
        }
        // Dropping standard input asks pi for an orderly shutdown.
    });

    // The reader: responses to their waiters, events through the mapper.
    let reader = {
        let (pending, busy, dialogs, updates_tx, lines_tx) = (
            pending.clone(),
            busy.clone(),
            dialogs.clone(),
            updates_tx.clone(),
            lines_tx.clone(),
        );
        tokio::spawn(async move {
            let mut mapper = Mapper::default();
            let mut out = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match out.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let Ok(v) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                if v["type"] == "response" {
                    let waiter = v["id"]
                        .as_str()
                        .and_then(|id| pending.lock().ok()?.remove(id));
                    match waiter {
                        Some(w) => {
                            let _ = w.send(v);
                        }
                        None => {
                            // A prompt that started no run leaves pi idle.
                            if v["command"] == "prompt"
                                && (v["success"] != true || v["data"]["disposition"] == "handled")
                            {
                                busy.store(false, Ordering::SeqCst);
                            }
                            for u in mapper.response(&v) {
                                let _ = updates_tx.send(u);
                            }
                        }
                    }
                    continue;
                }
                // The writer marks a run as going when it sends a prompt;
                // pi's own events correct it.
                let before = mapper.busy;
                let mapped = mapper.event(&v);
                if mapper.busy != before {
                    busy.store(mapper.busy, Ordering::SeqCst);
                }
                if let Ok(mut d) = dialogs.lock() {
                    d.extend(mapped.dialogs);
                }
                if let Some(r) = mapped.reply {
                    let _ = lines_tx.send(r);
                }
                for u in mapped.updates {
                    let _ = updates_tx.send(u);
                }
            }
        })
    };

    // The start check.
    let ask = |command: &'static str| {
        let id = format!("dreamer-{}", next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = pending.lock() {
            p.insert(id.clone(), tx);
        }
        let _ = lines_tx.send(json!({ "id": id, "type": command }));
        rx
    };
    let checked = async {
        let state = tokio::time::timeout(START, ask("get_state"))
            .await
            .map_err(|_| "pi did not answer `get_state` in time".to_owned())?
            .map_err(|_| "pi ended before answering `get_state`".to_owned())?;
        let session = surface("get_state", &state)?;
        let abort = tokio::time::timeout(START, ask("abort"))
            .await
            .map_err(|_| "pi did not answer `abort` in time".to_owned())?
            .map_err(|_| "pi ended before answering `abort`".to_owned())?;
        surface("abort", &abort)?;
        Ok::<_, String>(session)
    };
    let session = match checked.await {
        Ok(s) => s,
        Err(why) => {
            let _ = platform::terminate_group(pid, true);
            let _ = child.wait().await;
            let stderr = tail
                .lock()
                .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            let v = version.as_deref().unwrap_or("of unknown version");
            let mut msg = format!("pi {v} cannot run a Dreamer session: {why}");
            if !stderr.trim().is_empty() {
                msg.push_str(&format!(" ({})", stderr.trim()));
            }
            return Err(msg);
        }
    };
    if let Some((id, file)) = &session {
        let _ = updates_tx.send(Update::HarnessSession {
            id: id.clone(),
            file: file.clone(),
        });
    }

    // Inputs become commands.
    {
        let (busy, dialogs, lines_tx, updates_tx, next_id) = (
            busy.clone(),
            dialogs.clone(),
            lines_tx.clone(),
            updates_tx.clone(),
            next_id.clone(),
        );
        tokio::spawn(async move {
            while let Some(i) = inputs.recv().await {
                let id = format!("dreamer-{}", next_id.fetch_add(1, Ordering::Relaxed));
                let line = match i {
                    Input::Prompt(text) => {
                        let mut c = json!({ "id": id, "type": "prompt", "message": text });
                        // A run is going: the prompt waits for it to end.
                        if busy.swap(true, Ordering::SeqCst) {
                            c["streamingBehavior"] = json!("followUp");
                        }
                        c
                    }
                    Input::Cancel => json!({ "id": id, "type": "abort" }),
                    Input::Answer {
                        id: dialog,
                        allow,
                        option,
                    } => {
                        let kind = dialogs.lock().ok().and_then(|mut d| d.remove(&dialog));
                        match kind {
                            Some(k) => answer(&dialog, k, allow, option),
                            None => {
                                let _ = updates_tx.send(Update::Error(format!(
                                    "no approval request {dialog} is waiting"
                                )));
                                continue;
                            }
                        }
                    }
                };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
    }

    // The supervisor: waits for the process, or for a close.
    tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => Some(s),
            _ = closed => None,
        };
        let status = match status {
            Some(s) => s,
            None => {
                let _ = stop_writer.send(());
                match tokio::time::timeout(GRACE, child.wait()).await {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = platform::terminate_group(pid, false);
                        match tokio::time::timeout(GRACE, child.wait()).await {
                            Ok(s) => s,
                            Err(_) => {
                                let _ = platform::terminate_group(pid, true);
                                child.wait().await
                            }
                        }
                    }
                }
            }
        };
        // Whatever it started in its group goes with it.
        let _ = platform::terminate_group(pid, true);
        let _ = reader.await;
        let stderr = tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let mut why = match status {
            Ok(s) => format!("pi exited ({s})"),
            Err(e) => format!("pi: {e}"),
        };
        if !stderr.trim().is_empty() {
            why.push_str(&format!(": {}", crate::clip(stderr.trim())));
        }
        let _ = updates_tx.send(Update::Exited(why));
    });

    Ok(Harness {
        pid,
        version,
        session,
        control: Control {
            input,
            close: Some(close),
        },
        updates,
    })
}

/// What `pi --version` says, if it answers quickly.
async fn version(spec: &Spec) -> Option<String> {
    let mut c = tokio::process::Command::new(&spec.program);
    c.args(&spec.args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("PI_SKIP_VERSION_CHECK", "1")
        .kill_on_drop(true);
    if let Some(d) = &spec.agent_dir {
        c.env("PI_CODING_AGENT_DIR", d);
    }
    for v in SCRUBBED {
        c.env_remove(v);
    }
    let child = crate::spawn(&mut c).ok()?;
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Check a start-check answer; for `get_state`, the session id and file.
pub fn surface(command: &str, v: &Value) -> Result<Option<(String, Option<String>)>, String> {
    if v["success"] != true {
        let why = v["error"].as_str().unwrap_or("no reason given");
        return Err(format!(
            "it does not support the RPC command `{command}` ({why})"
        ));
    }
    if command != "get_state" {
        return Ok(None);
    }
    let id = v["data"]["sessionId"].as_str().ok_or(
        "its `get_state` answer has no `sessionId`, which Dreamer keeps to resume the session",
    )?;
    Ok(Some((
        id.to_owned(),
        v["data"]["sessionFile"].as_str().map(str::to_owned),
    )))
}

/// A dialog pi waits on, by its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Confirm,
    Select(Vec<String>),
}

/// The `extension_ui_response` for an approval answer.
pub fn answer(id: &str, kind: Dialog, allow: bool, option: Option<String>) -> Value {
    match (kind, allow) {
        (Dialog::Confirm, allow) => {
            json!({ "type": "extension_ui_response", "id": id, "confirmed": allow })
        }
        (Dialog::Select(options), true) => {
            let value = option
                .filter(|o| options.contains(o))
                .or_else(|| options.first().cloned());
            match value {
                Some(v) => json!({ "type": "extension_ui_response", "id": id, "value": v }),
                None => json!({ "type": "extension_ui_response", "id": id, "cancelled": true }),
            }
        }
        (Dialog::Select(_), false) => {
            json!({ "type": "extension_ui_response", "id": id, "cancelled": true })
        }
    }
}

/// What one of pi's records means for the session.
#[derive(Debug, Default, PartialEq)]
pub struct Mapped {
    pub updates: Vec<Update>,
    /// A record to write back at once (a dialog Dreamer cannot show).
    pub reply: Option<Value>,
    /// Dialogs now waiting for an answer.
    pub dialogs: Vec<(String, Dialog)>,
}

/// Turns pi's events into session updates. Pure, so recorded lines test it.
#[derive(Debug, Default)]
pub struct Mapper {
    /// A run is going (from `agent_start` until `agent_settled`).
    pub busy: bool,
    /// Tool calls already announced as started.
    tools: HashSet<String>,
    /// The running turn reported an error.
    failed: bool,
}

/// A tool result's text: its text blocks, else the JSON.
fn result_text(v: &Value) -> Option<String> {
    if v.is_null() {
        return None;
    }
    let texts: Vec<&str> = v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    Some(crate::clip(&if texts.is_empty() {
        v.to_string()
    } else {
        texts.join("\n")
    }))
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

impl Mapper {
    /// A response nobody waits for: a refused command is an error.
    pub fn response(&mut self, v: &Value) -> Vec<Update> {
        let command = v["command"].as_str().unwrap_or("?");
        if v["success"] != true {
            if command == "prompt" {
                self.busy = false;
            }
            let why = v["error"].as_str().unwrap_or("no reason given");
            return vec![Update::Error(format!("pi refused `{command}`: {why}"))];
        }
        // An extension command consumed the prompt: no run starts.
        if command == "prompt" && v["data"]["disposition"] == "handled" {
            self.busy = false;
            return vec![Update::TurnEnd(StopReason::EndTurn)];
        }
        Vec::new()
    }

    fn started(&mut self, id: &str, name: &str, input: &Value) -> Option<Update> {
        self.tools.insert(id.to_owned()).then(|| {
            Update::Tool(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                status: ToolStatus::Started,
                input: (!input.is_null()).then(|| input.clone()),
                output: None,
            })
        })
    }

    fn error(&mut self, why: String) -> Update {
        self.failed = true;
        Update::Error(why)
    }

    pub fn event(&mut self, v: &Value) -> Mapped {
        let mut m = Mapped::default();
        match v["type"].as_str().unwrap_or_default() {
            "agent_start" => self.busy = true,
            "agent_settled" => {
                self.busy = false;
                self.tools.clear();
                let reason = if v["aborted"] == true {
                    StopReason::Cancelled
                } else if std::mem::take(&mut self.failed) {
                    StopReason::Error
                } else {
                    StopReason::EndTurn
                };
                self.failed = false;
                m.updates.push(Update::TurnEnd(reason));
            }
            "message_update" => {
                let e = &v["assistantMessageEvent"];
                match e["type"].as_str().unwrap_or_default() {
                    "text_delta" => m.updates.push(Update::Text(text(&e["delta"]))),
                    "thinking_delta" => m.updates.push(Update::Thinking(text(&e["delta"]))),
                    "toolcall_end" => {
                        let c = &e["toolCall"];
                        if let Some(u) = self.started(
                            c["id"].as_str().unwrap_or_default(),
                            c["name"].as_str().unwrap_or_default(),
                            &c["arguments"],
                        ) {
                            m.updates.push(u);
                        }
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                if let Some(u) = self.started(
                    v["toolCallId"].as_str().unwrap_or_default(),
                    v["toolName"].as_str().unwrap_or_default(),
                    &v["args"],
                ) {
                    m.updates.push(u);
                }
            }
            "tool_execution_update" => m.updates.push(Update::Tool(ToolCall {
                id: text(&v["toolCallId"]),
                name: text(&v["toolName"]),
                status: ToolStatus::Updated,
                input: None,
                output: result_text(&v["partialResult"]),
            })),
            "tool_execution_end" => {
                let id = text(&v["toolCallId"]);
                self.tools.remove(&id);
                m.updates.push(Update::Tool(ToolCall {
                    id,
                    name: text(&v["toolName"]),
                    status: if v["isError"] == true {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Finished
                    },
                    input: None,
                    output: result_text(&v["result"]),
                }));
            }
            "message_end" if v["message"]["role"] == "assistant" => {
                let msg = &v["message"];
                let u = &msg["usage"];
                if u.is_object() {
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    m.updates.push(Update::Usage(Usage {
                        input: n("input"),
                        output: n("output"),
                        cache_read: n("cacheRead"),
                        cache_write: n("cacheWrite"),
                        cost_usd: u["cost"]["total"].as_f64(),
                    }));
                }
                if msg["stopReason"] == "error" {
                    let why = msg["errorMessage"]
                        .as_str()
                        .unwrap_or("the model provider reported an error");
                    m.updates.push(self.error(why.to_owned()));
                }
            }
            "auto_retry_end" if v["success"] == false => {
                let why = v["finalError"].as_str().unwrap_or("retries failed");
                m.updates.push(self.error(why.to_owned()));
            }
            "compaction_end" if v["errorMessage"].is_string() => {
                m.updates
                    .push(self.error(format!("compaction failed: {}", text(&v["errorMessage"]))));
            }
            "extension_error" => {
                m.updates.push(self.error(format!(
                    "pi extension {}: {}",
                    text(&v["extensionPath"]),
                    text(&v["error"])
                )));
            }
            "extension_ui_request" => {
                let id = text(&v["id"]);
                let request = |options: Vec<String>| {
                    Update::Approval(ApprovalRequest {
                        id: id.clone(),
                        title: text(&v["title"]),
                        message: v["message"].as_str().map(str::to_owned),
                        options,
                        timeout_ms: v["timeout"].as_u64(),
                    })
                };
                match v["method"].as_str().unwrap_or_default() {
                    "confirm" => {
                        m.updates.push(request(Vec::new()));
                        m.dialogs.push((id.clone(), Dialog::Confirm));
                    }
                    "select" => {
                        let options: Vec<String> = v["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|o| o.as_str().map(str::to_owned))
                            .collect();
                        m.updates.push(request(options.clone()));
                        m.dialogs.push((id.clone(), Dialog::Select(options)));
                    }
                    // Free text has no place in a session: dismissed.
                    "input" | "editor" => {
                        m.reply = Some(json!({
                            "type": "extension_ui_response", "id": id, "cancelled": true
                        }));
                    }
                    // Notifications, status, widgets, titles: not events.
                    _ => {}
                }
            }
            _ => {}
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(lines: &str) -> (Vec<Update>, Mapper) {
        let mut mapper = Mapper::default();
        let mut out = Vec::new();
        for l in lines.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(l).expect(l);
            if v["type"] == "response" {
                out.extend(mapper.response(&v));
            } else {
                out.extend(mapper.event(&v).updates);
            }
        }
        (out, mapper)
    }

    // Recorded from pi's documented event stream (json.md and rpc.md,
    // pi 1.5.2): a turn with thinking, text, and one bash tool call.
    const TURN: &str = r#"
{"id":"dreamer-3","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"user","content":"List files","timestamp":1733234401000}}
{"type":"message_end","message":{"role":"user","content":"List files","timestamp":1733234401000}}
{"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"pending"}}
{"type":"message_update","usage":{"input":100,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":101,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"The user wants a listing."}}
{"type":"message_update","usage":{"input":100,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":102,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"Let me look."}}
{"type":"message_update","usage":{"input":100,"output":3,"cacheRead":0,"cacheWrite":0,"totalTokens":103,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"toolcall_start","contentIndex":2,"id":"call_abc123","toolName":"bash"}}
{"type":"message_update","usage":{"input":100,"output":9,"cacheRead":0,"cacheWrite":0,"totalTokens":109,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"toolcall_delta","contentIndex":2,"delta":"{\"command\":\"ls"}}
{"type":"message_update","usage":{"input":100,"output":12,"cacheRead":0,"cacheWrite":0,"totalTokens":112,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"toolcall_end","contentIndex":2,"toolCall":{"type":"toolCall","id":"call_abc123","name":"bash","arguments":{"command":"ls -la"}}}}
{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"toolUse","usage":{"input":100,"output":12,"cacheRead":50,"cacheWrite":0,"totalTokens":162,"cost":{"input":0.0003,"output":0.0002,"cacheRead":0,"cacheWrite":0,"total":0.0005}}}}
{"type":"tool_execution_start","toolCallId":"call_abc123","toolName":"bash","args":{"command":"ls -la"}}
{"type":"tool_execution_update","toolCallId":"call_abc123","toolName":"bash","args":{"command":"ls -la"},"partialResult":{"content":[{"type":"text","text":"partial output"}],"details":{}}}
{"type":"tool_execution_end","toolCallId":"call_abc123","toolName":"bash","result":{"content":[{"type":"text","text":"complete output"}],"details":{}},"isError":false,"durationMs":12}
{"type":"turn_end","message":{"role":"assistant"},"toolResults":[]}
{"type":"agent_end","messages":[],"willRetry":false}
{"type":"agent_settled","aborted":false}
"#;

    #[test]
    fn a_turn_maps_to_deltas_one_tool_call_usage_and_its_end() {
        let (u, mapper) = feed(TURN);
        assert!(!mapper.busy);
        let started = Update::Tool(ToolCall {
            id: "call_abc123".into(),
            name: "bash".into(),
            status: ToolStatus::Started,
            input: Some(json!({ "command": "ls -la" })),
            output: None,
        });
        assert_eq!(
            u,
            vec![
                Update::Thinking("The user wants\u{2028}a listing.".into()),
                Update::Text("Let me look.".into()),
                started,
                Update::Usage(Usage {
                    input: 100,
                    output: 12,
                    cache_read: 50,
                    cache_write: 0,
                    cost_usd: Some(0.0005),
                }),
                Update::Tool(ToolCall {
                    id: "call_abc123".into(),
                    name: "bash".into(),
                    status: ToolStatus::Updated,
                    input: None,
                    output: Some("partial output".into()),
                }),
                Update::Tool(ToolCall {
                    id: "call_abc123".into(),
                    name: "bash".into(),
                    status: ToolStatus::Finished,
                    input: None,
                    output: Some("complete output".into()),
                }),
                Update::TurnEnd(StopReason::EndTurn),
            ],
            "the tool call is announced once, though pi reports it twice"
        );
    }

    #[test]
    fn an_abort_a_provider_error_and_a_refused_prompt_end_turns_as_such() {
        let (u, _) = feed(
            r#"
{"type":"agent_start"}
{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Work"}}
{"type":"message_end","message":{"role":"assistant","stopReason":"aborted","errorMessage":"Request was aborted"}}
{"type":"agent_end","messages":[],"willRetry":false}
{"type":"agent_settled","aborted":true}
{"type":"agent_start"}
{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"429 rate limited"}}
{"type":"auto_retry_end","success":false,"attempt":3,"finalError":"429 rate limited"}
{"type":"agent_settled","aborted":false}
{"id":"dreamer-9","type":"response","command":"prompt","success":false,"error":"Agent is streaming"}
{"id":"dreamer-10","type":"response","command":"prompt","success":true,"data":{"disposition":"handled"}}
"#,
        );
        assert_eq!(
            u,
            vec![
                Update::Text("Work".into()),
                Update::TurnEnd(StopReason::Cancelled),
                Update::Error("429 rate limited".into()),
                Update::Error("429 rate limited".into()),
                Update::TurnEnd(StopReason::Error),
                Update::Error("pi refused `prompt`: Agent is streaming".into()),
                Update::TurnEnd(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn dialogs_become_approvals_and_free_text_ones_are_dismissed() {
        let mut m = Mapper::default();
        let confirm = m.event(&json!({
            "type": "extension_ui_request", "id": "uuid-2", "method": "confirm",
            "title": "Allow bash?", "message": "rm -rf build", "timeout": 5000
        }));
        assert_eq!(
            confirm.updates,
            vec![Update::Approval(ApprovalRequest {
                id: "uuid-2".into(),
                title: "Allow bash?".into(),
                message: Some("rm -rf build".into()),
                options: vec![],
                timeout_ms: Some(5000),
            })]
        );
        assert_eq!(confirm.dialogs, vec![("uuid-2".into(), Dialog::Confirm)]);
        let select = m.event(&json!({
            "type": "extension_ui_request", "id": "uuid-1", "method": "select",
            "title": "Allow dangerous command?", "options": ["Allow", "Block"]
        }));
        assert!(
            matches!(&select.updates[0], Update::Approval(a) if a.options == ["Allow", "Block"])
        );
        let input = m.event(&json!({
            "type": "extension_ui_request", "id": "uuid-3", "method": "input", "title": "Enter a value"
        }));
        assert!(input.updates.is_empty());
        assert_eq!(
            input.reply.as_ref().map(|r| &r["cancelled"]),
            Some(&json!(true))
        );
        let notify = m.event(&json!({
            "type": "extension_ui_request", "id": "uuid-5", "method": "notify", "message": "hi"
        }));
        assert_eq!(notify, Mapped::default());

        assert_eq!(
            answer("uuid-2", Dialog::Confirm, false, None),
            json!({ "type": "extension_ui_response", "id": "uuid-2", "confirmed": false })
        );
        let opts = Dialog::Select(vec!["Allow".into(), "Block".into()]);
        assert_eq!(
            answer("uuid-1", opts.clone(), true, Some("Block".into()))["value"],
            "Block"
        );
        assert_eq!(answer("uuid-1", opts.clone(), true, None)["value"], "Allow");
        assert_eq!(answer("uuid-1", opts, false, None)["cancelled"], true);
    }

    #[test]
    fn the_start_check_names_what_pi_lacks() {
        let ok = json!({"id":"dreamer-1","type":"response","command":"get_state","success":true,
            "data":{"sessionId":"abc123","sessionFile":"/s/abc123.jsonl","isStreaming":false}});
        assert_eq!(
            surface("get_state", &ok).expect("ok"),
            Some(("abc123".into(), Some("/s/abc123.jsonl".into())))
        );
        let unknown = json!({"type":"response","command":"get_state","success":false,
            "error":"Unknown command: get_state"});
        let e = surface("get_state", &unknown).expect_err("refused");
        assert!(
            e.contains("`get_state`") && e.contains("Unknown command"),
            "{e}"
        );
        let no_id = json!({"type":"response","command":"get_state","success":true,"data":{}});
        assert!(
            surface("get_state", &no_id)
                .expect_err("no id")
                .contains("sessionId")
        );
    }
}
