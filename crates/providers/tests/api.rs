//! The API adapters against a stand-in HTTP server on this machine (the
//! vendors' streaming formats) and a stand-in MCP server: a tool loop to
//! an answer for each API, the key sent only as a header and never on a
//! command line, a refused key reported as such, and cancellation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use providers::{Api, ApiConfig, Event, Failure, KeySource, Provider, Request, Tools};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEY: &str = "sk-standin-4417";

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("providers-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

/// An MCP server with one tool, `list_panes`, answering `%0 %1`.
fn mcp_server(dir: &Path) -> Tools {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("mcp");
    std::fs::write(
        &p,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"s\",\"version\":\"1\"}}}" ;;
    *'"tools/list"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"list_panes\",\"description\":\"Panes\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
    *'"tools/call"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"%0 %1\"}]}}" ;;
  esac
done
"#,
    )
    .expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    Tools {
        program: p,
        args: Vec::new(),
    }
}

/// What the server saw: each request's head and body.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serve one response per request, in order; `hang` keeps the last open.
async fn server(responses: Vec<(u16, String)>, hang: bool) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Seen = Arc::default();
    let s = seen.clone();
    tokio::spawn(async move {
        let n = responses.len();
        for (i, (status, body)) in responses.into_iter().enumerate() {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            // Read the head, then the body by its length.
            let (head, mut body_in) = loop {
                let k = sock.read(&mut chunk).await.unwrap_or(0);
                if k == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..k]);
                if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (
                        String::from_utf8_lossy(&buf[..at]).into_owned(),
                        buf[at + 4..].to_vec(),
                    );
                }
            };
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while body_in.len() < len {
                let k = sock.read(&mut chunk).await.unwrap_or(0);
                if k == 0 {
                    break;
                }
                body_in.extend_from_slice(&chunk[..k]);
            }
            s.lock()
                .expect("seen")
                .push((head, String::from_utf8_lossy(&body_in).into_owned()));
            let kind = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            if hang && i + 1 == n {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
    (url, seen)
}

fn sse(events: &[&str]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn config(api: Api, url: &str) -> ApiConfig {
    ApiConfig {
        api,
        model: "standin-model".into(),
        key: KeySource::Given(KEY.into()),
        base_url: Some(url.into()),
        max_tokens: 256,
    }
}

async fn all(run: &mut providers::Run) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(20), run.events.recv()).await {
        out.push(e);
    }
    out
}

#[tokio::test]
async fn the_claude_api_runs_a_tool_loop_to_an_answer() {
    let d = dir("claude");
    let (url, seen) = server(
        vec![
            (200, sse(&[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"list_panes","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_stop"}"#,
            ])),
            (200, sse(&[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":20}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Two "}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"panes."}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            ])),
        ],
        false,
    )
    .await;
    let req = Request {
        prompt: "How many panes?".into(),
        system: Some("Be brief.".into()),
        tools: Some(mcp_server(&d)),
        ..Request::default()
    };
    let mut run = Provider::Api(config(Api::Claude, &url))
        .start(&req)
        .expect("start");
    let e = all(&mut run).await;
    assert!(
        matches!(&e[0], Event::Started { model: Some(m), .. } if m == "standin-model"),
        "{e:?}"
    );
    assert!(
        e.contains(&Event::ToolCall {
            id: "tu1".into(),
            name: "list_panes".into(),
            input: serde_json::json!({})
        }),
        "{e:?}"
    );
    assert!(
        e.contains(&Event::ToolResult {
            id: "tu1".into(),
            ok: true,
            text: "%0 %1".into()
        }),
        "{e:?}"
    );
    assert!(
        e.contains(&Event::Usage {
            input_tokens: 30,
            output_tokens: 8,
            cost_usd: None
        }),
        "{e:?}"
    );
    assert_eq!(e.last(), Some(&Event::Done("Two panes.".into())));

    let seen = seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 2);
    let (head, body) = &seen[0];
    assert!(head.starts_with("POST /v1/messages"), "{head}");
    assert!(
        head.to_lowercase().contains(&format!("x-api-key: {KEY}")),
        "{head}"
    );
    assert!(
        head.to_lowercase()
            .contains("anthropic-version: 2023-06-01"),
        "{head}"
    );
    let first: serde_json::Value = serde_json::from_str(body).expect("json");
    assert_eq!(first["model"], "standin-model");
    assert_eq!(first["system"], "Be brief.");
    assert_eq!(first["tools"][0]["name"], "list_panes");
    // The second request carries the tool's result back.
    let second: serde_json::Value = serde_json::from_str(&seen[1].1).expect("json");
    assert_eq!(second["messages"][2]["content"][0]["tool_use_id"], "tu1");
    assert_eq!(second["messages"][2]["content"][0]["content"], "%0 %1");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn the_openai_api_runs_a_tool_loop_to_an_answer() {
    let d = dir("openai");
    let (url, seen) = server(
        vec![
            (200, sse(&[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"list_panes","arguments":""}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":4}}"#,
                "[DONE]",
            ])),
            (200, sse(&[
                r#"{"choices":[{"index":0,"delta":{"content":"Two panes."}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ])),
        ],
        false,
    )
    .await;
    let req = Request {
        prompt: "How many panes?".into(),
        tools: Some(mcp_server(&d)),
        ..Request::default()
    };
    let mut run = Provider::Api(config(Api::OpenAi, &url))
        .start(&req)
        .expect("start");
    let e = all(&mut run).await;
    assert!(
        e.contains(&Event::ToolResult {
            id: "c1".into(),
            ok: true,
            text: "%0 %1".into()
        }),
        "{e:?}"
    );
    assert_eq!(e.last(), Some(&Event::Done("Two panes.".into())));
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen[0].0.starts_with("POST /chat/completions"),
        "{}",
        seen[0].0
    );
    assert!(
        seen[0]
            .0
            .to_lowercase()
            .contains(&format!("authorization: bearer {}", KEY.to_lowercase()))
    );
    let second: serde_json::Value = serde_json::from_str(&seen[1].1).expect("json");
    assert_eq!(second["messages"][2]["role"], "tool");
    assert_eq!(second["messages"][2]["tool_call_id"], "c1");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_refused_or_missing_key_is_reported_as_such() {
    let (url, _) = server(
        vec![(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into())],
        false,
    )
    .await;
    let req = Request {
        prompt: "hi".into(),
        ..Request::default()
    };
    let mut run = Provider::Api(config(Api::Claude, &url))
        .start(&req)
        .expect("start");
    let e = all(&mut run).await;
    assert!(
        matches!(e.last(), Some(Event::Failed(Failure::LoggedOut(why))) if why.contains("invalid x-api-key") && !why.contains(KEY)),
        "{e:?}"
    );

    let mut cfg = config(Api::OpenAi, &url);
    cfg.key = KeySource::Env("DREAMER_TEST_NO_SUCH_KEY_VARIABLE".into());
    let mut run = Provider::Api(cfg).start(&req).expect("start");
    let e = all(&mut run).await;
    assert!(
        matches!(e.as_slice(), [Event::Failed(Failure::LoggedOut(why))] if why.contains("DREAMER_TEST_NO_SUCH_KEY_VARIABLE")),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_cancelled_api_run_stops_and_its_key_was_never_on_a_command_line() {
    let (url, _) = server(
        vec![(200, sse(&[r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Thinking"}}"#]))],
        true,
    )
    .await;
    let req = Request {
        prompt: "hi".into(),
        ..Request::default()
    };
    let mut run = Provider::Api(config(Api::Claude, &url))
        .start(&req)
        .expect("start");
    loop {
        match run.events.recv().await {
            Some(Event::Text(_)) => break,
            Some(_) => {}
            None => panic!("no text"),
        }
    }
    // curl is running now: nothing on any command line holds the key.
    let ps = std::process::Command::new("ps")
        .args(["-ax", "-o", "args="])
        .output()
        .expect("ps");
    let listing = String::from_utf8_lossy(&ps.stdout);
    assert!(listing.contains("curl"), "curl should be running");
    assert!(
        !listing.contains(KEY),
        "the key is on a command line: {:?}",
        listing
            .lines()
            .filter(|l| l.contains(KEY))
            .collect::<Vec<_>>()
    );
    run.cancel();
    let e = all(&mut run).await;
    assert_eq!(e.last(), Some(&Event::Failed(Failure::Cancelled)), "{e:?}");
}
