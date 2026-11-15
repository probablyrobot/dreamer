//! The daemon's implementation of the API (one `Service`, reached through
//! the wire protocol now and JSON-RPC later).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use api::{RpcError, code};
use domain::ServerKey;
use tmux_backend::{Connection, ConnectionConfig, InventoryTracker, LinkState, Socket};
use tokio::sync::{Mutex, Notify};
use workspace_host::{Actions, Endpoint};

use crate::log;

/// The tmux server this daemon drives.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub name: String,
    pub program: PathBuf,
    pub socket: Socket,
}

impl EndpointConfig {
    fn describe(&self) -> String {
        match &self.socket {
            Socket::Default => "default".into(),
            Socket::Name(n) => format!("-L {n}"),
            Socket::Path(p) => format!("-S {}", p.display()),
        }
    }

    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.program);
        match &self.socket {
            Socket::Default => {}
            Socket::Name(n) => {
                c.args(["-L", n]);
            }
            Socket::Path(p) => {
                c.arg("-S").arg(p);
            }
        }
        c.stdin(std::process::Stdio::null());
        c
    }
}

#[derive(Debug)]
pub struct State {
    pub identity: wire::Identity,
    pub instance: String,
    pub epoch: u64,
    pub started: Instant,
    pub started_unix_ms: u64,
    pub socket: PathBuf,
    pub log: PathBuf,
    pub endpoint_config: EndpointConfig,
    pub endpoint: Mutex<Option<Endpoint>>,
    pub actions: Mutex<Actions>,
    pub clients: AtomicU32,
    pub shutting_down: AtomicBool,
    pub shutdown: Notify,
    /// Canonical mirrors of the current connection, if connected.
    pub mirrors: tokio::sync::RwLock<Option<mirror::MirrorHost>>,
    /// One input delivery at a time per pane, so a large paste is never
    /// interleaved with other input to that pane, and never holds up input
    /// to another pane.
    pub input: Mutex<std::collections::HashMap<u32, Arc<Mutex<()>>>>,
    /// The latest topology and its revision, for `tmux.watch` connections.
    /// A watch channel: a slow watcher only ever sees the newest.
    pub topology: tokio::sync::watch::Sender<Option<Arc<api::TopologyUpdate>>>,
    /// Who owns each tmux window's size, and every client's latest request
    /// (most recent last), so ownership can pass on disconnect.
    pub sizing: Mutex<std::collections::HashMap<u32, Sizing>>,
    /// Pastes arriving in parts, by (pane, paste id), until the last part.
    pub pastes: Mutex<std::collections::HashMap<(u32, u64), Vec<u8>>>,
    /// Connection epochs: each new control connection gets the next one.
    pub epochs: std::sync::atomic::AtomicU64,
    /// Outbound connections to other hosts' daemons (the coordinator role).
    pub hosts: crate::hosts::Hosts,
    /// Pane app launches waiting for registration.
    pub apps: crate::apps::Apps,
    /// The event log `events.subscribe` follows.
    pub events: Arc<crate::events::Events>,
    /// Structured messages between connections.
    pub broker: Arc<crate::messages::Broker>,
    /// Supervised extensions.
    pub extensions: crate::extensions::Extensions,
    /// Loaded scripts.
    pub scripts: crate::scripts::Scripts,
    /// Installed filters.
    pub filters: crate::filters::Filters,
    /// Marks filters put on lines, for clients to draw.
    pub decorations: crate::decorations::Decorations,
    /// What provider hooks report about their sessions.
    pub agents: crate::agents::Agents,
}

#[derive(Debug, Clone)]
pub struct DaemonService(pub Arc<State>);

/// Methods that act as the calling connection are answered there.
fn for_a_connection(method: &str) -> RpcError {
    RpcError::new(
        code::INVALID_REQUEST,
        format!("{method} needs a streaming connection to the daemon"),
    )
}

fn unavailable(msg: String) -> RpcError {
    RpcError::new(code::UNAVAILABLE, msg)
}

impl State {
    /// The endpoint, connecting on first use and after its link dropped.
    pub(crate) async fn endpoint(
        self: &Arc<Self>,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Endpoint>>, RpcError> {
        let mut guard = self.endpoint.lock().await;
        let live = guard
            .as_ref()
            .is_some_and(|e| !matches!(e.conn.state(), LinkState::Failed));
        if !live {
            *guard = None;
            let cfg = &self.endpoint_config;
            // Test harnesses set this so no test can reach the user's own
            // tmux server, even by forgetting to name an isolated socket.
            if matches!(cfg.socket, Socket::Default)
                && std::env::var_os("DREAMER_FORBID_DEFAULT_TMUX").is_some()
            {
                return Err(unavailable(
                    "the default tmux server is off limits here (DREAMER_FORBID_DEFAULT_TMUX)"
                        .into(),
                ));
            }
            let out = cfg
                .command()
                .args(["list-sessions", "-F", "#{session_id}"])
                .output()
                .await
                .map_err(|e| unavailable(format!("cannot run {}: {e}", cfg.program.display())))?;
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(str::to_owned)
                .filter(|_| out.status.success());
            let Some(session) = first else {
                return Err(unavailable(format!(
                    "no tmux server with sessions on endpoint `{}` (tmux {})",
                    cfg.name,
                    cfg.describe()
                )));
            };
            let mut config =
                ConnectionConfig::new(cfg.program.clone(), cfg.socket.clone(), session);
            config.config_file = None;
            let (conn, events) = Connection::spawn(config).await.map_err(|e| {
                unavailable(format!("cannot attach to tmux {}: {e}", cfg.describe()))
            })?;
            // The mirrors take the event stream first: responses to the
            // commands below only arrive while events keep flowing.
            let epoch = domain::ConnectionEpoch(self.epochs.fetch_add(1, Ordering::SeqCst) + 1);
            let (host, changes) =
                mirror::MirrorHost::start(conn.clone(), events, epoch, mirror_config())
                    .await
                    .map_err(|e| unavailable(format!("cannot start mirror workers: {e}")))?;
            let (tracker, _) = InventoryTracker::start(
                conn.clone(),
                ServerKey(cfg.name.clone()),
                cfg.name.clone(),
            )
            .await
            .map_err(|e| unavailable(format!("cannot read tmux {}: {e}", cfg.describe())))?;
            log::info(format!(
                "attached to tmux endpoint `{}` ({})",
                cfg.name,
                cfg.describe()
            ));
            host.sync(tracker.inventory().server.clone(), mirrored_panes(&tracker))
                .await;
            *self.mirrors.write().await = Some(host.clone());
            tokio::spawn(reconcile(Arc::downgrade(self), host, changes));
            *guard = Some(Endpoint {
                name: cfg.name.clone(),
                conn,
                tracker,
            });
        }
        Ok(guard)
    }
}

/// Mirror settings: the defaults, with journal limits from
/// `DREAMER_JOURNAL_PANE_BYTES` and `DREAMER_JOURNAL_HOST_BYTES` if set.
fn mirror_config() -> mirror::MirrorConfig {
    let mut c = mirror::MirrorConfig::default();
    // The terminal's colors and cursor are the daemon's to set: it owns the
    // canonical mirrors, and views get them through checkpoints.
    match workspace_config::load_settings() {
        Ok(settings) => {
            let t = settings.terminal;
            c.terminal = mirror::TerminalDefaults {
                scheme: t.theme,
                cursor: t.cursor_style.shape(),
                blink: t.cursor_blink,
            };
            log::info(format!("terminal theme {}", t.theme.name));
        }
        Err(e) => log::warn(format!("{e}; terminal defaults used")),
    }
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    };
    if let Some(b) = env("DREAMER_JOURNAL_PANE_BYTES") {
        c.retention.per_pane_bytes = b;
    }
    if let Some(b) = env("DREAMER_JOURNAL_HOST_BYTES") {
        c.retention.host_bytes = b;
    }
    // Tests put panes on one worker to measure fairness on a shared owner.
    if let Some(n) = env("DREAMER_MIRROR_WORKERS") {
        c.workers = n.max(1);
    }
    c
}

/// Every pane execution the inventory knows, for the mirrors.
fn mirrored_panes(tracker: &InventoryTracker) -> Vec<(domain::TmuxPaneId, domain::PaneGeneration)> {
    tracker
        .inventory()
        .panes
        .keys()
        .filter_map(|p| tracker.generation(*p).map(|g| (*p, g)))
        .collect()
}

/// Keep the mirrors in step with tmux: whenever tmux reports a structural
/// change (windows, layouts, pane processes), re-read the inventory and
/// attach, re-attach, or drop mirrors. Ends with its connection.
async fn reconcile(
    state: std::sync::Weak<State>,
    host: mirror::MirrorHost,
    mut changes: tokio::sync::watch::Receiver<u64>,
) {
    while changes.changed().await.is_ok() {
        // Let a burst of notifications settle into one re-read.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        changes.borrow_and_update();
        let Some(state) = state.upgrade() else { return };
        let mut guard = state.endpoint.lock().await;
        let Some(ep) = guard.as_mut() else { return };
        if ep.conn.state() == LinkState::Failed {
            return;
        }
        if let Err(e) = ep.tracker.refresh().await {
            log::warn(format!("re-reading tmux for mirrors: {e}"));
            continue;
        }
        publish_topology(&state, ep);
        let tmux = ep.tracker.inventory().server.clone();
        let panes = mirrored_panes(&ep.tracker);
        drop(guard);
        host.sync(tmux, panes).await;
    }
}

/// A tmux window's sizing: its owner and the latest request per client.
#[derive(Debug, Default)]
pub struct Sizing {
    pub owner: Option<u64>,
    /// (client, cols, rows), most recent last.
    pub requests: Vec<(u64, u16, u16)>,
}

/// Clients whose connection closed give up the windows they own; the most
/// recent other requester takes over and its size is applied once. With no
/// other requester, the window's size is cleared, so tmux sizes it from its
/// other clients (the user's terminal) again.
pub async fn release_sizing(state: &State, clients: &[u64]) {
    if clients.is_empty() {
        return;
    }
    let mut apply = Vec::new();
    let mut clear = Vec::new();
    let mut stop_sizing = false;
    {
        let mut sizing = state.sizing.lock().await;
        for (window, s) in sizing.iter_mut() {
            s.requests.retain(|(c, _, _)| !clients.contains(c));
            if s.owner.is_some_and(|o| clients.contains(&o)) {
                s.owner = s.requests.last().map(|(c, _, _)| *c);
                match s.requests.last() {
                    Some((_, cols, rows)) => apply.push((*window, *cols, *rows)),
                    None => clear.push(*window),
                }
            }
        }
        sizing.retain(|_, s| s.owner.is_some());
        // No Dreamer window sizes anything any more: let tmux size every
        // window from the other clients (the user's terminal) again.
        if sizing.is_empty() {
            stop_sizing = true;
        }
    }
    let guard = state.endpoint.lock().await;
    let Some(ep) = guard.as_ref() else { return };
    for (window, cols, rows) in apply {
        let _ = tmux_backend::ops::size_window(&ep.conn, domain::TmuxWindowId(window), cols, rows)
            .await;
    }
    for window in clear {
        let _ = tmux_backend::ops::clear_window_size(&ep.conn, domain::TmuxWindowId(window)).await;
    }
    if stop_sizing {
        let _ = tmux_backend::ops::set_ignore_size(&ep.conn, true).await;
    }
}

/// Publish the endpoint's topology to watchers if it changed.
pub fn publish_topology(state: &State, ep: &Endpoint) {
    let topology = workspace_host::topology(&ep.name, &ep.tracker);
    state.topology.send_if_modified(|current| {
        if current
            .as_ref()
            .is_some_and(|c| c.topology.as_ref() == Some(&topology))
        {
            return false;
        }
        let revision = current.as_ref().map_or(1, |c| c.revision + 1);
        *current = Some(Arc::new(api::TopologyUpdate {
            revision,
            topology: Some(topology),
        }));
        state.events.topology(revision);
        true
    });
}

/// The runs in a process list, each with how many processes serve it.
fn runs_of(
    list: &api::ProcessList,
    host: Option<&str>,
    provider: Option<&str>,
) -> Vec<api::AgentRun> {
    list.processes
        .iter()
        .filter(|p| p.provider.is_some())
        .filter(|p| provider.is_none_or(|w| p.provider.as_deref() == Some(w)))
        .map(|p| api::AgentRun {
            host: host.map(str::to_owned),
            pid: p.pid,
            started_unix_ms: p.started_unix_ms,
            provider: p.provider.clone().unwrap_or_default(),
            pane: p.pane,
            cwd: p.cwd.clone(),
            age_ms: list.taken_unix_ms.saturating_sub(p.started_unix_ms),
            cpu_permille: p.cpu_permille,
            rss_kib: p.rss_kib,
            confidence: p.confidence.clone().unwrap_or_default(),
            evidence: p.evidence.clone().unwrap_or_default(),
            instrumentation: "passive".into(),
            state: "running".into(),
            session_id: None,
            last_event_unix_ms: None,
            helpers: list
                .processes
                .iter()
                .filter(|h| h.helper_of == Some(p.pid))
                .count() as u32,
        })
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn version_info(identity: &wire::Identity) -> api::VersionInfo {
    api::VersionInfo {
        protocol_major: u32::from(wire::PROTOCOL_MAJOR),
        protocol_minor: u32::from(wire::PROTOCOL_MINOR),
        daemon_version: identity.software_version.clone(),
        build_family: identity.build_family.clone(),
        ghostty_commit: terminal_core::family::GHOSTTY_COMMIT.into(),
        platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    }
}

impl api::Service for DaemonService {
    async fn daemon_version(&self, _: api::Empty) -> Result<api::VersionInfo, RpcError> {
        Ok(version_info(&self.0.identity))
    }

    async fn daemon_status(&self, _: api::Empty) -> Result<api::DaemonStatus, RpcError> {
        let s = &self.0;
        let cfg = &s.endpoint_config;
        let (state, detail) = match s.endpoint.try_lock() {
            Ok(guard) => match guard.as_ref().map(|e| e.conn.state()) {
                Some(LinkState::Ready) => (api::LinkState::Live, None),
                Some(LinkState::Recovering) => (
                    api::LinkState::Live,
                    Some("draining late responses after a timeout".to_owned()),
                ),
                Some(LinkState::Failed) => (
                    api::LinkState::Failed,
                    Some("the control connection ended; reconnects on next use".to_owned()),
                ),
                None => (
                    api::LinkState::Unspecified,
                    Some("not connected yet; connects on first use".to_owned()),
                ),
            },
            // Busy running an action: it is connected.
            Err(_) => (api::LinkState::Live, None),
        };
        let tmux_version = cfg
            .command()
            .arg("-V")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        Ok(api::DaemonStatus {
            pid: std::process::id(),
            started_unix_ms: s.started_unix_ms,
            uptime_ms: s.started.elapsed().as_millis() as u64,
            socket_path: s.socket.display().to_string(),
            log_path: s.log.display().to_string(),
            endpoints: vec![api::EndpointStatus {
                name: cfg.name.clone(),
                socket: cfg.describe(),
                state: state as i32,
                tmux_version,
                detail,
            }],
            version: Some(version_info(&s.identity)),
            clients: s.clients.load(Ordering::Relaxed),
        })
    }

    async fn daemon_shutdown(&self, _: api::Empty) -> Result<api::ShutdownResult, RpcError> {
        self.0.shutting_down.store(true, Ordering::SeqCst);
        log::info("shutdown requested by a client");
        Ok(api::ShutdownResult {
            effect:
                "dreamerd is stopping. tmux sessions, windows, panes, and the programs in them \
                     keep running; no jobs or extensions run under this version of the daemon. \
                     Start it again with `dream ensure-running`."
                    .into(),
        })
    }

    async fn tmux_topology(&self, p: api::TopologyParams) -> Result<api::Topology, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        let mut guard = self.0.endpoint().await?;
        let ep = guard
            .as_mut()
            .ok_or_else(|| unavailable("endpoint not connected".into()))?;
        ep.tracker
            .refresh()
            .await
            .map_err(|e| unavailable(format!("cannot read tmux: {e}")))?;
        publish_topology(&self.0, ep);
        Ok(workspace_host::topology(&ep.name, &ep.tracker))
    }

    async fn tmux_size_window(&self, p: api::SizeWindow) -> Result<api::SizeResult, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        let mut sizing = self.0.sizing.lock().await;
        if p.release {
            let s = sizing.entry(p.window).or_default();
            s.requests.retain(|(c, _, _)| *c != p.client);
            let was_owner = s.owner == Some(p.client);
            drop(sizing);
            if was_owner {
                // Hand over as if this client had disconnected.
                let mut sizing = self.0.sizing.lock().await;
                if let Some(s) = sizing.get_mut(&p.window) {
                    s.owner = None;
                }
                drop(sizing);
                release_sizing(&self.0, &[p.client]).await;
            }
            let owner = self
                .0
                .sizing
                .lock()
                .await
                .get(&p.window)
                .and_then(|s| s.owner);
            return Ok(api::SizeResult {
                owner: false,
                applied: false,
                owner_client: owner,
            });
        }
        let (cols, rows) = match (u16::try_from(p.cols), u16::try_from(p.rows)) {
            (Ok(c), Ok(r)) if (2..=10_000).contains(&c) && (2..=10_000).contains(&r) => (c, r),
            _ => {
                return Err(RpcError::new(
                    code::INVALID_PARAMS,
                    format!("{}x{} is not a usable window size", p.cols, p.rows),
                ));
            }
        };
        let s = sizing.entry(p.window).or_default();
        s.requests.retain(|(c, _, _)| *c != p.client);
        s.requests.push((p.client, cols, rows));
        let owner = *s.owner.get_or_insert(p.client);
        drop(sizing);
        if owner != p.client {
            return Ok(api::SizeResult {
                owner: false,
                applied: false,
                owner_client: Some(owner),
            });
        }
        let guard = self.0.endpoint().await?;
        let ep = guard
            .as_ref()
            .ok_or_else(|| unavailable("endpoint not connected".into()))?;
        // A size of ours counts again (it may have been set aside when no
        // Dreamer window was sizing anything).
        let _ = tmux_backend::ops::set_ignore_size(&ep.conn, false).await;
        tmux_backend::ops::size_window(&ep.conn, domain::TmuxWindowId(p.window), cols, rows)
            .await
            .map_err(|e| {
                RpcError::new(code::INVALID_PARAMS, tmux_backend::ops::failure_message(&e))
            })?;
        Ok(api::SizeResult {
            owner: true,
            applied: true,
            owner_client: Some(owner),
        })
    }

    async fn tmux_watch(&self, p: api::TopologyParams) -> Result<api::TopologyUpdate, RpcError> {
        // Read tmux now, so the reply is current; the connection then
        // follows `State::topology` (see conn.rs).
        self.tmux_topology(p).await?;
        let current = self.0.topology.borrow().clone();
        current
            .map(|u| (*u).clone())
            .ok_or_else(|| unavailable("no topology yet".into()))
    }

    async fn actions_submit(&self, p: api::SubmitAction) -> Result<api::ActionRecord, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        let mut guard = self.0.endpoint().await?;
        let ep = guard
            .as_mut()
            .ok_or_else(|| unavailable("endpoint not connected".into()))?;
        let mut actions = self.0.actions.lock().await;
        let pane = crate::grants::single_pane(&api::Request::ActionsSubmit(p.clone()));
        let r = actions.submit(ep, p).await?;
        self.0.events.action(&r, pane);
        log::info(format!(
            "action {} {}{}",
            r.id,
            workspace_host::ledger::state_name(r.state),
            r.reason
                .as_deref()
                .map(|why| format!(": {why}"))
                .unwrap_or_default()
        ));
        Ok(r)
    }

    async fn terminal_read(&self, p: api::ReadParams) -> Result<api::TerminalText, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        drop(self.0.endpoint().await?);
        let host = self
            .0
            .mirrors
            .read()
            .await
            .clone()
            .ok_or_else(|| unavailable("no mirrors yet".into()))?;
        let pane = domain::TmuxPaneId(p.pane);
        let history = p.history.unwrap_or(0).min(10_000) as usize;
        let mut read = host.read(pane, history).await;
        if read.is_none() {
            // A pane that exists in tmux but is still attaching (or so new
            // the mirrors have not seen it yet, such as an app's own pane
            // right after launch) is worth a short wait. A pane tmux does
            // not know fails at once.
            let exists = {
                let guard = self.0.endpoint().await?;
                match guard.as_ref() {
                    Some(ep) => tmux_backend::ops::pane_pid(&ep.conn, pane).await.is_ok(),
                    None => false,
                }
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while exists && std::time::Instant::now() < deadline {
                if host.status().await.iter().any(|s| s.pane == pane) {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if host.wait_live(&[pane], left).await {
                        read = host.read(pane, history).await;
                    }
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let r = read
            .ok_or_else(|| {
                RpcError::new(code::NOT_FOUND, format!("pane %{} is not mirrored", p.pane))
            })?
            .map_err(|e| RpcError::new(code::INTERNAL, e))?;
        Ok(api::TerminalText {
            pane: r.pane.pane.0,
            generation: r.generation.0,
            epoch: r.epoch.0,
            sequence: r.sequence,
            cols: u32::from(r.cols),
            rows: u32::from(r.rows),
            cursor_x: u32::from(r.cursor.0),
            cursor_y: u32::from(r.cursor.1),
            history_lines: r.history as u32,
            lines: r.lines,
        })
    }

    async fn terminal_mirrors(&self, p: api::TopologyParams) -> Result<api::MirrorList, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        drop(self.0.endpoint().await?);
        let host = self.0.mirrors.read().await.clone();
        let Some(host) = host else {
            return Ok(api::MirrorList::default());
        };
        let mut mirrors: Vec<api::MirrorStatus> = host
            .status()
            .await
            .into_iter()
            .map(|m| {
                let (state, detail) = match &m.state {
                    mirror::MirrorState::Attaching => (api::MirrorState::Attaching, None),
                    mirror::MirrorState::Live => (api::MirrorState::Live, None),
                    mirror::MirrorState::Failed(e) => (api::MirrorState::Failed, Some(e.clone())),
                };
                api::MirrorStatus {
                    pane: m.pane.0,
                    generation: m.generation.0,
                    epoch: host.epoch().0,
                    state: state as i32,
                    sequence: m.live.as_ref().map_or(0, |l| l.sequence),
                    ingested_bytes: m.live.as_ref().map_or(0, |l| l.ingested),
                    worker: m.worker as u32,
                    cols: m.live.as_ref().map_or(0, |l| u32::from(l.cols)),
                    rows: m.live.as_ref().map_or(0, |l| u32::from(l.rows)),
                    detail,
                    tier: match m.live.as_ref().map(|l| l.tier) {
                        Some(mirror::Tier::Hot) => api::Tier::Hot,
                        Some(mirror::Tier::Warm) => api::Tier::Warm,
                        None => api::Tier::Unspecified,
                    } as i32,
                    journal_bytes: m.live.as_ref().map_or(0, |l| l.journal_bytes as u64),
                    journal_first_sequence: m.live.as_ref().map_or(0, |l| l.journal_first),
                    journal_records: m.live.as_ref().map_or(0, |l| l.journal_records as u32),
                    title: m.live.as_ref().and_then(|l| l.title.clone()),
                    cwd: m.live.as_ref().and_then(|l| l.cwd.clone()),
                    reattaches: m.live.as_ref().map_or(0, |l| l.reattaches),
                }
            })
            .collect();
        mirrors.sort_by_key(|m| m.pane);
        let r = host.retention();
        Ok(api::MirrorList {
            mirrors,
            workers: mirror_config().workers as u32,
            host_journal_bytes: host.host_journal_bytes() as u64,
            journal_pane_limit: r.per_pane_bytes as u64,
            journal_host_limit: r.host_bytes as u64,
            journal_max_age_ms: r.max_age.as_millis() as u64,
        })
    }

    async fn terminal_set_tier(&self, p: api::SetTier) -> Result<api::Empty, RpcError> {
        self.check_endpoint(p.endpoint.as_deref())?;
        let tier = match api::Tier::try_from(p.tier) {
            Ok(api::Tier::Hot) => mirror::Tier::Hot,
            Ok(api::Tier::Warm) => mirror::Tier::Warm,
            _ => {
                return Err(RpcError::new(
                    code::INVALID_PARAMS,
                    "only hot and warm can be set; cold (metadata only) is not available yet",
                ));
            }
        };
        let host = self.0.mirrors.read().await.clone();
        match host {
            Some(h) if h.set_tier(domain::TmuxPaneId(p.pane), tier).await => Ok(api::Empty {}),
            _ => Err(RpcError::new(
                code::NOT_FOUND,
                format!("pane %{} has no live mirror", p.pane),
            )),
        }
    }

    // Subscriptions push data on their connection, so the connection layer
    // (conn.rs) serves them; this path is only reached without one.
    async fn terminal_subscribe(
        &self,
        _: api::TerminalSubscribe,
    ) -> Result<api::Subscription, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "terminal.subscribe needs a streaming connection to the daemon",
        ))
    }

    async fn terminal_unsubscribe(&self, _: api::Unsubscribe) -> Result<api::Empty, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "terminal.unsubscribe needs a streaming connection to the daemon",
        ))
    }

    async fn terminal_input(&self, p: api::TerminalInput) -> Result<api::InputResult, RpcError> {
        use api::terminal_input::Event;
        let host = self.mirror_host(p.endpoint.as_deref()).await?;
        let pane = domain::TmuxPaneId(p.pane);
        let event = match p.event {
            Some(Event::Key(k)) => mirror::InputEvent::Key {
                key: k.key,
                text: k.text,
                mods: u16::try_from(k.mods)
                    .map_err(|_| RpcError::new(code::INVALID_PARAMS, "mods out of range"))?,
                held: k.held,
            },
            Some(Event::Text(t)) => mirror::InputEvent::Text(t.text),
            Some(Event::Paste(v)) => {
                // Gather parts; encode the whole paste once, on the last.
                const MAX_PASTE: usize = 64 << 20;
                let mut pastes = self.0.pastes.lock().await;
                let buf = pastes.entry((p.pane, v.id)).or_default();
                if buf.len() + v.data.len() > MAX_PASTE {
                    pastes.remove(&(p.pane, v.id));
                    return Err(RpcError::new(
                        code::INVALID_PARAMS,
                        "a paste is limited to 64 MiB",
                    ));
                }
                buf.extend_from_slice(&v.data);
                if v.more {
                    return Ok(api::InputResult {
                        bytes: 0,
                        batches: 0,
                    });
                }
                mirror::InputEvent::Paste(pastes.remove(&(p.pane, v.id)).unwrap_or_default())
            }
            Some(Event::Focus(f)) => mirror::InputEvent::Focus(f.gained),
            Some(Event::Mouse(m)) => {
                use api::{MouseAction as A, MouseButton as B};
                let action = match A::try_from(m.action) {
                    Ok(A::Press) => mirror::MouseAction::Press,
                    Ok(A::Release) => mirror::MouseAction::Release,
                    Ok(A::Motion) => mirror::MouseAction::Motion,
                    _ => {
                        return Err(RpcError::new(
                            code::INVALID_PARAMS,
                            "mouse input needs an action",
                        ));
                    }
                };
                let button = match B::try_from(m.button) {
                    Ok(B::None) => None,
                    Ok(B::Left) => Some(mirror::MouseButton::Left),
                    Ok(B::Middle) => Some(mirror::MouseButton::Middle),
                    Ok(B::Right) => Some(mirror::MouseButton::Right),
                    Ok(B::WheelUp) => Some(mirror::MouseButton::WheelUp),
                    Ok(B::WheelDown) => Some(mirror::MouseButton::WheelDown),
                    Err(_) => {
                        return Err(RpcError::new(code::INVALID_PARAMS, "unknown mouse button"));
                    }
                };
                let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                mirror::InputEvent::Mouse {
                    action,
                    button,
                    col: clamp(m.col),
                    row: clamp(m.row),
                    mods: u16::try_from(m.mods)
                        .map_err(|_| RpcError::new(code::INVALID_PARAMS, "mods out of range"))?,
                    buttons_held: m.buttons_held,
                }
            }
            None => return Err(RpcError::new(code::INVALID_PARAMS, "input needs an event")),
        };
        if !host
            .wait_live(&[pane], std::time::Duration::from_secs(3))
            .await
        {
            return Err(RpcError::new(
                code::NOT_FOUND,
                format!("pane %{} has no live mirror", p.pane),
            ));
        }
        let lane = Arc::clone(self.0.input.lock().await.entry(p.pane).or_default());
        let _serial = lane.lock().await;
        let bytes = host
            .encode(
                pane,
                p.generation.map(domain::PaneGeneration),
                event,
                p.client,
            )
            .await
            .map_err(|e| {
                let c = if e.contains("was replaced") {
                    code::PRECONDITION
                } else if e.contains("no live mirror") {
                    code::NOT_FOUND
                } else {
                    code::INVALID_PARAMS
                };
                RpcError::new(c, e)
            })?;
        let conn = self
            .0
            .endpoint
            .lock()
            .await
            .as_ref()
            .map(|e| e.conn.clone())
            .ok_or_else(|| unavailable("endpoint not connected".into()))?;
        let batches = if bytes.is_empty() {
            0
        } else {
            tmux_backend::ops::send_bytes(&conn, pane, &bytes)
                .await
                .map_err(|e| {
                    RpcError::new(code::UNAVAILABLE, tmux_backend::ops::failure_message(&e))
                })?
        };
        Ok(api::InputResult {
            bytes: bytes.len() as u64,
            batches,
        })
    }

    async fn hosts_connect(&self, p: api::HostConnect) -> Result<api::HostStatus, RpcError> {
        self.0.hosts.connect(p).await
    }

    async fn hosts_list(&self, _: api::Empty) -> Result<api::HostList, RpcError> {
        Ok(self.0.hosts.list().await)
    }

    async fn hosts_disconnect(&self, p: api::HostRef) -> Result<api::HostStatus, RpcError> {
        self.0.hosts.disconnect(&p.name).await
    }

    async fn hosts_reconnect(&self, p: api::HostRef) -> Result<api::HostStatus, RpcError> {
        self.0.hosts.reconnect(&p.name).await
    }

    async fn hosts_subscribe(&self, _: api::HostSubscribe) -> Result<api::Subscription, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "hosts.subscribe needs a streaming connection to the daemon",
        ))
    }

    async fn hosts_watch(&self, p: api::HostRef) -> Result<api::TopologyUpdate, RpcError> {
        Ok(self.0.hosts.watch(&p.name).await?.0)
    }

    async fn hosts_install_plan(
        &self,
        p: api::InstallPlanParams,
    ) -> Result<api::InstallPlan, RpcError> {
        let remote = self.0.hosts.remote_of(&p.name).await?;
        crate::install::plan(&p.name, &remote, p.artifacts.as_deref()).await
    }

    async fn hosts_install(&self, p: api::InstallParams) -> Result<api::InstallResult, RpcError> {
        let remote = self.0.hosts.remote_of(&p.name).await?;
        crate::log::info(format!("hosts: installing a server on `{}`", p.name));
        crate::install::install(&p.name, &remote, p.artifacts.as_deref(), &p.plan_id).await
    }

    async fn hosts_upgrade(&self, p: api::UpgradeParams) -> Result<api::UpgradeResult, RpcError> {
        self.0.hosts.upgrade(p).await
    }

    async fn apps_launch(&self, p: api::LaunchApp) -> Result<api::LaunchedApp, RpcError> {
        let (id, env, expires) = self.0.apps.prepare(&p).await?;
        let launched = async {
            let guard = self.0.endpoint().await?;
            let ep = guard
                .as_ref()
                .ok_or_else(|| unavailable("endpoint not connected".into()))?;
            let launch = tmux_backend::ops::Launch {
                argv: p.argv.clone(),
                cwd: p.cwd.clone(),
                env,
                background: false,
            };
            let name = p.name.clone().unwrap_or_else(|| p.app_id.clone());
            tmux_backend::ops::new_window(
                &ep.conn,
                domain::TmuxSessionId(p.session),
                Some(&name),
                &launch,
            )
            .await
            .map_err(|e| {
                RpcError::new(code::INVALID_PARAMS, tmux_backend::ops::failure_message(&e))
            })
        }
        .await;
        match launched {
            Ok(pane) => {
                self.0.apps.launched(&id, pane.0).await;
                crate::log::info(format!("apps: launched `{}` in pane %{}", p.app_id, pane.0));
                Ok(api::LaunchedApp {
                    launch_id: id,
                    pane: pane.0,
                    expires_unix_ms: expires,
                })
            }
            Err(e) => {
                self.0.apps.abandon(&id).await;
                Err(e)
            }
        }
    }

    async fn events_subscribe(
        &self,
        _: api::EventSubscribe,
    ) -> Result<api::EventSubscription, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "events.subscribe needs a streaming connection to the daemon",
        ))
    }

    async fn events_unsubscribe(&self, _: api::Unsubscribe) -> Result<api::Empty, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "events.unsubscribe needs a streaming connection to the daemon",
        ))
    }

    async fn messages_publish(&self, _: api::MessagePublish) -> Result<api::MessageSent, RpcError> {
        Err(for_a_connection("messages.publish"))
    }

    async fn messages_request(
        &self,
        _: api::MessageRequest,
    ) -> Result<api::MessageEnvelope, RpcError> {
        Err(for_a_connection("messages.request"))
    }

    async fn messages_reply(&self, _: api::MessageReply) -> Result<api::MessageSent, RpcError> {
        Err(for_a_connection("messages.reply"))
    }

    async fn messages_subscribe(
        &self,
        _: api::MessageSubscribe,
    ) -> Result<api::MessageSubscription, RpcError> {
        Err(for_a_connection("messages.subscribe"))
    }

    async fn messages_unsubscribe(&self, _: api::Unsubscribe) -> Result<api::Empty, RpcError> {
        Err(for_a_connection("messages.unsubscribe"))
    }

    async fn processes_list(&self, p: api::ProcessQuery) -> Result<api::ProcessList, RpcError> {
        // Pane programs, when tmux is connected; processes are listed
        // either way.
        let panes: Vec<procreg::Pane> = match self.0.endpoint().await {
            Ok(guard) => guard
                .as_ref()
                .map(|ep| {
                    workspace_host::topology(&ep.name, &ep.tracker)
                        .panes
                        .iter()
                        .filter_map(|pn| pn.pid.map(|pid| procreg::Pane { id: pn.id, pid }))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let taken = now_ms();
        let procs = tokio::task::spawn_blocking(procreg::snapshot)
            .await
            .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
            .map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
        let want_provider = p.provider.clone();
        let entries: Vec<procreg::Entry> = procreg::registry(&procs, &panes)
            .into_iter()
            .filter(|e| p.panes.is_empty() || e.pane.is_some_and(|(id, _)| p.panes.contains(&id)))
            .filter(|e| !p.runs_only || e.run.is_some())
            .filter(|e| {
                want_provider
                    .as_deref()
                    .is_none_or(|w| e.run.is_some_and(|r| r.0.name() == w))
            })
            .collect();
        // Working directories, for runs only (each costs a lookup).
        let run_pids: Vec<u32> = entries
            .iter()
            .filter(|e| e.run.is_some())
            .map(|e| e.proc.pid)
            .take(32)
            .collect();
        let cwds: std::collections::HashMap<u32, String> = tokio::task::spawn_blocking(move || {
            run_pids
                .into_iter()
                .filter_map(|pid| procreg::cwd(pid).map(|c| (pid, c)))
                .collect()
        })
        .await
        .unwrap_or_default();
        let cut = |s: &str| {
            let mut n = s.len().min(1024);
            while !s.is_char_boundary(n) {
                n -= 1;
            }
            s[..n].to_owned()
        };
        let processes = entries
            .into_iter()
            .map(|e| api::ProcessRecord {
                pid: e.proc.pid,
                started_unix_ms: e.proc.started_unix_s * 1000,
                ppid: e.proc.ppid,
                pgid: e.proc.pgid,
                tty: e.proc.tty.clone(),
                command: e.proc.command().to_owned(),
                args: cut(&e.proc.args),
                cwd: cwds.get(&e.proc.pid).cloned(),
                pane: e.pane.map(|(id, _)| id),
                pane_evidence: e.pane.map(|(_, ev)| {
                    match ev {
                        procreg::PaneEvidence::Ancestry => "ancestry",
                        procreg::PaneEvidence::Terminal => "terminal",
                    }
                    .to_owned()
                }),
                provider: e.run.map(|r| r.0.name().to_owned()),
                confidence: e.run.map(|r| {
                    match r.1 {
                        procreg::Confidence::High => "high",
                        procreg::Confidence::Medium => "medium",
                        procreg::Confidence::Low => "low",
                    }
                    .to_owned()
                }),
                evidence: e.run.map(|r| r.2.to_owned()),
                helper_of: e.helper_of,
                cpu_permille: e.proc.cpu_permille,
                rss_kib: e.proc.rss_kib,
            })
            .collect();
        Ok(api::ProcessList {
            processes,
            taken_unix_ms: taken,
        })
    }

    async fn agents_event(&self, e: api::AgentEvent) -> Result<api::Empty, RpcError> {
        if e.session_id.is_empty() || e.provider.is_empty() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "a report needs a provider and a session",
            ));
        }
        self.0.agents.report(e).await;
        Ok(api::Empty {})
    }

    async fn agents_runs(&self, p: api::AgentRunQuery) -> Result<api::AgentRunList, RpcError> {
        let mut out = api::AgentRunList::default();
        let here = self.processes_list(api::ProcessQuery::default()).await?;
        for mut run in runs_of(&here, None, p.provider.as_deref()) {
            // What its hooks say, when they report.
            if let Some(s) = self
                .0
                .agents
                .for_run(&run.provider, run.pid, run.started_unix_ms)
            {
                run.instrumentation = "hooks".into();
                run.state = s.state.into();
                run.session_id = Some(s.session_id);
                run.last_event_unix_ms = Some(s.last_unix_ms);
                if run.cwd.is_none() {
                    run.cwd = s.cwd;
                }
            }
            out.runs.push(run);
        }
        let hosts: Vec<String> = if p.hosts.iter().any(|h| h == "*") {
            self.0
                .hosts
                .list()
                .await
                .hosts
                .into_iter()
                .filter(|h| h.state == api::LinkState::Live as i32)
                .map(|h| h.name)
                .collect()
        } else {
            p.hosts.clone()
        };
        for name in hosts {
            let call = self.0.hosts.call(api::HostCall {
                name: name.clone(),
                method: api::method::PROCESSES_LIST.into(),
                payload: prost::Message::encode_to_vec(&api::ProcessQuery::default()),
            });
            match tokio::time::timeout(std::time::Duration::from_secs(5), call).await {
                Ok(Ok(reply)) => {
                    match <api::ProcessList as prost::Message>::decode(reply.payload.as_slice()) {
                        Ok(list) => {
                            out.runs
                                .extend(runs_of(&list, Some(&name), p.provider.as_deref()))
                        }
                        Err(e) => out.notes.push(format!("host {name}: {e}")),
                    }
                }
                Ok(Err(e)) => out.notes.push(format!("host {name}: {}", e.message)),
                Err(_) => out.notes.push(format!("host {name}: no answer in time")),
            }
        }
        Ok(out)
    }

    async fn jobs_start(&self, p: api::JobStart) -> Result<api::JobRecord, RpcError> {
        crate::jobs::start(&self.0, p).await
    }

    async fn jobs_get(&self, p: api::JobRef) -> Result<api::JobRecord, RpcError> {
        crate::jobs::get(&p.id)
    }

    async fn jobs_list(&self, _: api::Empty) -> Result<api::JobList, RpcError> {
        Ok(crate::jobs::list())
    }

    async fn jobs_cancel(&self, p: api::JobRef) -> Result<api::JobRecord, RpcError> {
        crate::jobs::cancel(&p.id)
    }

    async fn jobs_artifacts(&self, p: api::JobRef) -> Result<api::JobArtifacts, RpcError> {
        crate::jobs::artifacts(&p.id)
    }

    async fn decorations_list(
        &self,
        p: api::DecorationQuery,
    ) -> Result<api::DecorationList, RpcError> {
        let host = self.mirror_host(None).await?;
        let id = domain::TmuxPaneId(p.pane);
        // Every line, from the top of history, to check each mark against.
        // A respawn or reflow (exactly when marks change) can rebuild the
        // mirror briefly: wait for it as taps.subscribe does.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let r = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if host.wait_live(&[id], remaining).await
                && let Some(r) = host.read(id, usize::MAX / 2).await
            {
                let r = r.map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
                // A mark made from a line the mirror has not applied yet:
                // give the mirror a moment to catch up.
                if !self.0.decorations.ahead(
                    p.pane,
                    r.generation.0,
                    u32::from(r.cols),
                    r.lines.len(),
                ) || tokio::time::Instant::now() >= deadline
                {
                    break r;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RpcError::new(
                    code::NOT_FOUND,
                    format!("pane %{} has no live mirror", p.pane),
                ));
            }
            // Live a moment ago but gone by the read: look again shortly.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let (decorations, dropped) =
            self.0
                .decorations
                .valid(p.pane, r.generation.0, u32::from(r.cols), &r.lines);
        if dropped {
            self.0.events.decorations_changed(p.pane);
        }
        Ok(api::DecorationList {
            pane: p.pane,
            decorations,
        })
    }

    async fn filters_install(&self, p: api::FilterSpec) -> Result<api::FilterStatus, RpcError> {
        crate::filters::install(&self.0, p).await
    }

    async fn filters_list(&self, _: api::Empty) -> Result<api::FilterList, RpcError> {
        Ok(self.0.filters.list())
    }

    async fn filters_pause(&self, p: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        self.0.filters.pause(&p.name)
    }

    async fn filters_resume(&self, p: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        self.0.filters.resume(&p.name)
    }

    async fn filters_remove(&self, p: api::FilterRef) -> Result<api::FilterStatus, RpcError> {
        self.0.filters.remove(&p.name)
    }

    async fn terminal_query(&self, p: api::TerminalQuery) -> Result<api::QueryResult, RpcError> {
        // Hosts can be searched without a local tmux connection.
        let host = if p.panes.is_empty() && !p.hosts.is_empty() {
            self.mirror_host(None).await.ok()
        } else {
            Some(self.mirror_host(None).await?)
        };
        crate::query::query(&self.0, host, p).await
    }

    async fn taps_subscribe(&self, _: api::TapSubscribe) -> Result<api::TapSubscription, RpcError> {
        Err(for_a_connection("taps.subscribe"))
    }

    async fn taps_unsubscribe(&self, _: api::Unsubscribe) -> Result<api::Empty, RpcError> {
        Err(for_a_connection("taps.unsubscribe"))
    }

    async fn scripts_validate(&self, p: api::ScriptSource) -> Result<api::ScriptCheck, RpcError> {
        Ok(match script_runtime::check(&p.name, &p.source) {
            Ok(()) => api::ScriptCheck {
                ok: true,
                error: None,
            },
            Err(e) => api::ScriptCheck {
                ok: false,
                error: Some(e),
            },
        })
    }

    async fn scripts_load(&self, p: api::ScriptLoad) -> Result<api::ScriptStatus, RpcError> {
        crate::scripts::load(&self.0, p).await
    }

    async fn scripts_reload(&self, p: api::ScriptSource) -> Result<api::ScriptStatus, RpcError> {
        self.0.scripts.reload(p).await
    }

    async fn scripts_stop(&self, p: api::ScriptRef) -> Result<api::ScriptStatus, RpcError> {
        self.0.scripts.stop(&p.name).await
    }

    async fn scripts_list(&self, _: api::Empty) -> Result<api::ScriptList, RpcError> {
        Ok(self.0.scripts.list())
    }

    async fn extensions_list(&self, _: api::Empty) -> Result<api::ExtensionList, RpcError> {
        Ok(self.0.extensions.list())
    }

    async fn extensions_restart(
        &self,
        p: api::ExtensionRef,
    ) -> Result<api::ExtensionStatus, RpcError> {
        self.0.extensions.restart(&p.name)
    }

    async fn grants_narrow(&self, _: api::Grant) -> Result<api::Grant, RpcError> {
        Err(for_a_connection("grants.narrow"))
    }

    async fn apps_register(&self, _: api::RegisterApp) -> Result<api::Registration, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "apps.register is answered for a connection",
        ))
    }

    async fn grants_current(&self, _: api::Empty) -> Result<api::Grant, RpcError> {
        Err(RpcError::new(
            code::INVALID_REQUEST,
            "grants.current is answered for a connection",
        ))
    }

    async fn hosts_call(&self, p: api::HostCall) -> Result<api::HostReply, RpcError> {
        self.0.hosts.call(p).await
    }

    async fn actions_get(&self, p: api::ActionQuery) -> Result<api::ActionRecord, RpcError> {
        self.0.actions.lock().await.get(&p.id)
    }

    async fn actions_list(&self, p: api::ActionListParams) -> Result<api::ActionList, RpcError> {
        let limit = p.limit.unwrap_or(50).clamp(1, 1000) as usize;
        Ok(api::ActionList {
            actions: self.0.actions.lock().await.list(limit),
        })
    }
}

impl DaemonService {
    /// The mirrors of the endpoint, connecting it first if needed.
    pub async fn mirror_host(
        &self,
        endpoint: Option<&str>,
    ) -> Result<mirror::MirrorHost, RpcError> {
        self.check_endpoint(endpoint)?;
        drop(self.0.endpoint().await?);
        self.0
            .mirrors
            .read()
            .await
            .clone()
            .ok_or_else(|| unavailable("no mirrors yet".into()))
    }

    fn check_endpoint(&self, name: Option<&str>) -> Result<(), RpcError> {
        match name {
            Some(n) if n != self.0.endpoint_config.name => Err(RpcError::new(
                code::NOT_FOUND,
                format!(
                    "no endpoint `{n}`; this daemon drives `{}`",
                    self.0.endpoint_config.name
                ),
            )),
            _ => Ok(()),
        }
    }
}

pub fn started_now() -> (Instant, u64) {
    (Instant::now(), now_ms())
}
