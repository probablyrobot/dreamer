//! A small MCP client over a child's standard streams (newline-delimited
//! JSON-RPC 2.0): enough for the API adapters to list Dreamer's tools from
//! `dream mcp` and call them.

use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::Tools;

/// One tool as the server describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub(crate) struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Client {
    pub(crate) async fn start(tools: &Tools, env: &[(String, String)]) -> Result<Client, String> {
        let mut child = tokio::process::Command::new(&tools.program)
            .args(&tools.args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{}: {e}", tools.program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut c = Client {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next: 1,
        };
        c.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "dreamer-providers", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
        c.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await?;
        Ok(c)
    }

    /// The server's process group, to end with a run.
    pub(crate) fn pgid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("tools: {e}"))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|e| format!("tools: {e}"))?
                .ok_or("the tools server ended")?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Notifications and requests from the server are not ours.
            if v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(format!(
                    "{method}: {}",
                    e["message"].as_str().unwrap_or("error")
                ));
            }
            return Ok(v["result"].clone());
        }
    }

    pub(crate) async fn list(&mut self) -> Result<Vec<Tool>, String> {
        let r = self.request("tools/list", json!({})).await?;
        Ok(r["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| Tool {
                name: t["name"].as_str().unwrap_or_default().to_owned(),
                description: t["description"].as_str().unwrap_or_default().to_owned(),
                schema: if t["inputSchema"].is_object() {
                    t["inputSchema"].clone()
                } else {
                    json!({ "type": "object" })
                },
            })
            .collect())
    }

    /// Call a tool: its text, and whether it succeeded.
    pub(crate) async fn call(&mut self, name: &str, input: &Value) -> (bool, String) {
        let args = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        match self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await
        {
            Err(e) => (false, e),
            Ok(r) => {
                let text = r["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = if text.is_empty() {
                    r["structuredContent"].to_string()
                } else {
                    text
                };
                (r["isError"].as_bool() != Some(true), text)
            }
        }
    }
}
