//! Claude Code CLI: `claude -p ... --output-format stream-json --verbose
//! --include-partial-messages`, one JSON object per line.

use std::path::Path;

use serde_json::Value;

use crate::{Event, Failure, Kind, Parser, Request, State, classify, clip};

/// The CLI's own login, from `claude auth status --json`. Only whether
/// it is logged in and how are kept.
pub(crate) async fn login(program: &Path, login: &mut Option<String>) -> State {
    let Some((_, text)) =
        crate::output(program, &["auth", "status", "--json"], Kind::ClaudeCli).await
    else {
        return State::Unknown("`auth status` did not answer".into());
    };
    let Ok(v) = serde_json::from_str::<Value>(text.trim()) else {
        return State::Unknown("`auth status --json` was not JSON".into());
    };
    *login = v["authMethod"].as_str().map(str::to_owned);
    match v["loggedIn"].as_bool() {
        Some(true) => State::Ready,
        Some(false) => State::LoggedOut,
        None => State::Unknown("`auth status` did not say whether it is logged in".into()),
    }
}

pub(crate) fn arguments(req: &Request) -> Vec<String> {
    let mut mcp = serde_json::json!({ "mcpServers": {} });
    if let Some(t) = &req.tools {
        mcp["mcpServers"]["dreamer"] = serde_json::json!({
            "command": t.program.display().to_string(),
            "args": t.args,
        });
    }
    let mut a: Vec<String> = [
        "-p",
        &req.prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        // No built-in tools (shell, files, web): only Dreamer's.
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    a.push(mcp.to_string());
    if req.tools.is_some() {
        a.extend(["--allowedTools".into(), "mcp__dreamer".into()]);
    }
    if let Some(s) = &req.system {
        a.extend(["--append-system-prompt".into(), s.clone()]);
    }
    if let Some(n) = req.max_turns {
        a.extend(["--max-turns".into(), n.to_string()]);
    }
    if let Some(m) = &req.model {
        a.extend(["--model".into(), m.clone()]);
    }
    a
}

pub(crate) struct Stream {
    allow_metered: bool,
    /// Text deltas arrived for the current message, so its whole text is
    /// not sent again.
    streamed: bool,
}

impl Stream {
    pub(crate) fn new(allow_metered: bool) -> Stream {
        Stream {
            allow_metered,
            streamed: false,
        }
    }
}

impl Parser for Stream {
    fn line(&mut self, line: &str, stop: &mut bool) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                let source = v["apiKeySource"].as_str().unwrap_or("none");
                if source != "none" && !self.allow_metered {
                    *stop = true;
                    out.push(Event::Failed(Failure::Metered(source.to_owned())));
                    return out;
                }
                out.push(Event::Started {
                    session: v["session_id"].as_str().map(str::to_owned),
                    model: v["model"].as_str().map(str::to_owned),
                });
            }
            Some("stream_event") => {
                let e = &v["event"];
                match e["type"].as_str() {
                    Some("message_start") => self.streamed = false,
                    Some("content_block_delta") if e["delta"]["type"] == "text_delta" => {
                        if let Some(t) = e["delta"]["text"].as_str() {
                            self.streamed = true;
                            out.push(Event::Text(t.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") if !self.streamed => {
                            if let Some(t) = c["text"].as_str() {
                                out.push(Event::Text(t.to_owned()));
                            }
                        }
                        Some("tool_use") => out.push(Event::ToolCall {
                            id: c["id"].as_str().unwrap_or_default().to_owned(),
                            name: c["name"].as_str().unwrap_or_default().to_owned(),
                            input: c["input"].clone(),
                        }),
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" {
                        let text = match &c["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                            other => other.to_string(),
                        };
                        out.push(Event::ToolResult {
                            id: c["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                            ok: c["is_error"].as_bool() != Some(true),
                            text: clip(&text),
                        });
                    }
                }
            }
            Some("rate_limit_event") => {
                let info = &v["rate_limit_info"];
                let status = info["status"].as_str().unwrap_or("unknown");
                if status != "allowed" {
                    out.push(Event::Limit(format!(
                        "{status} ({})",
                        info["rateLimitType"].as_str().unwrap_or("limit")
                    )));
                }
            }
            Some("result") => {
                let u = &v["usage"];
                out.push(Event::Usage {
                    input_tokens: [
                        "input_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ]
                    .iter()
                    .filter_map(|k| u[*k].as_u64())
                    .sum(),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cost_usd: v["total_cost_usd"].as_f64(),
                });
                let text = v["result"].as_str().unwrap_or_default().to_owned();
                if v["is_error"].as_bool() == Some(true) || v["subtype"] != "success" {
                    let why = if text.is_empty() {
                        v["subtype"].as_str().unwrap_or("error").to_owned()
                    } else {
                        text
                    };
                    out.push(Event::Failed(
                        if v["api_error_status"].as_u64() == Some(429) {
                            Failure::RateLimited(why)
                        } else {
                            classify(&why)
                        },
                    ));
                } else {
                    out.push(Event::Done(text));
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

    /// Lines in the shape Claude Code 2.1.325 writes (trimmed).
    const RUN: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-x","apiKeySource":"none","tools":[],"mcp_servers":[{"name":"dreamer","status":"connected"}]}"#,
        r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Look"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ing."}}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"mcp__dreamer__panes","input":{}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"%0 sh"}]}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"One pane.","usage":{"input_tokens":2,"cache_read_input_tokens":10,"output_tokens":4},"total_cost_usd":0.01}"#,
    ];

    fn events(lines: &[&str], allow_metered: bool) -> (Vec<Event>, bool) {
        let mut s = Stream::new(allow_metered);
        let mut stop = false;
        let out = lines.iter().flat_map(|l| s.line(l, &mut stop)).collect();
        (out, stop)
    }

    #[test]
    fn a_run_streams_text_tool_calls_and_its_answer() {
        let (e, stop) = events(RUN, false);
        assert!(!stop);
        assert_eq!(
            e,
            vec![
                Event::Started {
                    session: Some("s1".into()),
                    model: Some("claude-x".into())
                },
                Event::Text("Look".into()),
                Event::Text("ing.".into()),
                // The whole message's text is not repeated after deltas.
                Event::ToolCall {
                    id: "t1".into(),
                    name: "mcp__dreamer__panes".into(),
                    input: serde_json::json!({})
                },
                Event::ToolResult {
                    id: "t1".into(),
                    ok: true,
                    text: "%0 sh".into()
                },
                Event::Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    cost_usd: Some(0.01)
                },
                Event::Done("One pane.".into()),
            ]
        );
    }

    #[test]
    fn an_api_key_stops_the_run_unless_allowed() {
        let init = r#"{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY"}"#;
        let (e, stop) = events(&[init], false);
        assert!(stop);
        assert_eq!(
            e,
            vec![Event::Failed(Failure::Metered("ANTHROPIC_API_KEY".into()))]
        );
        let (e, stop) = events(&[init], true);
        assert!(!stop);
        assert!(matches!(e[0], Event::Started { .. }));
    }

    #[test]
    fn a_logged_out_cli_says_so() {
        let (e, _) = events(
            &[
                r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login","usage":{}}"#,
            ],
            false,
        );
        assert!(
            matches!(e.last(), Some(Event::Failed(Failure::LoggedOut(_)))),
            "{e:?}"
        );
    }

    #[test]
    fn tools_are_dreamer_s_alone() {
        let req = Request {
            prompt: "hi".into(),
            tools: Some(crate::Tools {
                program: "/bin/dream".into(),
                args: vec!["mcp".into(), "--pane".into(), "3".into()],
            }),
            ..Request::default()
        };
        let a = arguments(&req);
        let at = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].clone());
        assert_eq!(at("--tools").as_deref(), Some(""));
        assert_eq!(at("--allowedTools").as_deref(), Some("mcp__dreamer"));
        assert!(a.contains(&"--strict-mcp-config".to_owned()));
        let mcp: Value =
            serde_json::from_str(&at("--mcp-config").unwrap_or_default()).expect("json");
        assert_eq!(mcp["mcpServers"]["dreamer"]["args"][2], "3");
    }
}
