//! The JSON-RPC 2.0 endpoint (spec 12.2): one JSON object per line, on its
//! own socket, so the protocol is chosen by the endpoint's name and never
//! guessed.
//!
//! The adapter does not reimplement the service. Each JSON-RPC connection
//! drives an ordinary native connection over an in-process socket pair, so
//! identity, grants, request handling, subscriptions, the event log, and
//! the message broker are the very same code. The adapter only translates:
//! a request's params to the method's record, its result or error back, and
//! every pushed record to a notification named by its topic.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::log;
use crate::service::State;

/// The longest line accepted.
pub const MAX_LINE: usize = 1 << 20;
/// Lines waiting for the writer.
const OUT_QUEUE: usize = 256;

pub fn socket_path() -> std::path::PathBuf {
    platform::rpc_socket_path()
}

/// Serve a client of the JSON-RPC socket, as the user (the socket is
/// owner-only, like the native one).
pub async fn connection(stream: UnixStream, state: Arc<State>) {
    let peer_pid = stream
        .peer_cred()
        .ok()
        .and_then(|c| c.pid())
        .and_then(|p| u32::try_from(p).ok());
    let (r, w) = stream.into_split();
    serve(r, w, state, peer_pid, crate::grants::Grant::owner()).await;
}

fn error(id: Value, code: i32, message: impl Into<String>, data: Option<String>) -> Value {
    let mut e = json!({ "code": code, "message": message.into() });
    if let Some(d) = data {
        e["data"] = json!(d);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

/// Speak JSON-RPC on `r`/`w` for a peer holding `grant`.
pub async fn serve<R, W>(
    r: R,
    mut w: W,
    state: Arc<State>,
    peer_pid: Option<u32>,
    grant: crate::grants::Grant,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (inner, outer) = match UnixStream::pair() {
        Ok(p) => p,
        Err(e) => {
            log::warn(format!("jsonrpc: socket pair: {e}"));
            return;
        }
    };
    tokio::spawn(crate::conn::connection_as(inner, state, peer_pid, grant));
    let (or, ow) = outer.into_split();
    let mut client = match client::Client::over(or, ow, "jsonrpc", 1024).await {
        Ok(c) => c,
        Err(e) => {
            log::warn(format!("jsonrpc: inner connection: {e}"));
            return;
        }
    };
    let pushed = client.pushed();
    let client = Arc::new(client);
    let (out, mut lines) = mpsc::channel::<String>(OUT_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(mut line) = lines.recv().await {
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
        let _ = w.flush().await;
    });
    let forwarder = pushed.map(|mut pushed| {
        let out = out.clone();
        tokio::spawn(async move {
            while let Some(p) = pushed.recv().await {
                let (topic, payload) = match p {
                    client::Pushed::Event { topic, payload, .. } => (topic, payload),
                    client::Pushed::Bulk { topic, data, .. } => (topic, data),
                    client::Pushed::Closed { .. } => return,
                };
                let params = match api::Notification::decode(&topic, &payload) {
                    Ok(n) => n.to_json(),
                    Err(e) => {
                        log::warn(format!("jsonrpc: pushed `{topic}`: {e}"));
                        continue;
                    }
                };
                let n = json!({ "jsonrpc": "2.0", "method": topic, "params": params });
                if out.send(n.to_string()).await.is_err() {
                    return;
                }
            }
        })
    });
    let mut requests = tokio::task::JoinSet::new();
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match read_line(&mut reader, &mut buf).await {
            Ok(Line::Eof) => break,
            Ok(Line::TooLong) => {
                let e = error(
                    Value::Null,
                    api::code::INVALID_REQUEST,
                    format!("a line is longer than {MAX_LINE} bytes"),
                    None,
                );
                if out.send(e.to_string()).await.is_err() {
                    break;
                }
            }
            Ok(Line::Text) => {
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let (client, out) = (client.clone(), out.clone());
                let text = std::mem::take(&mut buf);
                // Finished requests are reaped as new ones arrive.
                while requests.try_join_next().is_some() {}
                requests.spawn(async move {
                    if let Some(reply) = message(&client, &text).await {
                        let _ = out.send(reply.to_string()).await;
                    }
                });
            }
            Err(_) => break,
        }
    }
    // Input ended: answer what is in flight, then close. Subscriptions end
    // with the connection.
    let drain = async { while requests.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(60), drain)
        .await
        .is_err()
    {
        requests.abort_all();
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(out);
    let _ = writer.await;
}

enum Line {
    Text,
    TooLong,
    Eof,
}

/// One line into `buf`, without its newline. A line over [`MAX_LINE`] is
/// skipped to its end and reported, so one bad line does not end the
/// connection or hold unbounded memory.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Line> {
    let mut too_long = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() && !too_long {
                Line::Eof
            } else if too_long {
                Line::TooLong
            } else {
                Line::Text
            });
        }
        let (chunk, found) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (&available[..i], Some(i)),
            None => (available, None),
        };
        if !too_long {
            if buf.len() + chunk.len() > MAX_LINE {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = found.map_or(chunk.len(), |i| i + 1);
        reader.consume(used);
        if found.is_some() {
            return Ok(if too_long { Line::TooLong } else { Line::Text });
        }
    }
}

/// Answer one line: a request, a notification (no answer), or a batch.
async fn message(client: &client::Client, text: &[u8]) -> Option<Value> {
    let value: Value = match serde_json::from_slice(text) {
        Ok(v) => v,
        Err(e) => {
            return Some(error(
                Value::Null,
                api::code::PARSE_ERROR,
                format!("not JSON: {e}"),
                None,
            ));
        }
    };
    match value {
        Value::Array(items) if items.is_empty() => Some(error(
            Value::Null,
            api::code::INVALID_REQUEST,
            "an empty batch",
            None,
        )),
        Value::Array(items) => {
            // The calls of a batch run in order; notifications in it get no
            // entry in the reply.
            let mut replies = Vec::new();
            for v in items {
                if let Some(r) = one(client, v).await {
                    replies.push(r);
                }
            }
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        v => one(client, v).await,
    }
}

async fn one(client: &client::Client, v: Value) -> Option<Value> {
    let Value::Object(mut obj) = v else {
        return Some(error(
            Value::Null,
            api::code::INVALID_REQUEST,
            "a request is a JSON object",
            None,
        ));
    };
    let id = obj.remove("id");
    let reply_id = id.clone().unwrap_or(Value::Null);
    if obj.get("jsonrpc") != Some(&json!("2.0")) {
        return Some(error(
            reply_id,
            api::code::INVALID_REQUEST,
            "`jsonrpc` must be \"2.0\"",
            None,
        ));
    }
    let Some(Value::String(method)) = obj.remove("method") else {
        return Some(error(
            reply_id,
            api::code::INVALID_REQUEST,
            "`method` must be a string",
            None,
        ));
    };
    // A notification gets no reply, so it could never confirm anything:
    // it is not run at all.
    let id = match id {
        None => return None,
        Some(id @ (Value::Number(_) | Value::String(_) | Value::Null)) => id,
        Some(_) => {
            return Some(error(
                Value::Null,
                api::code::INVALID_REQUEST,
                "`id` must be a number, a string, or null",
                None,
            ));
        }
    };
    let params = match obj.remove("params") {
        None => json!({}),
        Some(p @ Value::Object(_)) => p,
        Some(_) => {
            return Some(error(
                id,
                api::code::INVALID_PARAMS,
                "params are passed by name, as an object",
                None,
            ));
        }
    };
    let request = match api::Request::from_json(&method, params) {
        Ok(r) => r,
        Err(api::CallError::UnknownMethod(m)) => {
            return Some(error(
                id,
                api::code::METHOD_NOT_FOUND,
                format!("no method `{m}`"),
                None,
            ));
        }
        Err(e) => {
            return Some(error(
                id,
                api::code::INVALID_PARAMS,
                format!("params for `{method}`: {e}"),
                None,
            ));
        }
    };
    Some(match client.call(request).await {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r.to_json() }),
        Err(client::ClientError::Rpc(e)) => error(id, e.code, e.message, e.data),
        Err(e) => error(id, api::code::INTERNAL, e.to_string(), None),
    })
}
