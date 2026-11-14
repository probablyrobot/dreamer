//! Write a Dreamer extension in Rust.
//!
//! An extension is a program `dreamerd` starts and supervises (see
//! `docs/extensions.md`). It speaks JSON-RPC 2.0 on standard input and
//! output; this crate does that part:
//!
//! - it answers the daemon's `extension.hello`, `$/ping`, and
//!   `extension.shutdown` by itself, so heartbeats are answered even while
//!   the extension is busy;
//! - [`Session::call`] makes typed calls ([`api::Request`] in,
//!   [`api::Response`] out) under the extension's grant;
//! - [`Session::notifications`] yields typed [`api::Notification`]s for its
//!   subscriptions;
//! - [`Session::stopped`] resolves when the daemon asks it to stop or its
//!   input ends.
//!
//! ```no_run
//! # async fn example() -> Result<(), dreamer_sdk::Error> {
//! let mut session = dreamer_sdk::start("my-extension 0.1").await?;
//! let mut events = session.notifications().expect("taken once");
//! session
//!     .call(api::Request::EventsSubscribe(api::EventSubscribe::default()))
//!     .await?;
//! loop {
//!     tokio::select! {
//!         Some(n) = events.recv() => eprintln!("{n:?}"),
//!         () = session.stopped() => return Ok(()),
//!     }
//! }
//! # }
//! ```
//!
//! Standard error is the extension's log; never print anything else to
//! standard output.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use api::RpcError;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

/// Notifications held for the extension before newer ones are dropped and
/// counted (see [`Session::dropped`]). Reading them never holds up the
/// heartbeat.
const NOTIFICATIONS: usize = 1024;

#[derive(Debug)]
pub enum Error {
    /// The daemon answered with an error.
    Rpc(RpcError),
    /// The connection to the daemon ended.
    Closed,
    /// A reply did not decode as the method's result.
    Decode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Rpc(e) => write!(f, "{e}"),
            Error::Closed => f.write_str("the connection to dreamerd ended"),
            Error::Decode(e) => write!(f, "undecodable reply: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<RpcError> for Error {
    fn from(e: RpcError) -> Self {
        Error::Rpc(e)
    }
}

/// What the daemon said in `extension.hello`.
#[derive(Debug, Clone, Default)]
pub struct Hello {
    /// This extension's name in `extensions.yaml`.
    pub name: String,
    /// The daemon's API version, `major.minor`.
    pub protocol: String,
    pub daemon_version: String,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A running extension's connection to the daemon.
pub struct Session {
    pub hello: Hello,
    out: mpsc::Sender<String>,
    pending: Pending,
    next: AtomicU64,
    notifications: Option<mpsc::Receiver<api::Notification>>,
    dropped: Arc<AtomicU64>,
    stop: watch::Receiver<bool>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

/// Start on standard input and output: wait for the daemon's
/// `extension.hello`, answer it with `version`, and return the session.
pub async fn start(version: &str) -> Result<Session, Error> {
    start_on(tokio::io::stdin(), tokio::io::stdout(), version).await
}

/// Like [`start`], on any reader and writer (for tests).
pub async fn start_on<R, W>(input: R, mut output: W, version: &str) -> Result<Session, Error>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut lines) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        while let Some(mut line) = lines.recv().await {
            line.push('\n');
            if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
                return;
            }
        }
    });
    let pending: Pending = Arc::default();
    let (notify, notifications) = mpsc::channel(NOTIFICATIONS);
    let dropped = Arc::new(AtomicU64::new(0));
    let (stop_tx, stop) = watch::channel(false);
    let (hello_tx, hello_rx) = oneshot::channel();
    tokio::spawn(read_loop(
        BufReader::new(input),
        out.clone(),
        pending.clone(),
        notify,
        dropped.clone(),
        stop_tx,
        hello_tx,
        version.to_owned(),
    ));
    let hello = hello_rx.await.map_err(|_| Error::Closed)?;
    Ok(Session {
        hello,
        out,
        pending,
        next: AtomicU64::new(1),
        notifications: Some(notifications),
        dropped,
        stop,
    })
}

#[allow(clippy::too_many_arguments)]
async fn read_loop<R: AsyncRead + Unpin>(
    input: BufReader<R>,
    out: mpsc::Sender<String>,
    pending: Pending,
    notify: mpsc::Sender<api::Notification>,
    dropped: Arc<AtomicU64>,
    stop: watch::Sender<bool>,
    hello: oneshot::Sender<Hello>,
    version: String,
) {
    let mut hello = Some(hello);
    let mut lines = input.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(Value::Object(mut msg)) = serde_json::from_str::<Value>(&line) else {
            eprintln!("dreamer-sdk: ignoring a line that is not a JSON object");
            continue;
        };
        let id = msg.remove("id");
        let params = msg.remove("params").unwrap_or(Value::Null);
        let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
        match msg.get("method").and_then(Value::as_str) {
            // The daemon's requests.
            Some("extension.hello") if id.is_some() => {
                let h = Hello {
                    name: str_field(&params, "name"),
                    protocol: str_field(&params, "protocol"),
                    daemon_version: str_field(&params, "daemon_version"),
                };
                if let Some(tx) = hello.take() {
                    let _ = tx.send(h);
                }
                let _ = out
                    .send(reply(json!({ "version": version })).to_string())
                    .await;
            }
            Some("$/ping") if id.is_some() => {
                let _ = out.send(reply(json!({})).to_string()).await;
            }
            Some("extension.shutdown") if id.is_some() => {
                let _ = out.send(reply(json!({})).to_string()).await;
                let _ = stop.send(true);
            }
            Some(other) if id.is_some() => {
                let e = json!({ "jsonrpc": "2.0", "id": id, "error": {
                    "code": api::code::METHOD_NOT_FOUND,
                    "message": format!("this extension has no method `{other}`"),
                }});
                let _ = out.send(e.to_string()).await;
            }
            // A notification for a subscription.
            Some(topic) => match api::Notification::from_json(topic, params) {
                Ok(n) => {
                    if notify.try_send(n).is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => eprintln!("dreamer-sdk: notification `{topic}`: {e}"),
            },
            // A reply to one of the extension's calls.
            None => {
                let Some(n) = id.as_ref().and_then(Value::as_u64) else {
                    continue;
                };
                let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&n)) else {
                    continue;
                };
                let r = match msg.remove("error") {
                    Some(e) => Err(RpcError {
                        code: e["code"].as_i64().unwrap_or(-32603) as i32,
                        message: str_field(&e, "message"),
                        data: e["data"].as_str().map(str::to_owned),
                    }),
                    None => Ok(msg.remove("result").unwrap_or(Value::Null)),
                };
                let _ = tx.send(r);
            }
        }
    }
    // Input ended: the daemon is gone or stopped this extension.
    let _ = stop.send(true);
    if let Ok(mut p) = pending.lock() {
        p.clear();
    }
}

fn str_field(v: &Value, name: &str) -> String {
    v.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl Session {
    /// One typed call under this extension's grant.
    pub async fn call(&self, request: api::Request) -> Result<api::Response, Error> {
        let method = request.method();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": request.to_json() });
        self.out
            .send(line.to_string())
            .await
            .map_err(|_| Error::Closed)?;
        let result = rx.await.map_err(|_| Error::Closed)??;
        api::Response::from_json(method, result).map_err(|e| Error::Decode(e.to_string()))
    }

    /// Publish `payload` on `topic` (see `docs/messages.md`).
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<api::MessageSent, Error> {
        match self
            .call(api::Request::MessagesPublish(api::MessagePublish {
                topic: Some(topic.to_owned()),
                payload,
                ..api::MessagePublish::default()
            }))
            .await?
        {
            api::Response::MessagesPublish(s) => Ok(s),
            other => Err(Error::Decode(format!("unexpected {}", other.method()))),
        }
    }

    /// The typed notifications for this extension's subscriptions; can be
    /// taken once.
    pub fn notifications(&mut self) -> Option<mpsc::Receiver<api::Notification>> {
        self.notifications.take()
    }

    /// Notifications dropped because they were not read in time.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Resolves when the daemon asks this extension to stop, or its input
    /// ends. Finish up and return from `main` promptly after it.
    pub async fn stopped(&self) {
        let mut stop = self.stop.clone();
        let _ = stop.wait_for(|s| *s).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake daemon on the other end of two pipes.
    struct Daemon {
        to_ext: tokio::io::DuplexStream,
        from_ext: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    }

    impl Daemon {
        async fn send(&mut self, v: Value) {
            self.to_ext
                .write_all(format!("{v}\n").as_bytes())
                .await
                .expect("write");
        }
        async fn recv(&mut self) -> Value {
            let line = self
                .from_ext
                .next_line()
                .await
                .expect("read")
                .expect("line");
            serde_json::from_str(&line).expect("json")
        }
    }

    async fn pair() -> (Session, Daemon) {
        let (to_ext, ext_in) = tokio::io::duplex(1 << 16);
        let (ext_out, from_ext) = tokio::io::duplex(1 << 16);
        let mut d = Daemon {
            to_ext,
            from_ext: BufReader::new(from_ext).lines(),
        };
        let starting = tokio::spawn(start_on(ext_in, ext_out, "test 1"));
        d.send(
            json!({"jsonrpc":"2.0","id":"dreamer-1","method":"extension.hello",
            "params":{"name":"t","protocol":"1.24","daemon_version":"0.1.0"}}),
        )
        .await;
        let r = d.recv().await;
        assert_eq!(r["id"], "dreamer-1");
        assert_eq!(r["result"]["version"], "test 1");
        let s = starting.await.expect("task").expect("start");
        assert_eq!(s.hello.name, "t");
        (s, d)
    }

    #[tokio::test]
    async fn the_handshake_heartbeat_and_shutdown_are_answered() {
        let (s, mut d) = pair().await;
        d.send(json!({"jsonrpc":"2.0","id":"dreamer-2","method":"$/ping","params":{}}))
            .await;
        assert_eq!(d.recv().await["id"], "dreamer-2");
        d.send(json!({"jsonrpc":"2.0","id":"dreamer-3","method":"extension.shutdown","params":{}}))
            .await;
        assert_eq!(d.recv().await["id"], "dreamer-3");
        tokio::time::timeout(std::time::Duration::from_secs(5), s.stopped())
            .await
            .expect("stopped");
    }

    #[tokio::test]
    async fn calls_are_typed_and_errors_come_back_as_rpc_errors() {
        let (mut s, mut d) = pair().await;
        let mut events = s.notifications().expect("notifications");
        let s = Arc::new(s);
        let caller = {
            let s = s.clone();
            tokio::spawn(async move { s.call(api::Request::GrantsCurrent(api::Empty {})).await })
        };
        let req = d.recv().await;
        assert_eq!(req["method"], "grants.current");
        d.send(json!({"jsonrpc":"2.0","id":req["id"],"result":{"owner":false,"methods":["x.*"]}}))
            .await;
        match caller.await.expect("task").expect("call") {
            api::Response::GrantsCurrent(g) => assert_eq!(g.methods, ["x.*"]),
            other => panic!("{other:?}"),
        }
        let caller = {
            let s = s.clone();
            tokio::spawn(async move { s.call(api::Request::DaemonShutdown(api::Empty {})).await })
        };
        let req = d.recv().await;
        d.send(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32005,"message":"no"}}))
            .await;
        match caller.await.expect("task") {
            Err(Error::Rpc(e)) => assert_eq!(e.code, -32005),
            other => panic!("{other:?}"),
        }
        // A notification arrives typed; the heartbeat is still answered.
        d.send(
            json!({"jsonrpc":"2.0","method":"events.batch","params":{"subscription":4,"cursor":9}}),
        )
        .await;
        match events.recv().await.expect("notification") {
            api::Notification::EventsBatch(b) => assert_eq!((b.subscription, b.cursor), (4, 9)),
            other => panic!("{other:?}"),
        }
    }
}
