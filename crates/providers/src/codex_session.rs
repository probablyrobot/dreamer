//! Codex as an agent session: one long-lived `codex app-server`, Codex's
//! JSON-RPC interface (one record per line, without the `jsonrpc` member).
//!
//! Built from the app server's protocol definitions (version 2) in the
//! Codex 0.169.0 sources; no real Codex was run. What the adapter uses:
//!
//! | Direction | Method | Use |
//! | --- | --- | --- |
//! | to Codex | `initialize`, then `initialized` | the start check |
//! | to Codex | `account/read` | the login: only a ChatGPT login runs; an API key never does |
//! | to Codex | `thread/start`, `thread/resume` | a new thread, or Codex's own one again |
//! | to Codex | `turn/start`, `turn/interrupt` | a prompt; cancelling it |
//! | from Codex | `item/agentMessage/delta`, `item/reasoning/*Delta` | text and thinking |
//! | from Codex | `item/started`, `item/completed`, `item/commandExecution/outputDelta` | tool calls |
//! | from Codex | `turn/plan/updated`, `thread/tokenUsage/updated` | the plan, usage |
//! | from Codex | `turn/completed`, `error` | the end of a turn, errors |
//! | from Codex | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` | approval requests |
//!
//! Threads run with `approvalPolicy: on-request` and the `workspace-write`
//! sandbox: anything beyond the sandbox asks Dreamer. Dreamer's tools are
//! `dream mcp`, given with `-c mcp_servers.dreamer.*` (Codex gives an MCP
//! server only the environment it is configured with). Codex starts its
//! MCP servers with a thread but does not wait for them before a turn, so
//! a prompt waits until Codex says (`mcpServer/startupStatus/updated`)
//! that `dreamer` is ready or failed, or [`TOOLS_WAIT`] has passed;
//! otherwise an early turn would run without Dreamer's tools (Codex
//! 0.170.0 took about 13 s to start `dream mcp`). A prompt sent while a
//! turn runs waits for that turn to end.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{
    ApprovalRequest, Harness, Input, PlanEntry, Spec, StopReason, ToolCall, ToolStatus, Update,
    Usage,
};
use crate::stdio::{Proc, Rpc, error_text, id_text, is_response};

/// How long a prompt waits for Codex to start Dreamer's tools.
pub const TOOLS_WAIT: Duration = Duration::from_secs(30);

/// A TOML basic string (JSON's escaping is valid TOML).
fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// The arguments after the configured command.
pub fn arguments(spec: &Spec) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(t) = &spec.tools {
        let args: Vec<String> = t.args.iter().map(|x| toml_string(x)).collect();
        let env: Vec<String> = spec
            .env
            .iter()
            .filter(|(k, _)| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .map(|(k, v)| format!("{k}={}", toml_string(v)))
            .collect();
        for kv in [
            format!(
                "mcp_servers.dreamer.command={}",
                toml_string(&t.program.display().to_string())
            ),
            format!("mcp_servers.dreamer.args=[{}]", args.join(",")),
            format!("mcp_servers.dreamer.env={{{}}}", env.join(",")),
            // Otherwise each Dreamer tool call comes as an elicitation,
            // which is declined; the server is already narrowed to the grant.
            crate::codex::APPROVE_DREAMER_TOOLS.to_owned(),
        ] {
            a.push("-c".to_owned());
            a.push(kv);
        }
    }
    a.push("app-server".to_owned());
    a
}

/// Whether Codex's own login may run a turn, from `account/read`.
pub fn login(r: &Value) -> Result<(), String> {
    if let Some(e) = error_text(r) {
        return Err(format!("`account/read` failed: {e}"));
    }
    let a = &r["result"]["account"];
    match a["type"].as_str() {
        Some("chatgpt") => Ok(()),
        Some("apiKey") => Err(
            "Codex is logged in with an API key, which would be metered; log it in with ChatGPT (`codex login`)"
                .to_owned(),
        ),
        Some(other) => Err(format!(
            "Codex uses a `{other}` account; Dreamer runs Codex sessions on a ChatGPT login only"
        )),
        None => Err("Codex is not logged in; run `codex login`".to_owned()),
    }
}

/// Start `codex app-server` for a session. Must be called inside a Tokio
/// runtime.
pub async fn start(spec: &Spec) -> Result<Harness, String> {
    let version = crate::stdio::version(spec).await;
    let mut proc = Proc::spawn("Codex", crate::stdio::command(spec, &arguments(spec)))?;
    let tx = proc.sender();
    let mut rpc = Rpc::new(false);
    let mut early = Vec::new();
    let checked = async {
        let mut call = async |method: &str, params: Value| {
            let (id, req) = rpc.request(method, params);
            let _ = tx.send(req);
            proc.until(&format!("`{method}`"), &mut early, |v| is_response(v, id))
                .await
        };
        let r = call(
            "initialize",
            json!({
                "clientInfo": { "name": "dreamer", "title": "Dreamer", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
        if let Some(e) = error_text(&r) {
            return Err(format!("`initialize` failed: {e}"));
        }
        let _ = tx.send(json!({ "method": "initialized" }));
        login(&call("account/read", json!({})).await?)?;
        let (method, params) = match &spec.resume {
            Some(id) => ("thread/resume", json!({ "threadId": id })),
            None => {
                let mut p = json!({
                    "cwd": spec.cwd.display().to_string(),
                    "approvalPolicy": "on-request",
                    "sandbox": "workspace-write",
                });
                if let Some(m) = &spec.model {
                    p["model"] = json!(m);
                }
                ("thread/start", p)
            }
        };
        let r = call(method, params).await?;
        if let Some(e) = error_text(&r) {
            return Err(format!("`{method}` failed: {e}"));
        }
        r["result"]["thread"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("its `{method}` answer has no thread id"))
    };
    let thread = match checked.await {
        Ok(t) => t,
        Err(why) => return Err(proc.fail(version.as_deref(), why).await),
    };
    let (updates_tx, updates) = mpsc::unbounded_channel();
    let _ = updates_tx.send(Update::HarnessSession {
        id: thread.clone(),
        file: None,
    });
    let pid = proc.pid;
    let mut state = Codex::new(thread.clone(), rpc, spec.tools.is_some());
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
        let wait = tokio::time::sleep(TOOLS_WAIT);
        tokio::pin!(wait);
        let mut waiting = true;
        loop {
            tokio::select! {
                _ = &mut wait, if waiting => {
                    waiting = false;
                    apply(state.tools_wait_over());
                }
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
        session: Some((thread, None)),
        control,
        updates,
    })
}

/// What one record or input means.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub updates: Vec<Update>,
    pub writes: Vec<Value>,
}

/// The client side of one Codex thread. Pure, so recorded records test it.
#[derive(Debug)]
pub struct Codex {
    thread: String,
    rpc: Rpc,
    /// A turn runs (from `turn/start` until `turn/completed`).
    running: bool,
    /// The running turn's id, once Codex has given it.
    turn: Option<String>,
    /// The `turn/start` request waiting for its answer.
    starting: Option<u64>,
    /// Cancel as soon as the turn's id is known.
    cancel: bool,
    queue: VecDeque<String>,
    /// Whether prompts may start: Dreamer's tools are ready or failed, or
    /// there are none, or the wait is over.
    tools_settled: bool,
    /// Approval requests waiting, by the id shown to Dreamer.
    asks: HashMap<String, Value>,
    /// Agent messages that came as deltas.
    streamed: HashSet<String>,
    tools: HashMap<String, String>,
}

/// A tool item's name, input, and output, if the item is a tool call.
fn tool(item: &Value) -> Option<(String, Value, Option<String>, ToolStatus)> {
    let failed = matches!(item["status"].as_str(), Some("failed" | "declined"));
    let done = matches!(
        item["status"].as_str(),
        Some("completed" | "failed" | "declined")
    );
    let status = match (done, failed) {
        (_, true) => ToolStatus::Failed,
        (true, false) => ToolStatus::Finished,
        _ => ToolStatus::Started,
    };
    let (name, input, output) = match item["type"].as_str()? {
        "commandExecution" => (
            "shell".to_owned(),
            json!({ "command": item["command"], "cwd": item["cwd"] }),
            item["aggregatedOutput"].as_str().map(crate::clip),
        ),
        "fileChange" => (
            "edit".to_owned(),
            json!({ "changes": item["changes"] }),
            None,
        ),
        "mcpToolCall" => {
            let text = item["result"]["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .or_else(|| item["error"]["message"].as_str().map(str::to_owned));
            (
                format!(
                    "{}.{}",
                    item["server"].as_str().unwrap_or("mcp"),
                    item["tool"].as_str().unwrap_or("?")
                ),
                item["arguments"].clone(),
                text.map(|t| crate::clip(&t)),
            )
        }
        "webSearch" => (
            "web_search".to_owned(),
            json!({ "query": item["query"] }),
            None,
        ),
        _ => return None,
    };
    Some((name, input, output, status))
}

impl Codex {
    /// `tools`: whether Codex was given Dreamer's tools, whose start
    /// prompts wait for.
    pub(crate) fn new(thread: String, rpc: Rpc, tools: bool) -> Codex {
        Codex {
            tools_settled: !tools,
            thread,
            rpc,
            running: false,
            turn: None,
            starting: None,
            cancel: false,
            queue: VecDeque::new(),
            asks: HashMap::new(),
            streamed: HashSet::new(),
            tools: HashMap::new(),
        }
    }

    fn begin(&mut self, text: String, out: &mut Out) {
        let (id, req) = self.rpc.request(
            "turn/start",
            json!({
                "threadId": self.thread,
                "input": [{ "type": "text", "text": text, "textElements": [] }],
            }),
        );
        self.running = true;
        self.turn = None;
        self.cancel = false;
        self.starting = Some(id);
        out.writes.push(req);
    }

    fn interrupt(&mut self, out: &mut Out) {
        if let Some(t) = &self.turn {
            let (_, req) = self.rpc.request(
                "turn/interrupt",
                json!({ "threadId": self.thread, "turnId": t }),
            );
            out.writes.push(req);
        } else {
            self.cancel = true;
        }
    }

    fn ended(&mut self, reason: StopReason, out: &mut Out) {
        self.running = false;
        self.turn = None;
        self.starting = None;
        self.cancel = false;
        self.asks.clear();
        self.streamed.clear();
        self.tools.clear();
        out.updates.push(Update::TurnEnd(reason));
        if let Some(next) = self.queue.pop_front() {
            self.begin(next, out);
        }
    }

    pub fn input(&mut self, i: Input) -> Out {
        let mut out = Out::default();
        match i {
            Input::Prompt(text) => {
                if self.running || !self.tools_settled {
                    self.queue.push_back(text);
                } else {
                    self.begin(text, &mut out);
                }
            }
            Input::Cancel => {
                self.queue.clear();
                if self.running {
                    for (_, id) in self.asks.drain() {
                        out.writes
                            .push(self.rpc.result(&id, json!({ "decision": "cancel" })));
                    }
                    self.interrupt(&mut out);
                }
            }
            Input::Answer { id, allow, .. } => {
                let Some(rid) = self.asks.remove(&id) else {
                    out.updates.push(Update::Error(format!(
                        "no approval request {id} is waiting"
                    )));
                    return out;
                };
                let decision = if allow { "accept" } else { "decline" };
                out.writes
                    .push(self.rpc.result(&rid, json!({ "decision": decision })));
            }
        }
        out
    }

    /// Dreamer's tools are settled: prompts that waited start.
    fn tools_settled(&mut self, out: &mut Out) {
        if self.tools_settled {
            return;
        }
        self.tools_settled = true;
        if !self.running
            && let Some(next) = self.queue.pop_front()
        {
            self.begin(next, out);
        }
    }

    /// [`TOOLS_WAIT`] passed: prompts go on without waiting longer.
    pub fn tools_wait_over(&mut self) -> Out {
        let mut out = Out::default();
        if !self.tools_settled {
            out.updates.push(Update::Error(
                "Codex did not say whether Dreamer's tools started; the prompt runs without waiting longer"
                    .into(),
            ));
        }
        self.tools_settled(&mut out);
        out
    }

    pub fn message(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let p = &v["params"];
        match (v["method"].as_str(), v.get("id")) {
            (Some("mcpServer/startupStatus/updated"), None) if p["name"] == "dreamer" => {
                match p["status"].as_str() {
                    Some("ready") => self.tools_settled(&mut out),
                    Some("failed") => {
                        let why = p["error"].as_str().unwrap_or("no reason given");
                        out.updates.push(Update::Error(format!(
                            "Codex could not start Dreamer's tools: {why}"
                        )));
                        self.tools_settled(&mut out);
                    }
                    _ => {}
                }
            }
            (None, Some(id)) => {
                if id.as_u64().is_some() && id.as_u64() == self.starting {
                    self.starting = None;
                    if let Some(e) = error_text(v) {
                        out.updates.push(Update::Error(format!("Codex: {e}")));
                        self.ended(StopReason::Error, &mut out);
                    } else if let Some(t) = v["result"]["turn"]["id"].as_str() {
                        self.turn = Some(t.to_owned());
                        if std::mem::take(&mut self.cancel) {
                            self.interrupt(&mut out);
                        }
                    }
                }
            }
            (Some("turn/started"), None) => {
                if let Some(t) = p["turn"]["id"].as_str() {
                    self.turn = Some(t.to_owned());
                    if std::mem::take(&mut self.cancel) {
                        self.interrupt(&mut out);
                    }
                }
            }
            (Some("turn/completed"), None) => {
                let t = &p["turn"];
                let reason = match t["status"].as_str() {
                    Some("interrupted") => StopReason::Cancelled,
                    Some("failed") => {
                        let why = t["error"]["message"].as_str().unwrap_or("the turn failed");
                        out.updates.push(Update::Error(why.to_owned()));
                        StopReason::Error
                    }
                    _ => StopReason::EndTurn,
                };
                self.ended(reason, &mut out);
            }
            (Some("item/agentMessage/delta"), None) => {
                if let Some(i) = p["itemId"].as_str() {
                    self.streamed.insert(i.to_owned());
                }
                out.updates
                    .push(Update::Text(p["delta"].as_str().unwrap_or_default().into()));
            }
            (Some("item/reasoning/textDelta" | "item/reasoning/summaryTextDelta"), None) => {
                out.updates.push(Update::Thinking(
                    p["delta"].as_str().unwrap_or_default().into(),
                ));
            }
            (Some("item/commandExecution/outputDelta"), None) => {
                let id = p["itemId"].as_str().unwrap_or_default().to_owned();
                out.updates.push(Update::Tool(ToolCall {
                    name: self
                        .tools
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "shell".into()),
                    id,
                    status: ToolStatus::Updated,
                    input: None,
                    output: Some(crate::clip(p["delta"].as_str().unwrap_or_default())),
                }));
            }
            (Some("item/started"), None) => {
                let item = &p["item"];
                if let Some((name, input, _, _)) = tool(item) {
                    let id = item["id"].as_str().unwrap_or_default().to_owned();
                    self.tools.insert(id.clone(), name.clone());
                    out.updates.push(Update::Tool(ToolCall {
                        id,
                        name,
                        status: ToolStatus::Started,
                        input: Some(input),
                        output: None,
                    }));
                }
            }
            (Some("item/completed"), None) => {
                let item = &p["item"];
                let id = item["id"].as_str().unwrap_or_default().to_owned();
                if item["type"] == "agentMessage" && !self.streamed.contains(&id) {
                    out.updates.push(Update::Text(
                        item["text"].as_str().unwrap_or_default().into(),
                    ));
                } else if let Some((name, _, output, status)) = tool(item) {
                    let status = if status == ToolStatus::Started {
                        ToolStatus::Finished
                    } else {
                        status
                    };
                    out.updates.push(Update::Tool(ToolCall {
                        id,
                        name,
                        status,
                        input: None,
                        output,
                    }));
                }
            }
            (Some("turn/plan/updated"), None) => out.updates.push(Update::Plan(
                p["plan"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| PlanEntry {
                        content: s["step"].as_str().unwrap_or_default().to_owned(),
                        status: match s["status"].as_str() {
                            Some("inProgress") => "in_progress",
                            Some("completed") => "completed",
                            _ => "pending",
                        }
                        .to_owned(),
                        priority: None,
                    })
                    .collect(),
            )),
            (Some("thread/tokenUsage/updated"), None) => {
                let u = &p["tokenUsage"]["last"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                out.updates.push(Update::Usage(Usage {
                    input: n("inputTokens"),
                    output: n("outputTokens"),
                    cache_read: n("cachedInputTokens"),
                    cache_write: n("cacheWriteInputTokens"),
                    cost_usd: None,
                }));
            }
            (Some("error"), None) if p["willRetry"] != true => {
                out.updates.push(Update::Error(
                    p["error"]["message"]
                        .as_str()
                        .unwrap_or("Codex reported an error")
                        .to_owned(),
                ));
            }
            (
                Some(
                    m @ ("item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"),
                ),
                Some(id),
            ) => {
                let key = id_text(id);
                let (title, message) = if m.starts_with("item/commandExecution") {
                    (
                        "Run a command?".to_owned(),
                        p["command"]
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| p["reason"].as_str().map(str::to_owned)),
                    )
                } else {
                    (
                        "Apply file changes?".to_owned(),
                        p["reason"].as_str().map(str::to_owned),
                    )
                };
                out.updates.push(Update::Approval(ApprovalRequest {
                    id: key.clone(),
                    title,
                    message,
                    options: Vec::new(),
                    timeout_ms: None,
                }));
                self.asks.insert(key, id.clone());
            }
            // More permissions: none granted.
            (Some("item/permissions/requestApproval"), Some(id)) => {
                out.writes
                    .push(self.rpc.result(id, json!({ "permissions": {} })));
            }
            (Some("mcpServer/elicitation/request"), Some(id)) => {
                out.writes
                    .push(self.rpc.result(id, json!({ "action": "decline" })));
            }
            (Some(m), Some(id)) => out.writes.push(self.rpc.error(
                id,
                -32601,
                &format!("Dreamer does not handle `{m}`"),
            )),
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_chatgpt_login_runs() {
        let r = |a: Value| json!({"id":2,"result":{"account":a,"requiresOpenaiAuth":true}});
        assert!(login(&r(json!({"type":"chatgpt","email":null,"planType":"plus"}))).is_ok());
        assert!(
            login(&r(json!({"type":"apiKey"})))
                .expect_err("key")
                .contains("metered")
        );
        assert!(
            login(&r(Value::Null))
                .expect_err("none")
                .contains("codex login")
        );
    }

    #[test]
    fn a_turn_maps_items_to_events_and_approvals_to_decisions() {
        let mut c = Codex::new("thr".into(), Rpc::new(false), false);
        let o = c.input(Input::Prompt("hi".into()));
        assert_eq!(o.writes[0]["method"], "turn/start");
        assert!(o.writes[0].get("jsonrpc").is_none());
        let start = o.writes[0]["id"].clone();
        assert!(c.input(Input::Prompt("later".into())).writes.is_empty());
        let mut got = Vec::new();
        let mut writes = Vec::new();
        for v in [
            json!({"id":start,"result":{"turn":{"id":"t1","items":[],"status":"inProgress"}}}),
            json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"thr","turnId":"t1","itemId":"r1","delta":"hm"}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thr","turnId":"t1","itemId":"m1","delta":"Hi"}}),
            json!({"method":"item/completed","params":{"threadId":"thr","turnId":"t1","item":{"type":"agentMessage","id":"m1","text":"Hi"}}}),
            json!({"method":"item/started","params":{"threadId":"thr","turnId":"t1","item":{"type":"commandExecution","id":"c1","command":"ls","cwd":"/w","status":"inProgress","commandActions":[]}}}),
            json!({"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":"thr","turnId":"t1","itemId":"c1","command":"ls","startedAtMs":1}}),
            json!({"method":"item/completed","params":{"threadId":"thr","turnId":"t1","item":{"type":"commandExecution","id":"c1","command":"ls","cwd":"/w","status":"completed","aggregatedOutput":"a.txt","commandActions":[]}}}),
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thr","turnId":"t1","tokenUsage":{"last":{"inputTokens":3,"outputTokens":4,"cachedInputTokens":1,"totalTokens":7,"reasoningOutputTokens":0},"total":{}}}}),
            json!({"method":"turn/plan/updated","params":{"threadId":"thr","turnId":"t1","plan":[{"step":"look","status":"inProgress"}]}}),
            json!({"method":"turn/completed","params":{"threadId":"thr","turn":{"id":"t1","items":[],"status":"completed","error":null}}}),
        ] {
            let o = c.message(&v);
            got.extend(o.updates);
            writes.extend(o.writes);
        }
        assert_eq!(got[0], Update::Thinking("hm".into()));
        assert_eq!(got[1], Update::Text("Hi".into()));
        assert!(
            matches!(&got[2], Update::Tool(t) if t.status == ToolStatus::Started && t.name == "shell")
        );
        assert!(
            matches!(&got[3], Update::Approval(a) if a.id == "0" && a.message.as_deref() == Some("ls"))
        );
        assert!(
            matches!(&got[4], Update::Tool(t) if t.status == ToolStatus::Finished && t.output.as_deref() == Some("a.txt"))
        );
        assert!(matches!(&got[5], Update::Usage(u) if u.input == 3 && u.output == 4));
        assert!(matches!(&got[6], Update::Plan(p) if p[0].status == "in_progress"));
        assert_eq!(got[7], Update::TurnEnd(StopReason::EndTurn));
        // The queued prompt starts when the turn ends.
        assert_eq!(
            writes.last().map(|w| &w["method"]),
            Some(&json!("turn/start"))
        );
        // A cancel answers waiting approvals and interrupts the turn.
        c.message(&json!({"method":"turn/started","params":{"threadId":"thr","turn":{"id":"t2"}}}));
        c.message(&json!({"id":5,"method":"item/fileChange/requestApproval","params":{"threadId":"thr","turnId":"t2","itemId":"f1","startedAtMs":1}}));
        let o = c.input(Input::Cancel);
        assert_eq!(o.writes[0]["result"]["decision"], "cancel");
        assert_eq!(o.writes[1]["method"], "turn/interrupt");
        assert_eq!(o.writes[1]["params"]["turnId"], "t2");
        let o = c.message(&json!({"method":"turn/completed","params":{"threadId":"thr","turn":{"id":"t2","status":"interrupted"}}}));
        assert_eq!(o.updates, vec![Update::TurnEnd(StopReason::Cancelled)]);
    }

    #[test]
    fn a_prompt_waits_for_dreamers_tools_to_start() {
        let status = |name: &str, status: &str| json!({"method":"mcpServer/startupStatus/updated","params":{"threadId":"thr","name":name,"status":status,"error":if status == "failed" { json!("closed") } else { Value::Null }}});
        let mut c = Codex::new("thr".into(), Rpc::new(false), true);
        assert!(
            c.input(Input::Prompt("hi".into())).writes.is_empty(),
            "waits"
        );
        assert!(c.message(&status("dreamer", "starting")).writes.is_empty());
        assert!(c.message(&status("codex_apps", "ready")).writes.is_empty());
        let o = c.message(&status("dreamer", "ready"));
        assert_eq!(o.writes[0]["method"], "turn/start");
        assert_eq!(o.writes[0]["params"]["input"][0]["text"], "hi");

        // Failed: said, and the prompt goes on without them.
        let mut c = Codex::new("thr".into(), Rpc::new(false), true);
        c.input(Input::Prompt("hi".into()));
        let o = c.message(&status("dreamer", "failed"));
        assert!(
            matches!(&o.updates[0], Update::Error(e) if e.contains("Dreamer's tools") && e.contains("closed")),
            "{o:?}"
        );
        assert_eq!(o.writes[0]["method"], "turn/start");

        // Codex never says: the wait ends.
        let mut c = Codex::new("thr".into(), Rpc::new(false), true);
        c.input(Input::Prompt("hi".into()));
        let o = c.tools_wait_over();
        assert_eq!(o.writes[0]["method"], "turn/start");
        assert!(c.tools_wait_over().updates.is_empty(), "once");
    }

    #[test]
    fn dreamer_tools_reach_codex_with_their_environment() {
        let spec = Spec {
            tools: Some(crate::Tools {
                program: "/bin/dream".into(),
                args: vec!["mcp".into()],
            }),
            env: vec![("DREAMER_SESSION_ID".into(), "s1".into())],
            ..Spec::default()
        };
        let a = arguments(&spec);
        assert_eq!(a.last().map(String::as_str), Some("app-server"));
        assert!(a.contains(&r#"mcp_servers.dreamer.env={DREAMER_SESSION_ID="s1"}"#.to_owned()));
        assert!(a.contains(&r#"mcp_servers.dreamer.args=["mcp"]"#.to_owned()));
    }
}
