//! The JSON-RPC 2.0 endpoint through a real daemon (spec 12.2): the same
//! service as the native socket, one JSON object per line.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message;
use serde_json::{Value, json};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-rpc-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    async fn native(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    async fn rpc(&self) -> Rpc {
        // The native socket answering means the JSON-RPC one is bound too.
        drop(self.native().await);
        let s = UnixStream::connect(self.root.join("rt/dreamerd.rpc.sock"))
            .await
            .expect("json-rpc socket");
        let (r, w) = s.into_split();
        Rpc {
            lines: BufReader::new(r),
            w,
            pending: Vec::new(),
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Rpc {
    lines: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
    /// Notifications read while waiting for a reply.
    pending: Vec<Value>,
}

impl Rpc {
    async fn send(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.expect("write");
        self.w.write_all(b"\n").await.expect("write");
    }

    async fn line(&mut self) -> Value {
        let mut s = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.lines.read_line(&mut s))
            .await
            .expect("a line within 10 s")
            .expect("read");
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("not JSON ({e}): {s:?}"))
    }

    /// The next reply (not a notification).
    async fn reply(&mut self) -> Value {
        loop {
            let v = self.line().await;
            if v.get("method").is_some() && v.get("id").is_none() {
                self.pending.push(v);
            } else {
                return v;
            }
        }
    }

    async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&req.to_string()).await;
        let r = self.reply().await;
        assert_eq!(r["id"], json!(id), "{r}");
        r
    }

    /// The next notification named `method`.
    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self.pending.iter().position(|v| v["method"] == method) {
            return self.pending.remove(i);
        }
        loop {
            let v = self.line().await;
            if v["method"] == method {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn calls_errors_batches_and_notifications() {
    let s = Setup::new();
    let mut rpc = s.rpc().await;

    let v = rpc.call(1, "daemon.version", json!({})).await;
    assert_eq!(v["result"]["protocol_major"], json!(1), "{v}");

    // Malformed input gets the standard errors and the connection lives on.
    rpc.send("this is not json").await;
    assert_eq!(rpc.reply().await["error"]["code"], json!(-32700));
    let v = rpc.call(2, "no.such_method", json!({})).await;
    assert_eq!(v["error"]["code"], json!(-32601), "{v}");
    rpc.send(r#"{"jsonrpc":"2.0","id":3,"method":"terminal.read","params":[1]}"#)
        .await;
    assert_eq!(rpc.reply().await["error"]["code"], json!(-32602));
    let v = rpc
        .call(4, "terminal.read", json!({ "pane": "not a number" }))
        .await;
    assert_eq!(v["error"]["code"], json!(-32602), "{v}");
    rpc.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"daemon.version","params":{{"x":"{}"}}}}"#,
        "a".repeat(2 << 20)
    ))
    .await;
    assert_eq!(rpc.reply().await["error"]["code"], json!(-32600));

    // A notification is not run: it could never confirm anything.
    rpc.send(r#"{"jsonrpc":"2.0","method":"daemon.shutdown"}"#)
        .await;
    let v = rpc.call(6, "daemon.status", json!({})).await;
    assert!(v["result"]["pid"].as_u64().is_some(), "still running: {v}");

    // A batch: two replies, none for the notification in it.
    rpc.send(
        r#"[{"jsonrpc":"2.0","id":7,"method":"daemon.version"},
            {"jsonrpc":"2.0","method":"daemon.shutdown"},
            {"jsonrpc":"2.0","id":8,"method":"grants.current"}]"#
            .replace('\n', " ")
            .as_str(),
    )
    .await;
    let v = rpc.reply().await;
    let ids: Vec<&Value> = v
        .as_array()
        .expect("batch reply")
        .iter()
        .map(|r| &r["id"])
        .collect();
    assert_eq!(ids, [&json!(7), &json!(8)]);
    assert_eq!(v[1]["result"]["owner"], json!(true));

    // Events arrive as notifications named by their topic.
    rpc.call(9, "tmux.topology", json!({})).await;
    let v = rpc
        .call(10, "events.subscribe", json!({ "kinds": ["tmux"] }))
        .await;
    assert!(v["result"]["id"].as_u64().is_some(), "{v}");
    s.tmux
        .run_checked(&["new-window", "-d", "-t", "main", "/bin/sh"])
        .expect("tmux");
    let n = rpc.notification("events.batch").await;
    assert_eq!(
        n["params"]["events"][0]["kind"],
        json!("tmux.topology"),
        "{n}"
    );
}

#[tokio::test]
async fn json_rpc_and_native_clients_exchange_messages() {
    let s = Setup::new();
    let mut rpc = s.rpc().await;
    let mut native = s.native().await;
    let mut pushed = native.pushed().expect("pushed");
    let Response::MessagesSubscribe(native_sub) = native
        .call(Request::MessagesSubscribe(api::MessageSubscribe {
            topics: vec!["chat.*".into()],
        }))
        .await
        .expect("subscribe")
    else {
        panic!("subscribe")
    };
    let v = rpc
        .call(1, "messages.subscribe", json!({ "topics": [] }))
        .await;
    let rpc_instance = v["result"]["instance"]
        .as_str()
        .expect("instance")
        .to_owned();
    assert!(rpc_instance.starts_with("owner:"), "{rpc_instance}");

    // JSON-RPC to native: bytes are base64 in JSON.
    let v = rpc
        .call(
            2,
            "messages.publish",
            json!({ "topic": "chat.hi", "payload": "aGk=", "sender": "someone-else" }),
        )
        .await;
    assert_eq!(v["result"]["delivered"], json!(1), "{v}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let d = loop {
        match tokio::time::timeout_at(deadline, pushed.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "messages.deliver" => {
                break api::MessageDelivery::decode(payload.as_slice()).expect("delivery");
            }
            Ok(Some(_)) => {}
            other => panic!("no delivery: {other:?}"),
        }
    };
    let e = d.envelope.expect("envelope");
    assert_eq!(e.payload, b"hi");
    assert_eq!(e.sender.expect("sender").instance, rpc_instance);
    assert_eq!(e.claimed_sender.as_deref(), Some("someone-else"));

    // Native to JSON-RPC, directly.
    native
        .call(Request::MessagesPublish(api::MessagePublish {
            recipient: Some(rpc_instance.clone()),
            payload: b"back".to_vec(),
            ..api::MessagePublish::default()
        }))
        .await
        .expect("direct");
    let n = rpc.notification("messages.deliver").await;
    assert_eq!(n["params"]["envelope"]["payload"], json!("YmFjaw=="), "{n}");
    assert_eq!(
        n["params"]["envelope"]["sender"]["instance"],
        json!(native_sub.instance)
    );
}
