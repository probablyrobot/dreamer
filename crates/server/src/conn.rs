//! One client connection: handshake, a reader that runs requests
//! concurrently, a writer that owns the outbox (priority frames before bulk
//! chunks), and the terminal subscriptions pushed on this connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use api::{RpcError, code};
use prost::Message as _;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use wire::io::{Reader, server_handshake};
use wire::{Kind, Limits, Outbox, WireError, welcome};

use crate::log;
use crate::service::{DaemonService, State};

/// Records a subscriber may have in flight before it counts as lagging.
const SUBSCRIBER_QUEUE: usize = 4096;
/// Frames waiting for the connection's writer.
const CONNECTION_QUEUE: usize = 256;
/// Limits of one pushed batch.
const BATCH_RECORDS: usize = 512;
const BATCH_BYTES: usize = 256 << 10;

/// Queue sizes; `DREAMER_TEST_SMALL_QUEUES` shrinks them so tests reach
/// back-pressure the same way on every platform.
fn queues() -> (usize, usize) {
    if std::env::var_os("DREAMER_TEST_SMALL_QUEUES").is_some() {
        (16, 4)
    } else {
        (SUBSCRIBER_QUEUE, CONNECTION_QUEUE)
    }
}

pub const TOPIC_RECORDS: &str = "terminal.records";
pub const TOPIC_END: &str = "terminal.end";
pub const TOPIC_CHECKPOINT: &str = "terminal.checkpoint";

enum Out {
    Frame(Kind, Vec<u8>),
    Bulk(Vec<u8>),
}

#[derive(Clone)]
struct Conn {
    out: mpsc::Sender<Out>,
    subs: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
    next_sub: Arc<AtomicU64>,
    events: Arc<AtomicU64>,
}

impl Conn {
    async fn event(&self, topic: &str, payload: Vec<u8>) -> bool {
        let e = wire::Event {
            topic: topic.to_owned(),
            sequence: self.events.fetch_add(1, Ordering::Relaxed),
            payload,
        };
        self.out
            .send(Out::Frame(Kind::Event, e.encode_to_vec()))
            .await
            .is_ok()
    }
}

pub async fn connection(stream: UnixStream, state: Arc<State>) {
    let (r, mut w) = stream.into_split();
    let me = &state.identity;
    let mut reader = Reader::new(r, me.limits);
    let accepted = tokio::time::timeout(
        Duration::from_secs(5),
        server_handshake(&mut reader, &mut w, me, |n| {
            welcome(
                me,
                n,
                &state.instance,
                state.epoch,
                &crate::host_name(),
                "default",
            )
        }),
    )
    .await;
    let (hello, negotiated) = match accepted {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::warn(format!("refused a client: {e}"));
            return;
        }
        Err(_) => {
            log::warn("a client did not complete the handshake in time");
            return;
        }
    };
    state.clients.fetch_add(1, Ordering::Relaxed);
    let (out, out_rx) = mpsc::channel(queues().1);
    let writer = tokio::spawn(write_loop(w, out_rx, negotiated.limits));
    let conn = Conn {
        out,
        subs: Arc::default(),
        next_sub: Arc::new(AtomicU64::new(1)),
        events: Arc::new(AtomicU64::new(1)),
    };
    let service = DaemonService(state.clone());
    loop {
        let (kind, payload) = match reader.next_frame().await {
            Ok(f) => f,
            Err(WireError::Closed) => break,
            Err(e) => {
                log::warn(format!("{} client (pid {}): {e}", hello.role, hello.pid));
                break;
            }
        };
        match kind {
            Kind::Request => {
                let req = match wire::Request::decode(payload.as_slice()) {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn(format!("undecodable request: {e}"));
                        break;
                    }
                };
                let (service, conn, state) = (service.clone(), conn.clone(), state.clone());
                tokio::spawn(async move {
                    let shutdown = req.method == api::method::DAEMON_SHUTDOWN;
                    if req.method == api::method::TERMINAL_SUBSCRIBE {
                        // Sends its own response, then the stream.
                        subscribe(&service, &conn, req).await;
                        return;
                    }
                    let response = if req.method == api::method::TERMINAL_UNSUBSCRIBE {
                        unsubscribe(&conn, req).await
                    } else {
                        answer(&service, req).await
                    };
                    let _ = conn
                        .out
                        .send(Out::Frame(Kind::Response, response.encode_to_vec()))
                        .await;
                    if shutdown && state.shutting_down.load(Ordering::SeqCst) {
                        // Let the writer flush the reply before stopping.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        state.shutdown.notify_one();
                    }
                });
            }
            Kind::Ping => {
                let _ = conn.out.send(Out::Frame(Kind::Pong, Vec::new())).await;
            }
            Kind::Goodbye => break,
            other => {
                log::warn(format!("unexpected {other:?} frame from a client"));
                break;
            }
        }
    }
    for (_, task) in conn.subs.lock().await.drain() {
        task.abort();
    }
    drop(conn);
    drop(service);
    let _ = writer.await;
    state.clients.fetch_sub(1, Ordering::Relaxed);
}

/// Owns the socket's write half: priority frames first, bulk data in
/// chunks between them.
async fn write_loop(mut w: OwnedWriteHalf, mut rx: mpsc::Receiver<Out>, limits: Limits) {
    let mut outbox = Outbox::new(limits);
    let push = |outbox: &mut Outbox, msg: Out| -> Result<(), WireError> {
        match msg {
            Out::Frame(kind, payload) => outbox.push_priority(kind, &payload),
            Out::Bulk(data) => outbox.push_bulk(data),
        }
    };
    loop {
        let msg = if outbox.is_empty() {
            match rx.recv().await {
                Some(m) => Some(m),
                None => break,
            }
        } else {
            rx.try_recv().ok()
        };
        if let Some(m) = msg
            && let Err(e) = push(&mut outbox, m)
        {
            log::warn(format!("dropping a client that does not read: {e}"));
            return;
        }
        if let Some(frame) = outbox.next_frame()
            && w.write_all(&frame).await.is_err()
        {
            return;
        }
    }
    while let Some(frame) = outbox.next_frame() {
        if w.write_all(&frame).await.is_err() {
            return;
        }
    }
    let _ = w.flush().await;
}

pub async fn answer(service: &DaemonService, req: wire::Request) -> wire::Response {
    let request = match api::Request::decode(&req.method, &req.payload) {
        Ok(r) => r,
        Err(e) => return failure(req.id, e.into()),
    };
    match api::dispatch(service, request).await {
        Ok(response) => wire::Response {
            id: req.id,
            payload: response.encode(),
            error_code: None,
            error_message: String::new(),
        },
        Err(e) => failure(req.id, e),
    }
}

fn failure(id: u64, e: RpcError) -> wire::Response {
    wire::Response {
        id,
        payload: Vec::new(),
        error_code: Some(e.code),
        error_message: e.message,
    }
}

fn ok(id: u64, payload: Vec<u8>) -> wire::Response {
    wire::Response {
        id,
        payload,
        error_code: None,
        error_message: String::new(),
    }
}

async fn unsubscribe(conn: &Conn, req: wire::Request) -> wire::Response {
    let p = match api::Unsubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => return failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string())),
    };
    match conn.subs.lock().await.remove(&p.subscription) {
        Some(task) => {
            task.abort();
            let end = api::SubscriptionEnd {
                subscription: p.subscription,
                reason: api::EndReason::Unsubscribed as i32,
                detail: None,
            };
            conn.event(TOPIC_END, end.encode_to_vec()).await;
            ok(req.id, api::Empty {}.encode_to_vec())
        }
        None => failure(
            req.id,
            RpcError::new(
                code::NOT_FOUND,
                format!("no subscription {}", p.subscription),
            ),
        ),
    }
}

fn to_api(seq: u64, r: &mirror::Record) -> api::TerminalRecord {
    match r {
        mirror::Record::Bytes(b) => api::TerminalRecord {
            sequence: seq,
            data: b.clone(),
            cols: None,
            rows: None,
        },
        mirror::Record::Resize { cols, rows } => api::TerminalRecord {
            sequence: seq,
            data: Vec::new(),
            cols: Some(u32::from(*cols)),
            rows: Some(u32::from(*rows)),
        },
    }
}

async fn subscribe(service: &DaemonService, conn: &Conn, req: wire::Request) {
    let id = req.id;
    let reply = |r: wire::Response| async move {
        let _ = conn
            .out
            .send(Out::Frame(Kind::Response, r.encode_to_vec()))
            .await;
    };
    let p = match api::TerminalSubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => {
            return reply(failure(
                id,
                RpcError::new(code::INVALID_PARAMS, e.to_string()),
            ))
            .await;
        }
    };
    let host = match service.mirror_host(p.endpoint.as_deref()).await {
        Ok(h) => h,
        Err(e) => return reply(failure(id, e)).await,
    };
    let pane = domain::TmuxPaneId(p.pane);
    // Replay is only possible within the same connection epoch.
    let resume = match (p.resume_epoch, p.resume_generation, p.resume_sequence) {
        (Some(e), Some(g), Some(s)) if e == host.epoch().0 => Some((domain::PaneGeneration(g), s)),
        _ => None,
    };
    if !host.wait_live(&[pane], Duration::from_secs(3)).await {
        return reply(failure(
            id,
            RpcError::new(
                code::NOT_FOUND,
                format!("pane %{} has no live mirror", p.pane),
            ),
        ))
        .await;
    }
    let (tx, rx) = mpsc::channel(queues().0);
    let ended = Arc::new(AtomicU8::new(mirror::end::RUNNING));
    let started = match host
        .subscribe(
            pane,
            resume,
            mirror::Sink {
                records: tx,
                ended: ended.clone(),
            },
        )
        .await
    {
        Ok(s) => s,
        Err(e) => return reply(failure(id, RpcError::new(code::UNAVAILABLE, e))).await,
    };
    let sub = conn.next_sub.fetch_add(1, Ordering::Relaxed);
    let (mode, sequence, bytes, pause_us) = match &started.start {
        mirror::Start::Checkpoint {
            sequence,
            snapshot,
            pause,
        } => (
            api::SubscribeMode::Checkpoint,
            *sequence,
            snapshot.len() as u64,
            pause.as_micros() as u64,
        ),
        mirror::Start::Replay { after, .. } => (api::SubscribeMode::Replay, *after, 0, 0),
    };
    let s = api::Subscription {
        id: sub,
        pane: p.pane,
        epoch: started.epoch.0,
        generation: started.generation.0,
        mode: mode as i32,
        sequence,
        checkpoint_bytes: bytes,
        checkpoint_pause_us: pause_us,
        scrollback_lines: started.scrollback as u32,
    };
    reply(ok(id, s.encode_to_vec())).await;
    let (pane_id, epoch, generation) = (p.pane, started.epoch.0, started.generation.0);
    let batch = move |records: Vec<api::TerminalRecord>| api::RecordBatch {
        subscription: sub,
        pane: pane_id,
        epoch,
        generation,
        records,
    };
    match started.start {
        mirror::Start::Checkpoint {
            sequence, snapshot, ..
        } => {
            let data = api::CheckpointData {
                subscription: sub,
                pane: p.pane,
                epoch: started.epoch.0,
                generation: started.generation.0,
                sequence,
                snapshot,
            };
            let bulk = wire::Bulk {
                stream: sub,
                topic: TOPIC_CHECKPOINT.into(),
                data: data.encode_to_vec(),
            };
            let _ = conn.out.send(Out::Bulk(bulk.encode_to_vec())).await;
        }
        mirror::Start::Replay { records, .. } => {
            for chunk in records.chunks(BATCH_RECORDS) {
                let b = batch(chunk.iter().map(|(s, r)| to_api(*s, r)).collect());
                conn.event(TOPIC_RECORDS, b.encode_to_vec()).await;
            }
        }
    }
    let task_conn = conn.clone();
    let task = tokio::spawn(stream(
        task_conn,
        rx,
        ended,
        host,
        sub,
        p.pane,
        started.generation,
        move |r| batch(r).encode_to_vec(),
    ));
    conn.subs.lock().await.insert(sub, task);
}

/// Forward a subscriber's records as batches until its stream ends, then
/// say why. A client that does not read makes this task wait, the
/// subscriber queue fill, and the owner end the subscription as lagged.
#[allow(clippy::too_many_arguments)]
async fn stream(
    conn: Conn,
    mut rx: mpsc::Receiver<(u64, mirror::Record)>,
    ended: Arc<AtomicU8>,
    host: mirror::MirrorHost,
    sub: u64,
    pane: u32,
    generation: domain::PaneGeneration,
    encode: impl Fn(Vec<api::TerminalRecord>) -> Vec<u8> + Send,
) {
    while let Some((seq, r)) = rx.recv().await {
        let mut records = vec![to_api(seq, &r)];
        let mut bytes = records[0].data.len();
        while records.len() < BATCH_RECORDS && bytes < BATCH_BYTES {
            match rx.try_recv() {
                Ok((seq, r)) => {
                    let r = to_api(seq, &r);
                    bytes += r.data.len();
                    records.push(r);
                }
                Err(_) => break,
            }
        }
        if !conn.event(TOPIC_RECORDS, encode(records)).await {
            return;
        }
    }
    let reason = match ended.load(Ordering::SeqCst) {
        mirror::end::LAGGED => api::EndReason::Lagged,
        mirror::end::CONNECTION_LOST => api::EndReason::ConnectionLost,
        _ => {
            // The mirror went away: a new generation means a respawn.
            let respawned = host
                .status()
                .await
                .iter()
                .any(|s| s.pane.0 == pane && s.generation != generation);
            if respawned {
                api::EndReason::Respawned
            } else {
                api::EndReason::PaneClosed
            }
        }
    };
    let end = api::SubscriptionEnd {
        subscription: sub,
        reason: reason as i32,
        detail: None,
    };
    conn.event(TOPIC_END, end.encode_to_vec()).await;
    conn.subs.lock().await.remove(&sub);
}
