//! Codex CLI: `codex exec --json ...`, one JSON event per line. Built from
//! Codex's documented events (`thread.started`, `item.*`, `turn.*`,
//! `error`); not yet run against a real Codex.

use std::path::Path;

use serde_json::Value;

use crate::{Event, Kind, Parser, Request, State, classify, clip};

/// The CLI's own login, from `codex login status`.
pub(crate) async fn login(program: &Path, pre: &[String], login: &mut Option<String>) -> State {
    let Some((ok, text)) = crate::output(program, pre, &["login", "status"], Kind::CodexCli).await
    else {
        return State::Unknown("`login status` did not answer".into());
    };
    let t = text.to_lowercase();
    if !ok || t.contains("not logged in") {
        return State::LoggedOut;
    }
    if t.contains("api key") {
        *login = Some("api key".into());
        return State::MeteredLogin("logged in with an API key".into());
    }
    if t.contains("chatgpt") {
        *login = Some("chatgpt".into());
        return State::Ready;
    }
    State::Unknown(text.trim().to_owned())
}

/// A TOML string for a `-c key=value` override.
fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

pub(crate) fn arguments(req: &Request) -> Vec<String> {
    let mut a: Vec<String> = [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(t) = &req.tools {
        a.push("-c".into());
        a.push(format!(
            "mcp_servers.dreamer.command={}",
            toml_string(&t.program.display().to_string())
        ));
        let args: Vec<String> = t.args.iter().map(|x| toml_string(x)).collect();
        a.push("-c".into());
        a.push(format!("mcp_servers.dreamer.args=[{}]", args.join(",")));
    }
    if let Some(m) = &req.model {
        a.extend(["--model".into(), m.clone()]);
    }
    let prompt = match &req.system {
        Some(s) => format!("{s}\n\n{}", req.prompt),
        None => req.prompt.clone(),
    };
    a.push("--".into());
    a.push(prompt);
    a
}

#[derive(Default)]
pub(crate) struct Stream {
    /// The last agent message: the run's answer when the turn completes.
    last: String,
}

impl Parser for Stream {
    fn line(&mut self, line: &str, _stop: &mut bool) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let item = &v["item"];
        match (v["type"].as_str(), item["type"].as_str()) {
            (Some("thread.started"), _) => vec![Event::Started {
                session: v["thread_id"].as_str().map(str::to_owned),
                model: None,
            }],
            (Some("item.started"), Some("mcp_tool_call")) => vec![Event::ToolCall {
                id: item["id"].as_str().unwrap_or_default().to_owned(),
                name: format!(
                    "{}.{}",
                    item["server"].as_str().unwrap_or_default(),
                    item["tool"].as_str().unwrap_or_default()
                ),
                input: item["arguments"].clone(),
            }],
            (Some("item.completed"), Some("mcp_tool_call")) => {
                let ok = item["status"] == "completed" && item["error"].is_null();
                let text = if ok {
                    item["result"].to_string()
                } else {
                    item["error"]["message"]
                        .as_str()
                        .map_or_else(|| item["error"].to_string(), str::to_owned)
                };
                vec![Event::ToolResult {
                    id: item["id"].as_str().unwrap_or_default().to_owned(),
                    ok,
                    text: clip(&text),
                }]
            }
            // Codex's own shell commands (in its read-only sandbox).
            (Some("item.started"), Some("command_execution")) => vec![Event::ToolCall {
                id: item["id"].as_str().unwrap_or_default().to_owned(),
                name: "command".into(),
                input: serde_json::json!({ "command": item["command"] }),
            }],
            (Some("item.completed"), Some("command_execution")) => vec![Event::ToolResult {
                id: item["id"].as_str().unwrap_or_default().to_owned(),
                ok: item["status"] == "completed" && item["exit_code"].as_i64().unwrap_or(0) == 0,
                text: clip(item["aggregated_output"].as_str().unwrap_or_default()),
            }],
            (Some("item.completed"), Some("agent_message")) => {
                let text = item["text"].as_str().unwrap_or_default().to_owned();
                self.last.clone_from(&text);
                vec![Event::Text(text)]
            }
            (Some("turn.completed"), _) => {
                let u = &v["usage"];
                vec![
                    Event::Usage {
                        input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                        cost_usd: None,
                    },
                    Event::Done(std::mem::take(&mut self.last)),
                ]
            }
            (Some("turn.failed"), _) => vec![Event::Failed(classify(
                v["error"]["message"].as_str().unwrap_or("the turn failed"),
            ))],
            (Some("error"), _) => vec![Event::Failed(classify(
                v["message"].as_str().unwrap_or("error"),
            ))],
            _ => Vec::new(),
        }
    }
}

impl Stream {
    #[cfg(test)]
    fn all(lines: &[&str]) -> Vec<Event> {
        let mut s = Stream::default();
        let mut stop = false;
        lines.iter().flat_map(|l| s.line(l, &mut stop)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Failure;

    #[test]
    fn a_run_reports_tool_calls_messages_and_usage() {
        let e = Stream::all(&[
            r#"{"type":"thread.started","thread_id":"th1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"mcp_tool_call","server":"dreamer","tool":"panes","arguments":{},"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"mcp_tool_call","server":"dreamer","tool":"panes","status":"completed","result":{"content":[]}}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"Two panes."}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":30,"cached_input_tokens":0,"output_tokens":5}}"#,
        ]);
        assert_eq!(
            e[0],
            Event::Started {
                session: Some("th1".into()),
                model: None
            }
        );
        assert!(matches!(&e[1], Event::ToolCall { name, .. } if name == "dreamer.panes"));
        assert!(matches!(&e[2], Event::ToolResult { ok: true, .. }));
        assert_eq!(e[3], Event::Text("Two panes.".into()));
        assert_eq!(e[5], Event::Done("Two panes.".into()));
    }

    #[test]
    fn shell_commands_are_tool_calls() {
        let e = Stream::all(&[
            r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"ls","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"a","exit_code":2,"status":"failed"}}"#,
        ]);
        assert!(matches!(&e[0], Event::ToolCall { name, .. } if name == "command"));
        assert!(
            matches!(&e[1], Event::ToolResult { ok: false, .. }),
            "{e:?}"
        );
    }

    #[test]
    fn a_failed_turn_is_classified() {
        let e = Stream::all(&[
            r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#,
        ]);
        assert!(matches!(&e[0], Event::Failed(Failure::RateLimited(_))));
    }

    #[test]
    fn dreamer_s_tools_are_its_one_mcp_server() {
        let req = Request {
            prompt: "hi".into(),
            system: Some("Be brief.".into()),
            tools: Some(crate::Tools {
                program: "/bin/dream".into(),
                args: vec!["mcp".into()],
            }),
            ..Request::default()
        };
        let a = arguments(&req);
        assert!(
            a.contains(&r#"mcp_servers.dreamer.command="/bin/dream""#.to_owned()),
            "{a:?}"
        );
        assert!(
            a.contains(&r#"mcp_servers.dreamer.args=["mcp"]"#.to_owned()),
            "{a:?}"
        );
        assert_eq!(a.last().map(String::as_str), Some("Be brief.\n\nhi"));
    }
}
