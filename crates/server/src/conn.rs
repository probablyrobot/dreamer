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

pub const TOPIC_RECORDS: &str = api::topic::TERMINAL_RECORDS;
pub const TOPIC_END: &str = api::topic::TERMINAL_END;
pub const TOPIC_CHECKPOINT: &str = api::topic::TERMINAL_CHECKPOINT;
pub const TOPIC_TOPOLOGY: &str = api::topic::TMUX_TOPOLOGY;
pub const TOPIC_EVENTS: &str = api::topic::EVENTS_BATCH;
pub const TOPIC_MESSAGES: &str = api::topic::MESSAGES_DELIVER;
pub const TOPIC_TAPS: &str = api::topic::TAPS_DATA;
pub const TOPIC_SESSIONS: &str = api::topic::AGENT_SESSIONS_EVENTS;
pub const TOPIC_APP_INPUT: &str = api::topic::APPS_INPUT;

/// Connection ids, for the broker; unique within a daemon.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

enum Out {
    Frame(Kind, Vec<u8>),
    Bulk(Vec<u8>),
}

#[derive(Clone)]
struct Conn {
    out: mpsc::Sender<Out>,
    subs: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
    /// Clients that asked to size windows through this connection: they
    /// give up ownership when it closes.
    sizers: Arc<Mutex<std::collections::HashSet<u64>>>,
    /// The `tmux.watch` task, once this connection asked for one.
    watch: Arc<Mutex<Option<JoinHandle<()>>>>,
    next_sub: Arc<AtomicU64>,
    /// The negotiated frame limit: a reply larger than this is refused
    /// with an error instead of being dropped.
    max_frame: u32,
    events: Arc<AtomicU64>,
    /// What this connection may do: the owner grant for the user's own
    /// clients, a narrow one after an app registers.
    grant: Arc<std::sync::Mutex<crate::grants::Grant>>,
    /// The connecting process, from the socket's peer credentials.
    peer_pid: Option<u32>,
    id: u64,
    /// Who this connection is when it is not the user: a registered app
    /// or a supervised extension.
    identity: Arc<std::sync::Mutex<Option<crate::messages::Identity>>>,
    /// The message rate limit.
    bucket: Arc<std::sync::Mutex<crate::messages::Bucket>>,
}

impl Conn {
    /// This connection as a message sender and recipient.
    fn who(&self) -> crate::messages::Who {
        let owner = self.grant.lock().is_ok_and(|g| g.0.owner);
        match self.identity.lock().ok().and_then(|r| r.clone()) {
            Some(i) => crate::messages::Who {
                conn: self.id,
                instance: i.instance,
                app_id: i.app_id,
                pane: i.pane,
                owner,
            },
            None => crate::messages::Who {
                conn: self.id,
                instance: format!("owner:{}", self.id),
                app_id: None,
                pane: None,
                owner,
            },
        }
    }
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
    let peer_pid = stream
        .peer_cred()
        .ok()
        .and_then(|c| c.pid())
        .and_then(|p| u32::try_from(p).ok());
    // The socket is owner-only: whoever connects is the user.
    connection_as(stream, state, peer_pid, crate::grants::Grant::owner(), None).await;
}

/// Run [`connection_as`] as its own task. A plain function, so a method
/// that starts a connection (a script loading) does not make the
/// connection's future part of its own type.
pub fn spawn_connection(
    stream: UnixStream,
    state: Arc<State>,
    peer_pid: Option<u32>,
    grant: crate::grants::Grant,
    identity: Option<crate::messages::Identity>,
) {
    tokio::spawn(connection_as(stream, state, peer_pid, grant, identity));
}

/// A connection whose peer process and starting grant the daemon already
/// knows: the JSON-RPC endpoint and supervised extensions reach the
/// service this way, over an in-process socket.
pub async fn connection_as(
    stream: UnixStream,
    state: Arc<State>,
    peer_pid: Option<u32>,
    grant: crate::grants::Grant,
    identity: Option<crate::messages::Identity>,
) {
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
        watch: Arc::default(),
        sizers: Arc::default(),
        next_sub: Arc::new(AtomicU64::new(1)),
        max_frame: negotiated.limits.max_frame,
        grant: Arc::new(std::sync::Mutex::new(grant)),
        peer_pid,
        events: Arc::new(AtomicU64::new(1)),
        id: CONNECTIONS.fetch_add(1, Ordering::Relaxed) + 1,
        identity: Arc::new(std::sync::Mutex::new(identity)),
        bucket: Arc::default(),
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
                    // The grant decides first, before anything is acted on.
                    if let Some(refused) = refuse(&state, &conn, &req) {
                        let _ = conn
                            .out
                            .send(Out::Frame(Kind::Response, refused.encode_to_vec()))
                            .await;
                        return;
                    }
                    // Then the act level: acting on panes this connection
                    // did not create may wait here for the user's approval.
                    if let Err(e) = act_level(&state, &conn, &req).await {
                        let _ = conn
                            .out
                            .send(Out::Frame(
                                Kind::Response,
                                failure(req.id, e).encode_to_vec(),
                            ))
                            .await;
                        return;
                    }
                    if req.method.starts_with("agent.sessions.") {
                        // Sends its own response, and for a follow the stream.
                        agent_sessions(&state, &conn, req).await;
                        return;
                    }
                    if req.method == api::method::APPS_REGISTER {
                        let response = register(&state, &conn, &req).await;
                        let _ = conn
                            .out
                            .send(Out::Frame(Kind::Response, response.encode_to_vec()))
                            .await;
                        return;
                    }
                    if req.method == api::method::GRANTS_NARROW {
                        let response = match api::Grant::decode(req.payload.as_slice()) {
                            Err(e) => {
                                failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string()))
                            }
                            Ok(wanted) => {
                                let mut g = match conn.grant.lock() {
                                    Ok(g) => g,
                                    Err(_) => return,
                                };
                                match crate::grants::within(&g.0, &wanted) {
                                    Ok(()) => {
                                        *g = crate::grants::Grant(wanted.clone());
                                        ok(req.id, wanted.encode_to_vec())
                                    }
                                    Err(why) => {
                                        failure(req.id, RpcError::new(code::FORBIDDEN, why))
                                    }
                                }
                            }
                        };
                        let _ = conn
                            .out
                            .send(Out::Frame(Kind::Response, response.encode_to_vec()))
                            .await;
                        return;
                    }
                    if req.method == api::method::APPS_CALL {
                        let response = app_call(&state, &service, &conn, &req).await;
                        let _ = conn
                            .out
                            .send(Out::Frame(
                                Kind::Response,
                                fit(response, conn.max_frame).encode_to_vec(),
                            ))
                            .await;
                        return;
                    }
                    if req.method == api::method::APPS_PUBLISH {
                        let response = match api::AppViewPublish::decode(req.payload.as_slice()) {
                            Err(e) => {
                                failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string()))
                            }
                            Ok(p) => match state.apps.publish(conn.id, p.view) {
                                Ok(()) => ok(req.id, api::Empty {}.encode_to_vec()),
                                Err(e) => failure(req.id, e),
                            },
                        };
                        let _ = conn
                            .out
                            .send(Out::Frame(Kind::Response, response.encode_to_vec()))
                            .await;
                        return;
                    }
                    if req.method == api::method::APPS_PUBLISH_TREE {
                        let response = match api::AppTreePublish::decode(req.payload.as_slice()) {
                            Err(e) => {
                                failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string()))
                            }
                            Ok(p) => match state.apps.publish_tree(conn.id, &p, &|pane| {
                                may_read_pane(&state, &conn, pane)
                            }) {
                                Ok(()) => ok(req.id, api::Empty {}.encode_to_vec()),
                                Err(e) => failure(req.id, e),
                            },
                        };
                        let _ = conn
                            .out
                            .send(Out::Frame(Kind::Response, response.encode_to_vec()))
                            .await;
                        return;
                    }
                    if req.method == api::method::GRANTS_CURRENT {
                        let g = conn.grant.lock().map(|g| g.0.clone()).unwrap_or_default();
                        let _ = conn
                            .out
                            .send(Out::Frame(
                                Kind::Response,
                                ok(req.id, g.encode_to_vec()).encode_to_vec(),
                            ))
                            .await;
                        return;
                    }
                    let shutdown = req.method == api::method::DAEMON_SHUTDOWN;
                    if req.method == api::method::TERMINAL_SUBSCRIBE {
                        // Sends its own response, then the stream.
                        subscribe(&service, &conn, req).await;
                        return;
                    }
                    if req.method == api::method::HOSTS_SUBSCRIBE {
                        host_subscribe(&state, &conn, req).await;
                        return;
                    }
                    if req.method == api::method::MESSAGES_SUBSCRIBE {
                        messages_subscribe(&state, &conn, req).await;
                        return;
                    }
                    if let Some(response) = messages(&state, &conn, &req).await {
                        let _ = conn
                            .out
                            .send(Out::Frame(
                                Kind::Response,
                                fit(response, conn.max_frame).encode_to_vec(),
                            ))
                            .await;
                        return;
                    }
                    if req.method == api::method::EVENTS_SUBSCRIBE {
                        events_subscribe(&state, &conn, req).await;
                        return;
                    }
                    if req.method == api::method::TAPS_SUBSCRIBE {
                        taps_subscribe(&service, &conn, req).await;
                        return;
                    }
                    if req.method == api::method::HOSTS_WATCH {
                        host_watch(&state, &conn, req).await;
                        return;
                    }
                    let watch = req.method == api::method::TMUX_WATCH;
                    if req.method == api::method::TMUX_SIZE_WINDOW
                        && let Ok(p) = api::SizeWindow::decode(req.payload.as_slice())
                    {
                        conn.sizers.lock().await.insert(p.client);
                    }
                    let response = if req.method == api::method::TERMINAL_UNSUBSCRIBE
                        || req.method == api::method::EVENTS_UNSUBSCRIBE
                        || req.method == api::method::MESSAGES_UNSUBSCRIBE
                        || req.method == api::method::TAPS_UNSUBSCRIBE
                    {
                        unsubscribe(&conn, req).await
                    } else {
                        let caller = crate::leases::Caller {
                            conn: conn.id,
                            owner: conn.grant.lock().is_ok_and(|g| g.0.owner),
                            level: conn.grant.lock().map_or(0, |g| g.0.level),
                        };
                        crate::leases::CALLER
                            .scope(caller, answer(&service, req))
                            .await
                    };
                    let response = fit(response, conn.max_frame);
                    if watch && response.error_code.is_none() {
                        follow_topology(&state, &conn, &response).await;
                    }
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
    if let Some(task) = conn.watch.lock().await.take() {
        task.abort();
    }
    state.leases.drop_conn(conn.id);
    state.approvals.drop_conn(conn.id);
    state.captures.drop_conn(conn.id);
    // An app host that left: its panes keep their last view, as stale.
    if state.apps.closed(conn.id)
        && let Some(ep) = state.endpoint.lock().await.as_ref()
    {
        crate::service::publish_topology(&state, ep);
    }
    let sizers: Vec<u64> = conn.sizers.lock().await.drain().collect();
    crate::service::release_sizing(&state, &sizers).await;
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

/// The connection's grant refuses this request: the error reply to send.
/// A request that does not decode is left to the method's own handling,
/// which reports it; it acts on nothing.
fn refuse(state: &State, conn: &Conn, req: &wire::Request) -> Option<wire::Response> {
    let grant = conn.grant.lock().ok()?.clone();
    if grant.0.owner {
        return None;
    }
    // The user starts apps and hands out grants. An agent at `own` or
    // `elsewhere` may too, for a grant within its own, and may watch and
    // capture the app panes it launched; an app's grant can never include
    // any of that, whatever it lists.
    if [
        api::method::APPS_LAUNCH,
        api::method::APPS_TREE,
        api::method::APPS_CAPTURE,
    ]
    .contains(&req.method.as_str())
        && let Err(e) = agent_apps(state, conn, &grant, req)
    {
        return Some(failure(req.id, e));
    }
    // Pages run in the user's desktop: an agent acting at `own` or
    // `elsewhere` may open one or load another address in one (under the
    // approvals below), an app never.
    if [api::method::WEB_OPEN, api::method::WEB_NAVIGATE].contains(&req.method.as_str())
        && grant.0.level < api::ActLevel::Own as i32
    {
        return Some(failure(
            req.id,
            RpcError::new(
                code::FORBIDDEN,
                format!(
                    "only the user's own clients and agents acting at own or elsewhere may call {}",
                    req.method
                ),
            ),
        ));
    }
    // A registered app host publishes its own view whatever its grant
    // lists; the registry refuses any other connection.
    if req.method == api::method::APPS_PUBLISH || req.method == api::method::APPS_PUBLISH_TREE {
        return None;
    }
    // Likewise for starting and stopping extensions and scripts, which
    // hands out their grants.
    // Approvals are the user's to answer: an agent never approves itself.
    if [
        api::method::SCRIPTS_LOAD,
        api::method::SCRIPTS_RELOAD,
        api::method::SCRIPTS_STOP,
        api::method::APPROVALS_LIST,
        api::method::APPROVALS_ANSWER,
        // The drawer starts the user's harness with the user's authority.
        api::method::DRAWER_TOGGLE,
        // App panes are the user's to see and type into.
        api::method::APPS_VIEW,
        api::method::APPS_INPUT,
        // Images of app panes are the user's desktop's to make.
        api::method::APPS_CAPTURE_WAIT,
        api::method::APPS_CAPTURE_DONE,
        // The desktop drawing a page reports its state.
        api::method::WEB_STATE,
        // An app's page calls through the user's desktop.
        api::method::APPS_CALL,
    ]
    .contains(&req.method.as_str())
    {
        return Some(failure(
            req.id,
            RpcError::new(
                code::FORBIDDEN,
                format!("only the user's own clients may call {}", req.method),
            ),
        ));
    }
    if req.method.starts_with("filters.") {
        return match app_filters(state, &grant, req) {
            Err(e) => Some(failure(req.id, e)),
            // An app lists only the filters it may manage.
            Ok(Some(list)) => Some(wire::Response {
                id: req.id,
                payload: api::Response::FiltersList(list).encode(),
                error_code: None,
                error_message: String::new(),
            }),
            Ok(None) => None,
        };
    }
    if req.method == api::method::EXTENSIONS_RESTART {
        return Some(failure(
            req.id,
            RpcError::new(
                code::FORBIDDEN,
                "only the user's own clients restart extensions",
            ),
        ));
    }
    if !grant.method_allowed(&req.method) {
        return Some(failure(
            req.id,
            RpcError::new(
                code::FORBIDDEN,
                format!("`{}` is not in this connection's grant", req.method),
            ),
        ));
    }
    let request = api::Request::decode(&req.method, &req.payload).ok()?;
    // Panes this connection created are in its scope too.
    grant
        .check_owning(&req.method, &request, |p| state.approvals.owns(conn.id, p))
        .err()
        .map(|e| failure(req.id, e))
}

/// Whether a connection that is not the owner may launch an app, or read
/// the view tree of or capture an app pane. Only an agent's grant at `own`
/// or `elsewhere` with the method in it may, and never a registered app
/// (whose grant has no level). A launch hands out a grant, which must be
/// within the agent's own and at its level at most; the tree and images
/// are of the app panes it launched. The ordinary checks (pane scope, act
/// level, approvals) still follow.
fn agent_apps(
    state: &State,
    conn: &Conn,
    grant: &crate::grants::Grant,
    req: &wire::Request,
) -> Result<(), RpcError> {
    let no = |why: String| Err(RpcError::new(code::FORBIDDEN, why));
    if grant.0.level < api::ActLevel::Own as i32 {
        return no(if req.method == api::method::APPS_LAUNCH {
            "only the user's own clients and agents acting at own or elsewhere launch apps".into()
        } else {
            format!(
                "only the user's own clients, and agents for the app panes they launched, may call {}",
                req.method
            )
        });
    }
    if !grant.method_allowed(&req.method) {
        return no(format!(
            "`{}` is not in this connection's grant",
            req.method
        ));
    }
    let Ok(request) = api::Request::decode(&req.method, &req.payload) else {
        // Left to the method, which reports it; it acts on nothing.
        return Ok(());
    };
    let pane = match &request {
        api::Request::AppsLaunch(p) => {
            let mut wanted = p.grant.clone().unwrap_or_default();
            if wanted.level == api::ActLevel::Unspecified as i32 {
                wanted.level = grant.0.level;
            }
            return crate::grants::within(&grant.0, &wanted)
                .or_else(|why| no(format!("the app's grant: {why}")));
        }
        api::Request::AppsTree(p) => p.pane,
        api::Request::AppsCapture(p) => p.pane,
        _ => return Ok(()),
    };
    if state.approvals.owns(conn.id, pane) {
        Ok(())
    } else {
        no(format!(
            "pane %{pane} is not an app this connection launched; {} is for those",
            req.method
        ))
    }
}

/// The act level of the connection's grant, for a request that acts on
/// panes (see approvals.rs). A request that does not decode acts on
/// nothing.
async fn act_level(state: &State, conn: &Conn, req: &wire::Request) -> Result<(), RpcError> {
    let Some(grant) = conn.grant.lock().ok().map(|g| g.0.clone()) else {
        return Ok(());
    };
    if grant.owner || grant.level == api::ActLevel::Unspecified as i32 {
        return Ok(());
    }
    let Ok(request) = api::Request::decode(&req.method, &req.payload) else {
        return Ok(());
    };
    let requester = conn.who().instance;
    crate::approvals::gate(state, conn.id, &requester, &grant, &req.method, &request).await
}

/// Whether the connection's grant lets it read pane `pane`'s content, as
/// `terminal.read` would: what an app's `terminal` node may show.
fn may_read_pane(state: &State, conn: &Conn, pane: u32) -> bool {
    let Some(grant) = conn.grant.lock().ok().map(|g| g.clone()) else {
        return false;
    };
    let read = api::Request::TerminalRead(api::ReadParams {
        pane,
        history: None,
        endpoint: None,
    });
    grant
        .check_owning(api::method::TERMINAL_READ, &read, |p| {
            state.approvals.owns(conn.id, p)
        })
        .is_ok()
}

/// What an app may do with filters (spec 13.4: a filter pane). Only with
/// `filters.manage` in its grant, and content read (`terminal.query`,
/// `terminal.read`, or `taps.subscribe`):
/// - install a filter that only reads: on panes within the app's own, with
///   a grant of at most `taps.subscribe` on them, and no output that acts
///   (`topic`, `recipient`, `input`); it may record events and decorate;
/// - pause, resume, or remove such a filter, never one the user installed;
/// - list those filters (answered here: `Some`).
fn app_filters(
    state: &State,
    grant: &crate::grants::Grant,
    req: &wire::Request,
) -> Result<Option<api::FilterList>, RpcError> {
    let no = |why: String| Err(RpcError::new(code::FORBIDDEN, why));
    if !grant.method_allowed(api::MANAGE_FILTERS) {
        return no(format!(
            "`{}` needs `{}` in this connection's grant",
            req.method,
            api::MANAGE_FILTERS
        ));
    }
    let reads = [
        api::method::TERMINAL_QUERY,
        api::method::TERMINAL_READ,
        api::method::TAPS_SUBSCRIBE,
    ];
    if !reads.iter().any(|m| grant.method_allowed(m)) {
        return no("managing filters needs content read (`terminal.query`) too".into());
    }
    let allowed = api::Grant {
        owner: false,
        methods: vec![api::method::TAPS_SUBSCRIBE.into()],
        panes: grant.0.panes.clone(),
        all_panes: grant.0.all_panes,
        expires_unix_ms: grant.0.expires_unix_ms,
        level: 0,
    };
    let bad = |e: String| RpcError::new(code::INVALID_PARAMS, e);
    match api::Request::decode(&req.method, &req.payload).map_err(|e| bad(e.to_string()))? {
        api::Request::FiltersList(_) => Ok(Some(state.filters.list_for_app(&allowed))),
        api::Request::FiltersInstall(spec) => {
            if spec.topic.is_some() || spec.recipient.is_some() || spec.input.is_some() {
                return no(
                    "an app's filter only reads: it may record events and decorate, \
                     not publish, send, or type"
                        .into(),
                );
            }
            let g = spec.grant.clone().unwrap_or_default();
            let wanted = api::Grant {
                owner: false,
                methods: g.methods,
                panes: g.panes,
                all_panes: g.all_panes,
                // An app's filter ends with the app's grant.
                expires_unix_ms: grant.0.expires_unix_ms,
                level: 0,
            };
            let watched = api::Grant {
                panes: spec.panes.clone(),
                all_panes: spec.every_pane,
                ..allowed.clone()
            };
            crate::grants::within(&allowed, &wanted)
                .and_then(|()| crate::grants::within(&allowed, &watched))
                .map(|()| None)
                .or_else(no)
        }
        api::Request::FiltersPause(r)
        | api::Request::FiltersResume(r)
        | api::Request::FiltersRemove(r) => state
            .filters
            .app_may_manage(&r.name, &allowed)
            .map(|()| None)
            .or_else(no),
        _ => no(format!(
            "`{}` is not in this connection's grant",
            req.method
        )),
    }
}

/// What a desktop asks for the app in pane `pane` (a request's
/// `for_pane`), checked against that app's grant as if the app asked:
/// `method`, the panes `request` names, the grant's expiry, and `also`
/// (with the app as the caller).
fn app_may(
    state: &State,
    pane: u32,
    method: &str,
    request: &api::Request,
    also: impl FnOnce(&crate::sessions::Caller) -> Result<(), RpcError>,
) -> Result<(), RpcError> {
    let (instance, grant) = state.apps.app_of(pane)?;
    let in_app = |e: RpcError| {
        RpcError::new(
            e.code,
            format!("for the app in pane %{pane}: {}", e.message),
        )
    };
    crate::grants::Grant(grant.clone())
        .check_owning(method, request, |_| false)
        .map_err(in_app)?;
    also(&crate::sessions::Caller { instance, grant }).map_err(in_app)
}

/// Whether a web page's bridge may call `method`: a request answered
/// once, not a stream, nor what only an app host or the connection itself
/// asks (registering, publishing, narrowing, messages, sessions).
fn bridged(method: &str) -> bool {
    use api::method as m;
    let streams = [
        m::TERMINAL_SUBSCRIBE,
        m::TERMINAL_UNSUBSCRIBE,
        m::EVENTS_SUBSCRIBE,
        m::EVENTS_UNSUBSCRIBE,
        m::TAPS_SUBSCRIBE,
        m::TAPS_UNSUBSCRIBE,
        m::HOSTS_SUBSCRIBE,
        m::HOSTS_WATCH,
        m::TMUX_WATCH,
        m::TMUX_SIZE_WINDOW,
    ];
    let own = [
        m::APPS_REGISTER,
        m::APPS_PUBLISH,
        m::APPS_PUBLISH_TREE,
        m::APPS_CALL,
        m::GRANTS_NARROW,
        m::GRANTS_CURRENT,
        m::DAEMON_SHUTDOWN,
    ];
    !streams.contains(&method)
        && !own.contains(&method)
        && !method.starts_with("messages.")
        && !method.starts_with("agent.sessions.")
}

/// `apps.call`: a call an app's web page makes through its bridge, passed
/// on by the user's desktop. It runs as the app: with its host's
/// connection id (the panes the app created are its own) and identity,
/// and through the same checks as a request on that connection (the
/// app's grant, its act level), never with the desktop's own authority.
async fn app_call(
    state: &Arc<State>,
    service: &DaemonService,
    conn: &Conn,
    req: &wire::Request,
) -> wire::Response {
    let fail = |code: i32, why: String| failure(req.id, RpcError::new(code, why));
    let p = match api::AppCall::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => return fail(code::INVALID_PARAMS, e.to_string()),
    };
    if !bridged(&p.method) {
        return fail(
            code::FORBIDDEN,
            format!("`{}` is not available to a web page", p.method),
        );
    }
    let app = match state.apps.caller(p.pane) {
        Ok(a) => a,
        Err(e) => return failure(req.id, e),
    };
    let params = if p.params.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&p.params) {
            Ok(v) => v,
            Err(e) => return fail(code::INVALID_PARAMS, format!("params: {e}")),
        }
    };
    let request = match api::Request::from_json(&p.method, params) {
        Ok(r) => r,
        Err(e) => return failure(req.id, e.into()),
    };
    let inner = wire::Request {
        id: req.id,
        method: p.method.clone(),
        payload: request.encode(),
        deadline_ms: req.deadline_ms,
    };
    let level = app.grant.level;
    let as_app = Conn {
        grant: Arc::new(std::sync::Mutex::new(crate::grants::Grant(app.grant))),
        id: app.conn,
        identity: Arc::new(std::sync::Mutex::new(Some(crate::messages::Identity {
            instance: app.instance,
            app_id: Some(app.app_id),
            pane: Some(p.pane),
        }))),
        ..conn.clone()
    };
    if let Some(refused) = refuse(state, &as_app, &inner) {
        return refused;
    }
    if let Err(e) = act_level(state, &as_app, &inner).await {
        return failure(req.id, e);
    }
    let caller = crate::leases::Caller {
        conn: as_app.id,
        owner: false,
        level,
    };
    let response = crate::leases::CALLER
        .scope(caller, answer(service, inner))
        .await;
    if response.error_code.is_some() {
        return response;
    }
    match api::Response::decode(&p.method, &response.payload) {
        Ok(r) => ok(
            req.id,
            api::AppCallResult {
                result: r.to_json().to_string(),
            }
            .encode_to_vec(),
        ),
        Err(e) => failure(req.id, e.into()),
    }
}

/// `apps.register`: check the ticket and that this connection's process
/// runs in the launch's pane, then hold the app's grant from now on.
async fn register(state: &Arc<State>, conn: &Conn, req: &wire::Request) -> wire::Response {
    let p = match api::RegisterApp::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => return failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string())),
    };
    // A process an agent session's harness started takes the session's
    // grant.
    if p.launch_id.starts_with("session:") {
        return match state
            .sessions
            .register(&p.launch_id, &p.ticket, conn.peer_pid)
            .await
        {
            Ok(r) => {
                if let (Ok(mut g), Some(narrow)) = (conn.grant.lock(), r.grant.clone()) {
                    *g = crate::grants::Grant(narrow);
                }
                if let Ok(mut me) = conn.identity.lock() {
                    *me = Some(crate::messages::Identity {
                        instance: r.instance.clone(),
                        app_id: Some(r.app_id.clone()),
                        pane: None,
                    });
                }
                ok(req.id, r.encode_to_vec())
            }
            Err(e) => failure(req.id, e),
        };
    }
    let state2 = state.clone();
    let pane_pid = |pane: u32| async move {
        let guard = state2.endpoint.lock().await;
        let ep = guard.as_ref()?;
        tmux_backend::ops::pane_pid(&ep.conn, domain::TmuxPaneId(pane))
            .await
            .ok()
    };
    match state.apps.register(&p, conn.peer_pid, pane_pid).await {
        Ok(r) => {
            if let (Ok(mut g), Some(narrow)) = (conn.grant.lock(), r.grant.clone()) {
                *g = crate::grants::Grant(narrow);
            }
            state.events.app(&p.launch_id, r.pane);
            if let Ok(mut me) = conn.identity.lock() {
                *me = Some(crate::messages::Identity {
                    instance: r.instance.clone(),
                    app_id: Some(r.app_id.clone()),
                    pane: Some(r.pane),
                });
            }
            // The pane is an app pane now: keys for it come here, and
            // watchers see it marked.
            let granted = r.grant.clone().unwrap_or_default();
            let mut keys = state
                .apps
                .host(r.pane, &r.instance, &r.app_id, &granted, conn.id);
            let (to, pane) = (conn.clone(), r.pane);
            tokio::spawn(async move {
                while let Some(mut input) = keys.recv().await {
                    input.pane = pane;
                    if !to.event(TOPIC_APP_INPUT, input.encode_to_vec()).await {
                        return;
                    }
                }
            });
            if let Some(ep) = state.endpoint.lock().await.as_ref() {
                crate::service::publish_topology(state, ep);
            }
            ok(req.id, r.encode_to_vec())
        }
        Err(e) => failure(req.id, e),
    }
}

/// `agent.sessions.*`, answered here because they need the caller: its
/// grant bounds a new session's, and a narrow caller sees only the
/// sessions it created. `agent.sessions.events` with `follow` replies, then
/// pushes `agent.sessions.events` until it is unsubscribed or the session
/// closes.
async fn agent_sessions(state: &Arc<State>, conn: &Conn, req: wire::Request) {
    use api::Request as R;
    let send = |r: wire::Response| async {
        let _ = conn
            .out
            .send(Out::Frame(
                Kind::Response,
                fit(r, conn.max_frame).encode_to_vec(),
            ))
            .await;
    };
    let request = match api::Request::decode(&req.method, &req.payload) {
        Ok(r) => r,
        Err(e) => return send(failure(req.id, e.into())).await,
    };
    let caller = crate::sessions::Caller {
        instance: conn.who().instance,
        grant: conn.grant.lock().map(|g| g.0.clone()).unwrap_or_default(),
    };
    let sessions = &state.sessions;
    // A desktop drawing an app's bound transcript reads and answers for
    // that app: the app's grant must allow it too.
    let for_app = match &request {
        R::AgentSessionsEvents(q) => q.for_pane.map(|p| (p, q.session.clone())),
        R::AgentSessionsApprove(a) | R::AgentSessionsDeny(a) => {
            a.for_pane.map(|p| (p, a.session.clone()))
        }
        _ => None,
    };
    if let Some((pane, session)) = for_app
        && let Err(e) = app_may(state, pane, &req.method, &request, |caller| {
            sessions.get(caller, &session).map(|_| ())
        })
    {
        return send(failure(req.id, e)).await;
    }
    let one = |r: Result<api::AgentSession, RpcError>| match r {
        Ok(s) => ok(req.id, s.encode_to_vec()),
        Err(e) => failure(req.id, e),
    };
    let response = match request {
        R::AgentSessionsCreate(p) => one(sessions.create(&caller, p).await),
        R::AgentSessionsSend(p) => one(sessions.send(&caller, p, &state.memory).await),
        R::AgentSessionsCancel(p) => one(sessions.cancel(&caller, p).await),
        R::AgentSessionsApprove(p) => one(sessions.answer(&caller, p, true).await),
        R::AgentSessionsDeny(p) => one(sessions.answer(&caller, p, false).await),
        R::AgentSessionsResume(p) => one(sessions.resume(&caller, p).await),
        R::AgentSessionsClose(p) => one(sessions.close(&caller, p).await),
        R::AgentSessionsList(_) => ok(req.id, sessions.list(&caller).encode_to_vec()),
        R::AgentSessionsEvents(q) => {
            let s = match sessions.get(&caller, &q.session) {
                Ok(s) => s,
                Err(e) => return send(failure(req.id, e)).await,
            };
            let after = q.after.unwrap_or(0);
            if !q.follow {
                let (events, more) = s.read(after, crate::sessions::limit(&q));
                let r = api::AgentSessionEvents {
                    session: q.session,
                    subscription: None,
                    events,
                    more,
                };
                return send(ok(req.id, r.encode_to_vec())).await;
            }
            let id = conn.next_sub.fetch_add(1, Ordering::Relaxed);
            // Subscribed before the transcript is read, so nothing falls
            // between the two.
            let mut live = s.subscribe();
            let (go, ready) = tokio::sync::oneshot::channel::<()>();
            let task_conn = conn.clone();
            let session = q.session.clone();
            let task = tokio::spawn(async move {
                if ready.await.is_err() {
                    return;
                }
                let push = |events: Vec<api::AgentSessionEvent>| {
                    let c = task_conn.clone();
                    let session = session.clone();
                    async move {
                        let b = api::AgentSessionEvents {
                            session,
                            subscription: Some(id),
                            events,
                            more: false,
                        };
                        c.event(TOPIC_SESSIONS, b.encode_to_vec()).await
                    }
                };
                let closed = |e: &api::AgentSessionEvent| {
                    matches!(&e.kind, Some(api::agent_session_event::Kind::Status(st))
                        if st.state == api::AgentSessionState::Closed as i32)
                };
                let mut last = after;
                let mut ended = false;
                // The transcript first, then live events; after falling
                // behind, the transcript again from where this left off.
                'stream: loop {
                    loop {
                        let (events, more) = s.read(last, 512);
                        if let Some(e) = events.last() {
                            last = e.seq;
                        }
                        ended |= events.iter().any(closed);
                        if !events.is_empty() && !push(events).await {
                            return;
                        }
                        if !more {
                            break;
                        }
                    }
                    if ended {
                        break;
                    }
                    loop {
                        let first = match live.recv().await {
                            Ok(e) => e,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue 'stream;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break 'stream;
                            }
                        };
                        let mut batch = vec![first];
                        while batch.len() < 512 {
                            match live.try_recv() {
                                Ok(e) => batch.push(e),
                                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                    continue 'stream;
                                }
                                Err(_) => break,
                            }
                        }
                        batch.retain(|e| e.seq > last);
                        let Some(e) = batch.last() else { continue };
                        if e.seq != last + batch.len() as u64 {
                            // A gap: read it from the transcript.
                            continue 'stream;
                        }
                        last = e.seq;
                        ended = batch.iter().any(closed);
                        if !push(batch).await {
                            return;
                        }
                        if ended {
                            break 'stream;
                        }
                    }
                }
                let end = api::SubscriptionEnd {
                    subscription: id,
                    reason: api::EndReason::Unspecified as i32,
                    detail: Some("the agent session closed".into()),
                };
                task_conn.event(TOPIC_END, end.encode_to_vec()).await;
                task_conn.subs.lock().await.remove(&id);
            });
            conn.subs.lock().await.insert(id, task);
            let r = api::AgentSessionEvents {
                session: q.session,
                subscription: Some(id),
                events: Vec::new(),
                more: false,
            };
            send(ok(req.id, r.encode_to_vec())).await;
            let _ = go.send(());
            return;
        }
        _ => failure(
            req.id,
            RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("unknown method {}", req.method),
            ),
        ),
    };
    send(response).await;
}

/// A reply that would not fit in one frame becomes an error saying so.
/// Sending it would fail in the writer and end the connection, leaving
/// the caller waiting.
fn fit(r: wire::Response, max_frame: u32) -> wire::Response {
    let len = r.encoded_len();
    if len + wire::HEADER_LEN <= max_frame as usize {
        return r;
    }
    failure(
        r.id,
        RpcError::new(
            code::PRECONDITION,
            format!(
                "the result is {len} bytes, more than the {max_frame}-byte frame limit; \
                 ask for less (for example, fewer history lines)"
            ),
        ),
    )
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

/// After a `tmux.watch` reply, push every newer topology as a
/// `tmux.topology` event: never an older one, and only the newest when the
/// client reads slowly. One watcher per connection.
async fn follow_topology(state: &Arc<State>, conn: &Conn, reply: &wire::Response) {
    follow(conn, state.topology.subscribe(), reply).await;
}

/// Push every topology newer than the one in `reply` from `rx`.
async fn follow(
    conn: &Conn,
    mut rx: tokio::sync::watch::Receiver<Option<Arc<api::TopologyUpdate>>>,
    reply: &wire::Response,
) {
    let sent = api::TopologyUpdate::decode(reply.payload.as_slice()).map_or(0, |u| u.revision);
    let mut slot = conn.watch.lock().await;
    if slot.is_some() {
        return;
    }
    let conn = conn.clone();
    *slot = Some(tokio::spawn(async move {
        let mut last = sent;
        loop {
            let next = rx.borrow_and_update().clone();
            if let Some(u) = next.filter(|u| u.revision > last) {
                last = u.revision;
                if !conn.event(TOPIC_TOPOLOGY, u.encode_to_vec()).await {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }));
}

/// `hosts.watch`: the host's topology, then its changes as `tmux.topology`
/// events, across reconnects.
async fn host_watch(state: &Arc<State>, conn: &Conn, req: wire::Request) {
    let response = match api::HostRef::decode(req.payload.as_slice()) {
        Err(e) => failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string())),
        Ok(p) => match state.hosts.watch(&p.name).await {
            Err(e) => failure(req.id, e),
            Ok((first, rx)) => {
                let r = ok(req.id, first.encode_to_vec());
                follow(conn, rx, &r).await;
                r
            }
        },
    };
    let _ = conn
        .out
        .send(Out::Frame(Kind::Response, response.encode_to_vec()))
        .await;
}

/// `hosts.subscribe`: a relayed subscription under this connection's own
/// id, ended like any other with `terminal.unsubscribe`.
async fn host_subscribe(state: &Arc<State>, conn: &Conn, req: wire::Request) {
    let id = req.id;
    let send = |r: wire::Response| async move {
        let _ = conn
            .out
            .send(Out::Frame(Kind::Response, r.encode_to_vec()))
            .await;
    };
    let p = match api::HostSubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => {
            return send(failure(
                id,
                RpcError::new(code::INVALID_PARAMS, e.to_string()),
            ))
            .await;
        }
    };
    let mut relay = match state.hosts.subscribe(p).await {
        Ok(r) => r,
        Err(e) => return send(failure(id, e)).await,
    };
    let local = conn.next_sub.fetch_add(1, Ordering::Relaxed);
    let mut reply = relay.subscription.clone();
    reply.id = local;
    let (go, wait) = tokio::sync::oneshot::channel::<()>();
    let c = conn.clone();
    let task = tokio::spawn(async move {
        let _ = wait.await;
        while let Some(item) = relay.items.recv().await {
            let sent = match item {
                crate::hosts::RelayItem::Records(mut b) => {
                    b.subscription = local;
                    c.event(TOPIC_RECORDS, b.encode_to_vec()).await
                }
                crate::hosts::RelayItem::Checkpoint(mut d) => {
                    d.subscription = local;
                    let bulk = wire::Bulk {
                        stream: local,
                        topic: TOPIC_CHECKPOINT.into(),
                        data: d.encode_to_vec(),
                    };
                    c.out.send(Out::Bulk(bulk.encode_to_vec())).await.is_ok()
                }
                crate::hosts::RelayItem::End(mut e) => {
                    e.subscription = local;
                    c.event(TOPIC_END, e.encode_to_vec()).await;
                    c.subs.lock().await.remove(&local);
                    return;
                }
            };
            if !sent {
                return;
            }
        }
    });
    conn.subs.lock().await.insert(local, task);
    send(ok(id, reply.encode_to_vec())).await;
    let _ = go.send(());
}

/// `messages.publish`, `messages.request`, and `messages.reply`, sent as
/// this connection; `None` for any other method.
async fn messages(state: &Arc<State>, conn: &Conn, req: &wire::Request) -> Option<wire::Response> {
    let broker = &state.broker;
    let bad =
        |e: prost::DecodeError| failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string()));
    let result = match req.method.as_str() {
        api::method::MESSAGES_PUBLISH => {
            match api::MessagePublish::decode(req.payload.as_slice()) {
                Ok(p) => broker
                    .publish(&conn.who(), &conn.bucket, p)
                    .map(|r| r.encode_to_vec()),
                Err(e) => return Some(bad(e)),
            }
        }
        api::method::MESSAGES_REQUEST => {
            match api::MessageRequest::decode(req.payload.as_slice()) {
                Ok(p) => broker
                    .request(&conn.who(), &conn.bucket, p)
                    .await
                    .map(|r| r.encode_to_vec()),
                Err(e) => return Some(bad(e)),
            }
        }
        api::method::MESSAGES_REPLY => match api::MessageReply::decode(req.payload.as_slice()) {
            Ok(p) => broker
                .reply(&conn.who(), &conn.bucket, p)
                .map(|r| r.encode_to_vec()),
            Err(e) => return Some(bad(e)),
        },
        _ => return None,
    };
    Some(match result {
        Ok(payload) => ok(req.id, payload),
        Err(e) => failure(req.id, e),
    })
}

/// `messages.subscribe`: reply with this connection's recipient identity,
/// then push `messages.deliver` for each message. Messages arrive only as
/// these events: nothing is ever written to a pane.
async fn messages_subscribe(state: &Arc<State>, conn: &Conn, req: wire::Request) {
    let p = match api::MessageSubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => {
            let r = failure(req.id, RpcError::new(code::INVALID_PARAMS, e.to_string()));
            let _ = conn
                .out
                .send(Out::Frame(Kind::Response, r.encode_to_vec()))
                .await;
            return;
        }
    };
    let who = conn.who();
    let id = conn.next_sub.fetch_add(1, Ordering::Relaxed);
    let mut rx = state.broker.subscribe(&who, p.topics);
    let (go, ready) = tokio::sync::oneshot::channel::<()>();
    let task_conn = conn.clone();
    let task = tokio::spawn(async move {
        if ready.await.is_err() {
            return;
        }
        while let Some((envelope, missed)) = rx.recv().await {
            let d = api::MessageDelivery {
                subscription: id,
                envelope: Some(envelope),
                missed,
            };
            if !task_conn.event(TOPIC_MESSAGES, d.encode_to_vec()).await {
                return;
            }
        }
    });
    conn.subs.lock().await.insert(id, task);
    let r = api::MessageSubscription {
        id,
        instance: who.instance,
    };
    let _ = conn
        .out
        .send(Out::Frame(
            Kind::Response,
            ok(req.id, r.encode_to_vec()).encode_to_vec(),
        ))
        .await;
    let _ = go.send(());
}

impl crate::taps::Deliver for Conn {
    fn send(&self, d: api::TapData) -> impl std::future::Future<Output = bool> + Send {
        let c = self.clone();
        async move { c.event(TOPIC_TAPS, d.encode_to_vec()).await }
    }
}

/// `taps.subscribe`: reply with the panes followed, then push `taps.data`.
/// One task per pane; with `every_pane`, panes that appear later are
/// followed too.
async fn taps_subscribe(service: &DaemonService, conn: &Conn, req: wire::Request) {
    let reply = |r: wire::Response| {
        let conn = conn.clone();
        async move {
            let _ = conn
                .out
                .send(Out::Frame(Kind::Response, r.encode_to_vec()))
                .await;
        }
    };
    let p = match api::TapSubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => {
            return reply(failure(
                req.id,
                RpcError::new(code::INVALID_PARAMS, e.to_string()),
            ))
            .await;
        }
    };
    // A desktop drawing an app's bound log follows the tap for that app:
    // the app's grant must allow it too.
    if let Some(pane) = p.for_pane {
        let asked = api::Request::TapsSubscribe(api::TapSubscribe {
            for_pane: None,
            ..p.clone()
        });
        if let Err(e) = app_may(&service.0, pane, &req.method, &asked, |_| Ok(())) {
            return reply(failure(req.id, e)).await;
        }
    }
    let level = api::TapLevel::try_from(p.level).unwrap_or(api::TapLevel::Unspecified);
    if level == api::TapLevel::Unspecified {
        return reply(failure(
            req.id,
            RpcError::new(code::INVALID_PARAMS, "choose a level: raw, text, or meta"),
        ))
        .await;
    }
    let host = match service.mirror_host(None).await {
        Ok(h) => h,
        Err(e) => return reply(failure(req.id, e)).await,
    };
    let live = |host: &mirror::MirrorHost| {
        let host = host.clone();
        async move {
            host.status()
                .await
                .into_iter()
                .filter(|m| m.state == mirror::MirrorState::Live)
                .map(|m| m.pane.0)
                .collect::<Vec<u32>>()
        }
    };
    let panes = if p.every_pane {
        live(&host).await
    } else {
        let ids: Vec<domain::TmuxPaneId> = p.panes.iter().map(|&p| domain::TmuxPaneId(p)).collect();
        if !host.wait_live(&ids, Duration::from_secs(3)).await {
            return reply(failure(
                req.id,
                RpcError::new(code::NOT_FOUND, "a listed pane has no live mirror"),
            ))
            .await;
        }
        p.panes.clone()
    };
    let interval = Duration::from_millis(u64::from(p.interval_ms.unwrap_or(250).clamp(50, 10_000)));
    let id = conn.next_sub.fetch_add(1, Ordering::Relaxed);
    // Where each tap starts is taken before the reply, so nothing the panes
    // write after it is missed.
    let mut begun = Vec::with_capacity(panes.len());
    for &pane in &panes {
        begun.push((pane, crate::taps::begin(&host, pane, level).await));
    }
    let (go, ready) = tokio::sync::oneshot::channel::<()>();
    let (task_conn, every) = (conn.clone(), p.every_pane);
    let task = tokio::spawn(async move {
        if ready.await.is_err() {
            return;
        }
        let mut followed = std::collections::HashSet::new();
        let mut set = tokio::task::JoinSet::new();
        for (pane, begun) in begun {
            followed.insert(pane);
            set.spawn(crate::taps::follow(
                begun,
                host.clone(),
                task_conn.clone(),
                id,
                pane,
                level,
                interval,
            ));
        }
        if !every {
            while set.join_next().await.is_some() {}
            return;
        }
        // Every pane: pick up new ones as they appear.
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            while set.try_join_next().is_some() {}
            for pane in live(&host).await {
                if followed.insert(pane) {
                    let begun = crate::taps::begin(&host, pane, level).await;
                    set.spawn(crate::taps::follow(
                        begun,
                        host.clone(),
                        task_conn.clone(),
                        id,
                        pane,
                        level,
                        interval,
                    ));
                }
            }
        }
    });
    conn.subs.lock().await.insert(id, task);
    reply(ok(
        req.id,
        api::TapSubscription { id, panes }.encode_to_vec(),
    ))
    .await;
    let _ = go.send(());
}

/// `events.subscribe`: reply with where the subscription starts, then
/// push `events.batch` until it ends. The task reads the log at the pace
/// the connection takes batches; the log never waits for it.
async fn events_subscribe(state: &Arc<State>, conn: &Conn, req: wire::Request) {
    use crate::events::{DEFAULT_QUOTA, Filter, start, step};
    let reply = |r: wire::Response| {
        let conn = conn.clone();
        async move {
            let _ = conn
                .out
                .send(Out::Frame(Kind::Response, r.encode_to_vec()))
                .await;
        }
    };
    let p = match api::EventSubscribe::decode(req.payload.as_slice()) {
        Ok(p) => p,
        Err(e) => {
            return reply(failure(
                req.id,
                RpcError::new(code::INVALID_PARAMS, e.to_string()),
            ))
            .await;
        }
    };
    let events = state.events.clone();
    // Subscribe to wake-ups before reading the start, so nothing recorded
    // in between is missed.
    let mut wake = events.watch();
    let (cursor, opening_gap) = start(&events, &state.instance, p.after, p.instance.as_deref());
    let id = conn.next_sub.fetch_add(1, Ordering::Relaxed);
    let (oldest, _) = events.bounds();
    let filter = Filter {
        kinds: p.kinds,
        panes: p.panes,
    };
    let quota = p.quota.unwrap_or(DEFAULT_QUOTA).max(1);
    let task_conn = conn.clone();
    // The reply goes out first, so no batch precedes it.
    let (go, ready) = tokio::sync::oneshot::channel::<()>();
    let mut subs = conn.subs.lock().await;
    let task = tokio::spawn(async move {
        let conn = task_conn;
        if ready.await.is_err() {
            return;
        }
        let mut cursor = cursor;
        let mut pending_gap = opening_gap;
        loop {
            let grant = conn.grant.lock().map(|g| g.0.clone()).unwrap_or_default();
            let (next, gap, batch) = step(&events, cursor, quota, &filter, &grant);
            let gap = match (pending_gap.take(), gap) {
                (Some(a), Some(b)) => Some(api::EventGap {
                    first: a.first.min(b.first),
                    last: a.last.max(b.last),
                    reason: a.reason,
                }),
                (a, b) => a.or(b),
            };
            let advanced = next != cursor;
            cursor = next;
            if gap.is_some() || !batch.is_empty() {
                let b = api::EventBatch {
                    subscription: id,
                    gap,
                    events: batch,
                    cursor,
                };
                if !conn.event(TOPIC_EVENTS, b.encode_to_vec()).await {
                    return;
                }
            }
            if advanced {
                // More may already be waiting.
                continue;
            }
            if wake.changed().await.is_err() {
                return;
            }
        }
    });
    subs.insert(id, task);
    drop(subs);
    let r = api::EventSubscription {
        id,
        instance: state.instance.clone(),
        cursor,
        oldest,
    };
    reply(ok(req.id, r.encode_to_vec())).await;
    let _ = go.send(());
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
                client: p.client,
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
        title: started.title.clone(),
        cwd: started.cwd.clone(),
    };
    let (pane_id, epoch, generation) = (p.pane, started.epoch.0, started.generation.0);
    let batch = move |records: Vec<api::TerminalRecord>, effects: Vec<api::TerminalEffect>| {
        api::RecordBatch {
            subscription: sub,
            pane: pane_id,
            epoch,
            generation,
            records,
            effects,
        }
    };
    // Register the stream before replying, so an immediate unsubscribe
    // finds it; it waits for `go` so live records follow the reply and the
    // starting data, never precede them.
    let (go, wait) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(stream(
        conn.clone(),
        wait,
        rx,
        ended,
        host,
        sub,
        p.pane,
        started.generation,
        move |r, e| batch(r, e).encode_to_vec(),
    ));
    conn.subs.lock().await.insert(sub, task);
    reply(ok(id, s.encode_to_vec())).await;
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
                // Replayed records carry no effects: they already happened.
                let b = batch(
                    chunk.iter().map(|(s, r)| to_api(*s, r)).collect(),
                    Vec::new(),
                );
                conn.event(TOPIC_RECORDS, b.encode_to_vec()).await;
            }
        }
    }
    let _ = go.send(());
}

/// Forward a subscriber's records as batches until its stream ends, then
/// say why. A client that does not read makes this task wait, the
/// subscriber queue fill, and the owner end the subscription as lagged.
#[allow(clippy::too_many_arguments)]
async fn stream(
    conn: Conn,
    go: tokio::sync::oneshot::Receiver<()>,
    mut rx: mpsc::Receiver<mirror::Live>,
    ended: Arc<AtomicU8>,
    host: mirror::MirrorHost,
    sub: u64,
    pane: u32,
    generation: domain::PaneGeneration,
    encode: impl Fn(Vec<api::TerminalRecord>, Vec<api::TerminalEffect>) -> Vec<u8> + Send,
) {
    let _ = go.await;
    let effect = |seq: u64, e: mirror::Effect| api::TerminalEffect {
        id: e.id,
        sequence: seq,
        bells: e.bells,
        title: e.title,
        cwd: e.cwd,
        clipboard: e.clipboard,
    };
    while let Some((seq, r, fx)) = rx.recv().await {
        let mut records = vec![to_api(seq, &r)];
        // Command steps (shell integration) are for collectors; an effect
        // with nothing else is not sent to views.
        let shown = |e: &mirror::Effect| {
            e.bells > 0 || e.title.is_some() || e.cwd.is_some() || e.clipboard.is_some()
        };
        let mut effects: Vec<api::TerminalEffect> = fx
            .filter(shown)
            .map(|e| effect(seq, e))
            .into_iter()
            .collect();
        let mut bytes = records[0].data.len();
        while records.len() < BATCH_RECORDS && bytes < BATCH_BYTES {
            match rx.try_recv() {
                Ok((seq, r, fx)) => {
                    let r = to_api(seq, &r);
                    bytes += r.data.len();
                    records.push(r);
                    effects.extend(fx.filter(shown).map(|e| effect(seq, e)));
                }
                Err(_) => break,
            }
        }
        if !conn.event(TOPIC_RECORDS, encode(records, effects)).await {
            return;
        }
    }
    let reason = match ended.load(Ordering::SeqCst) {
        mirror::end::LAGGED => api::EndReason::Lagged,
        mirror::end::CONNECTION_LOST => api::EndReason::ConnectionLost,
        mirror::end::REATTACHED => api::EndReason::Reattached,
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
