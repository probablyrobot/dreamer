//! The coordinator role (spec 7.1, 11.1, 11.2): this daemon's outbound
//! connections to daemons on other hosts.
//!
//! Each host is reached through system `ssh` running the fixed
//! `dream bridge --stdio` on the remote side (`client::ssh`). The protocol
//! runs over the SSH child's standard streams; its standard error is kept
//! apart as diagnostics and never parsed as protocol. A connection belongs
//! to this daemon, not to the client that asked for it, so it outlives that
//! client. Remote daemons receive nothing of ours: no keys, no grants, and
//! no way to call other hosts through us.
//!
//! Hosts come from profiles in `hosts.yaml` or are given directly. A
//! dropped connection under an `auto` policy is retried with growing
//! delays. Failures that retrying cannot fix (authentication, host key,
//! incompatible or missing remote) are never retried automatically, so a
//! refused login never becomes a tight loop.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use api::{HostFailure, LinkState, RpcError, code};
use client::ssh::{Mode, Remote};
use client::{Client, ClientError, Pushed};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use workspace_config::hosts::{HostProfile, Reconnect, load_hosts};

use crate::log;

/// SSH plus the remote handshake: connecting, authenticating, and possibly
/// starting the remote daemon.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// SSH diagnostic lines kept per host, and the length kept of each.
const DIAG_LINES: usize = 8;
const DIAG_LINE_LEN: usize = 300;
/// The first retry delay; it doubles up to `BACKOFF_CAP` times this.
const BACKOFF_BASE_MS: u64 = 1000;
const BACKOFF_CAP: u64 = 60;

/// Methods `hosts.call` never forwards: streams, which go through
/// `hosts.subscribe` and `hosts.watch`, and the coordinator's own methods.
fn forwardable(method: &str) -> bool {
    !matches!(
        method,
        "terminal.subscribe" | "terminal.unsubscribe" | "tmux.watch"
    ) && !method.starts_with("hosts.")
}

/// The `ssh` program: `DREAMER_SSH` (tests use a stand-in), else `ssh`
/// from `PATH`.
fn ssh_program() -> String {
    std::env::var("DREAMER_SSH").unwrap_or_else(|_| "ssh".into())
}

fn backoff_base_ms() -> u64 {
    std::env::var("DREAMER_TEST_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(BACKOFF_BASE_MS)
}

/// The delay before retry number `attempts` (1-based): the base doubling
/// each time, capped, with up to a fifth taken off at random so many hosts
/// that dropped together do not retry together.
fn backoff(attempts: u32, base_ms: u64, jitter_seed: u64) -> Duration {
    let doubled = base_ms.saturating_mul(1 << attempts.saturating_sub(1).min(16));
    let ms = doubled.min(base_ms * BACKOFF_CAP);
    let jitter = ms / 5 * (jitter_seed % 1000) / 1000;
    Duration::from_millis(ms - jitter)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why an attempt failed, from what the client saw, SSH's exit status, and
/// SSH's diagnostic lines (matched only to decide whether to retry; the
/// lines themselves are shown to the user unchanged).
fn classify(error: &ClientError, exit: Option<i32>, diag: &[String]) -> HostFailure {
    let said = |needles: &[&str]| diag.iter().any(|l| needles.iter().any(|n| l.contains(n)));
    if said(&[
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "No matching host key",
    ]) {
        HostFailure::HostKey
    } else if said(&[
        "Permission denied",
        "Too many authentication failures",
        "No more authentication methods",
    ]) {
        HostFailure::Auth
    } else if matches!(error, ClientError::Handshake(h) if !matches!(h, wire::io::HandshakeError::Wire(_)))
    {
        HostFailure::Incompatible
    } else if exit == Some(127) || said(&["command not found"]) {
        HostFailure::NotInstalled
    } else {
        HostFailure::Transient
    }
}

/// How a host is reached, and its policy.
#[derive(Debug, Clone, Default)]
struct Config {
    remote: Remote,
    auto: bool,
    tags: Vec<String>,
    workspace: Option<String>,
    profile: bool,
}

impl From<&HostProfile> for Config {
    fn from(p: &HostProfile) -> Config {
        Config {
            remote: Remote {
                destination: p.destination.clone(),
                dream: p.dream.clone(),
                tmux: p.tmux.clone(),
                tmux_socket: p.tmux_socket.clone(),
                tmux_socket_path: p.tmux_socket_path.clone(),
            },
            auto: p.reconnect == Reconnect::Auto,
            tags: p.tags.clone(),
            workspace: p.workspace.clone(),
            profile: true,
        }
    }
}

#[derive(Default)]
struct Link {
    state: i32,
    client: Option<Arc<Client>>,
    child: Option<Child>,
    remote: Option<api::VersionInfo>,
    remote_host: Option<String>,
    remote_pid: Option<u32>,
    remote_instance: Option<String>,
    last_contact_unix_ms: Option<u64>,
    detail: Option<String>,
    failure: i32,
    attempts: u32,
    next_retry_unix_ms: Option<u64>,
    /// Bumped on every connect, reconnect, and disconnect, so a watcher or
    /// retry belonging to an earlier one cannot act on a later one.
    generation: u64,
    /// Bumped each time a link goes live (retries included): pushed data,
    /// relays, and topology belong to one link.
    link: u64,
}

struct Host {
    name: String,
    config: Mutex<Config>,
    link: Mutex<Link>,
    diag: Arc<Mutex<VecDeque<String>>>,
    /// Where pushed data from the current link goes.
    routes: Mutex<Routes>,
    /// The host's latest topology, renumbered here so revisions keep
    /// increasing across reconnects and remote restarts.
    topology: tokio::sync::watch::Sender<Option<Arc<api::TopologyUpdate>>>,
    topology_revision: std::sync::atomic::AtomicU64,
    /// The newest remote revision seen, and on which link: a reply can
    /// arrive after a newer event, and must not overwrite it.
    remote_revision: Mutex<(u64, u64)>,
    /// A local client asked for the topology: watch it on every link.
    watching: std::sync::atomic::AtomicBool,
}

/// Something a relayed subscription receives from the host.
#[derive(Debug)]
pub enum RelayItem {
    Records(api::RecordBatch),
    Checkpoint(api::CheckpointData),
    End(api::SubscriptionEnd),
}

/// Pushed data still waiting per remote subscription before its relay
/// claims it (the remote's first records can overtake the reply).
const UNCLAIMED_ITEMS: usize = 4096;
/// Items queued per relay before it counts as lagging.
const RELAY_QUEUE: usize = 1024;

struct Route {
    tx: tokio::sync::mpsc::Sender<RelayItem>,
    lagged: Arc<std::sync::atomic::AtomicBool>,
}

/// The routes of one live link, by remote subscription id.
#[derive(Default)]
struct Routes {
    link: u64,
    by_sub: HashMap<u64, Route>,
    unclaimed: HashMap<u64, Vec<RelayItem>>,
    spoiled: std::collections::HashSet<u64>,
}

impl Routes {
    fn reset(&mut self, link: u64) {
        *self = Routes {
            link,
            ..Routes::default()
        };
    }

    fn deliver(&mut self, sub: u64, item: RelayItem) {
        let end = matches!(item, RelayItem::End(_));
        if let Some(route) = self.by_sub.get(&sub) {
            if route.tx.try_send(item).is_err() {
                // A relay that cannot keep up is dropped; it ends as lagged.
                route
                    .lagged
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.by_sub.remove(&sub);
            } else if end {
                self.by_sub.remove(&sub);
            }
        } else if !self.spoiled.contains(&sub) {
            let q = self.unclaimed.entry(sub).or_default();
            if q.len() < UNCLAIMED_ITEMS {
                q.push(item);
            } else {
                self.unclaimed.remove(&sub);
                self.spoiled.insert(sub);
            }
        }
    }

    /// Start routing `sub` to `route`, after anything that arrived first.
    fn claim(&mut self, sub: u64, route: Route) {
        if self.spoiled.remove(&sub) {
            route
                .lagged
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        for item in self.unclaimed.remove(&sub).unwrap_or_default() {
            let end = matches!(item, RelayItem::End(_));
            if route.tx.try_send(item).is_err() {
                route
                    .lagged
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            if end {
                return;
            }
        }
        self.by_sub.insert(sub, route);
    }
}

impl Host {
    fn new(name: &str, config: Config) -> Arc<Host> {
        Arc::new(Host {
            name: name.to_owned(),
            config: Mutex::new(config),
            link: Mutex::default(),
            diag: Arc::default(),
            routes: Mutex::default(),
            topology: tokio::sync::watch::channel(None).0,
            topology_revision: 0.into(),
            remote_revision: Mutex::default(),
            watching: false.into(),
        })
    }

    fn publish_topology(&self, generation: u64, u: api::TopologyUpdate) {
        {
            let mut seen = lock(&self.remote_revision);
            if seen.0 == generation && u.revision <= seen.1 {
                return;
            }
            *seen = (generation, u.revision);
        }
        let t = u.topology;
        let revision = self
            .topology_revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.topology
            .send_replace(Some(Arc::new(api::TopologyUpdate {
                revision,
                topology: t,
            })));
    }

    /// The live link's client, if any.
    fn client(&self) -> Option<(Arc<Client>, u64, Option<String>)> {
        let l = lock(&self.link);
        (l.state == LinkState::Live as i32)
            .then(|| {
                l.client
                    .clone()
                    .map(|c| (c, l.link, l.remote_instance.clone()))
            })
            .flatten()
    }

    /// Wait for a live link newer than `after`. `None` when the host will
    /// not come back by itself: disconnected, or failed with no retry.
    async fn next_live(&self, after: u64) -> Option<(Arc<Client>, u64, Option<String>)> {
        loop {
            {
                let l = lock(&self.link);
                let waiting = l.state == LinkState::Connecting as i32
                    || (l.state == LinkState::Failed as i32 && l.next_retry_unix_ms.is_some())
                    || (l.state == LinkState::Live as i32 && l.link == after);
                if l.state == LinkState::Live as i32 && l.link != after {
                    if let Some(c) = l.client.clone() {
                        return Some((c, l.link, l.remote_instance.clone()));
                    }
                } else if !waiting {
                    return None;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn status(&self) -> api::HostStatus {
        status_of(&self.name, &lock(&self.config), Some(&lock(&self.link)))
    }

    fn diag_lines(&self) -> Vec<String> {
        lock(&self.diag).iter().cloned().collect()
    }

    /// `reason`, followed by SSH's last diagnostic lines, if any.
    fn explain(&self, reason: &str) -> String {
        let lines = self.diag_lines();
        if lines.is_empty() {
            reason.to_owned()
        } else {
            format!("{reason}; ssh: {}", lines.join(" | "))
        }
    }

    /// Start a new generation in `state`, ending whatever the previous one
    /// had. Returns the generation and the old client and child, to be
    /// closed outside the lock.
    fn restart(&self, state: LinkState) -> (u64, Option<Arc<Client>>, Option<Child>) {
        let mut l = lock(&self.link);
        l.generation += 1;
        // Link ids start at 1: no link's pushed data matches 0.
        lock(&self.routes).reset(0);
        l.state = state as i32;
        l.next_retry_unix_ms = None;
        if state == LinkState::Connecting {
            l.attempts = 0;
            l.failure = HostFailure::Unspecified as i32;
        }
        (l.generation, l.client.take(), l.child.take())
    }
}

fn status_of(name: &str, c: &Config, link: Option<&Link>) -> api::HostStatus {
    let mut s = api::HostStatus {
        name: name.to_owned(),
        destination: c.remote.destination.clone(),
        auto_reconnect: c.auto,
        tags: c.tags.clone(),
        workspace: c.workspace.clone(),
        profile: c.profile,
        ..api::HostStatus::default()
    };
    if let Some(l) = link {
        s.state = l.state;
        s.remote = l.remote.clone();
        s.remote_host = l.remote_host.clone();
        s.remote_pid = l.remote_pid;
        s.remote_instance = l.remote_instance.clone();
        s.last_contact_unix_ms = l.last_contact_unix_ms;
        s.detail = l.detail.clone();
        s.failure = l.failure;
        s.attempts = l.attempts;
        s.next_retry_unix_ms = l.next_retry_unix_ms;
    }
    s
}

/// Close a connection's client (which ends SSH's input, so the remote
/// bridge ends) and then its SSH child.
async fn close(client: Option<Arc<Client>>, child: Option<Child>) {
    drop(client);
    if let Some(mut child) = child
        && tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Every host this daemon has been asked to connect to.
#[derive(Default)]
pub struct Hosts {
    hosts: tokio::sync::Mutex<HashMap<String, Arc<Host>>>,
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts").finish_non_exhaustive()
    }
}

fn profile(name: &str) -> Result<HostProfile, RpcError> {
    let profiles = load_hosts().map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))?;
    profiles
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            RpcError::new(
                code::NOT_FOUND,
                format!("no host named `{name}` in hosts.yaml"),
            )
        })
}

impl Hosts {
    /// Profiles first, in file order, then hosts given directly, by name.
    pub async fn list(&self) -> api::HostList {
        let hosts = self.hosts.lock().await;
        let (profiles, profiles_error) = match load_hosts() {
            Ok(p) => (p, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let mut list = Vec::new();
        for p in &profiles {
            list.push(match hosts.get(&p.name) {
                Some(h) => h.status(),
                None => status_of(&p.name, &Config::from(p), None),
            });
        }
        let mut others: Vec<api::HostStatus> = hosts
            .values()
            .filter(|h| !profiles.iter().any(|p| p.name == h.name))
            .map(|h| h.status())
            .collect();
        others.sort_by(|a, b| a.name.cmp(&b.name));
        list.extend(others);
        api::HostList {
            hosts: list,
            profiles_error,
        }
    }

    pub async fn connect(&self, p: api::HostConnect) -> Result<api::HostStatus, RpcError> {
        if p.name.is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "a host needs a name"));
        }
        let mut config = if p.destination.is_empty() {
            Config::from(&profile(&p.name)?)
        } else {
            Config {
                remote: Remote {
                    destination: p.destination.clone(),
                    dream: p.dream.clone(),
                    tmux: p.tmux.clone(),
                    tmux_socket: p.tmux_socket.clone(),
                    tmux_socket_path: p.tmux_socket_path.clone(),
                },
                ..Config::default()
            }
        };
        if let Some(auto) = p.auto_reconnect {
            config.auto = auto;
        }
        client::ssh::command(&ssh_program(), &config.remote, Mode::Batch)
            .map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))?;
        let host = {
            let mut hosts = self.hosts.lock().await;
            let host = hosts
                .entry(p.name.clone())
                .or_insert_with(|| Host::new(&p.name, config.clone()))
                .clone();
            let state = lock(&host.link).state;
            if state == LinkState::Live as i32 || state == LinkState::Connecting as i32 {
                return Err(RpcError::new(
                    code::CONFLICT,
                    format!("host `{}` is already connected or connecting", p.name),
                ));
            }
            *lock(&host.config) = config;
            host
        };
        start(host).await
    }

    /// Connect again now: after a failure that is never retried by itself,
    /// to skip a backoff wait, or to replace a live connection.
    pub async fn reconnect(&self, name: &str) -> Result<api::HostStatus, RpcError> {
        let host = {
            let mut hosts = self.hosts.lock().await;
            match hosts.get(name) {
                Some(h) => h.clone(),
                None => {
                    let h = Host::new(name, Config::from(&profile(name)?));
                    hosts.insert(name.to_owned(), h.clone());
                    h
                }
            }
        };
        if let Ok(p) = profile(name) {
            *lock(&host.config) = Config::from(&p);
        }
        start(host).await
    }

    pub async fn disconnect(&self, name: &str) -> Result<api::HostStatus, RpcError> {
        let host = self.get(name).await?;
        let (_, client, child) = host.restart(LinkState::Closed);
        lock(&host.link).detail = Some("disconnected".into());
        close(client, child).await;
        log::info(format!("hosts: `{name}` disconnected"));
        Ok(host.status())
    }

    pub async fn call(&self, p: api::HostCall) -> Result<api::HostReply, RpcError> {
        if !forwardable(&p.method) {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!("`{}` cannot be forwarded to another host", p.method),
            ));
        }
        let host = self.get(&p.name).await?;
        let client = lock(&host.link).client.clone().ok_or_else(|| {
            RpcError::new(
                code::UNAVAILABLE,
                format!("host `{}` is not connected", p.name),
            )
        })?;
        match client.call_raw(&p.method, p.payload).await {
            Ok(payload) => {
                touch(&host);
                Ok(api::HostReply { payload })
            }
            // The remote daemon answered: the link is fine.
            Err(ClientError::Rpc(e)) => {
                touch(&host);
                Err(e)
            }
            Err(e) => Err(RpcError::new(
                code::UNAVAILABLE,
                format!("host `{}`: {e}", p.name),
            )),
        }
    }

    /// How `name` is reached: a host the daemon knows, else its profile.
    pub async fn remote_of(&self, name: &str) -> Result<Remote, RpcError> {
        if let Some(h) = self.hosts.lock().await.get(name) {
            return Ok(lock(&h.config).remote.clone());
        }
        Ok(Config::from(&profile(name)?).remote)
    }

    async fn get(&self, name: &str) -> Result<Arc<Host>, RpcError> {
        self.hosts
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no host named `{name}`")))
    }
}

/// A fresh generation's first attempt, after closing any earlier link.
async fn start(host: Arc<Host>) -> Result<api::HostStatus, RpcError> {
    let (generation, client, child) = host.restart(LinkState::Connecting);
    close(client, child).await;
    match attempt(host.clone(), generation).await {
        Ok(()) => Ok(host.status()),
        Err(why) => Err(RpcError::new(code::UNAVAILABLE, why)),
    }
}

/// One connection attempt for `generation`. On failure the host is marked
/// failed and, when the policy and the kind of failure allow, a retry is
/// scheduled.
fn attempt(
    host: Arc<Host>,
    generation: u64,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move {
        let remote = lock(&host.config).remote.clone();
        let argv = client::ssh::command(&ssh_program(), &remote, Mode::Batch)
            .map_err(|e| e.to_string())?;
        lock(&host.diag).clear();
        log::info(format!("hosts: connecting `{}` with {argv:?}", host.name));
        let mut child = match Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let why = format!("could not run {}: {e}", argv[0]);
                failed(&host, generation, HostFailure::NotInstalled, why.clone());
                return Err(why);
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("ssh pipes missing".into());
        };
        let diag_done = tokio::spawn(collect_diagnostics(
            stderr,
            host.diag.clone(),
            host.name.clone(),
        ));
        let connected =
            Client::over_within(stdout, stdin, "coordinator", 64, CONNECT_TIMEOUT).await;
        let mut client = match connected {
            Ok(c) => c,
            Err(e) => {
                // Let SSH finish by itself so its status and last words are
                // known; stop it only if it does not.
                let exit = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                    Ok(Ok(s)) => s.code(),
                    _ => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        None
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(1), diag_done).await;
                let kind = classify(&e, exit, &host.diag_lines());
                let why = host.explain(&reason(&e));
                failed(&host, generation, kind, why.clone());
                return Err(why);
            }
        };
        let version = match client
            .call(api::Request::DaemonVersion(api::Empty {}))
            .await
        {
            Ok(api::Response::DaemonVersion(v)) => Some(v),
            _ => None,
        };
        let pushed = client.pushed();
        let client = Arc::new(client);
        let link = {
            let mut l = lock(&host.link);
            if l.generation != generation {
                return Err("superseded by another connect".into());
            }
            l.link += 1;
            l.state = LinkState::Live as i32;
            l.remote = version;
            l.remote_host = Some(client.welcome.host.clone());
            l.remote_pid = Some(client.welcome.pid);
            l.remote_instance = Some(client.welcome.instance.clone());
            l.last_contact_unix_ms = Some(now_ms());
            l.detail = None;
            l.failure = HostFailure::Unspecified as i32;
            l.attempts = 0;
            l.next_retry_unix_ms = None;
            l.client = Some(client.clone());
            l.child = Some(child);
            lock(&host.routes).reset(l.link);
            l.link
        };
        log::info(format!(
            "hosts: `{}` live: daemon pid {} on {}",
            host.name, client.welcome.pid, client.welcome.host
        ));
        if let Some(rx) = pushed {
            tokio::spawn(watch(host.clone(), generation, link, rx));
        }
        if host.watching.load(std::sync::atomic::Ordering::SeqCst) {
            watch_topology(&host, &client, link).await;
        }
        Ok(())
    })
}

/// Mark `generation` failed, and schedule a retry when that can help.
fn failed(host: &Arc<Host>, generation: u64, kind: HostFailure, why: String) {
    let auto = lock(&host.config).auto;
    let delay = {
        let mut l = lock(&host.link);
        if l.generation != generation {
            return;
        }
        l.state = LinkState::Failed as i32;
        l.failure = kind as i32;
        l.detail = Some(why.clone());
        l.client = None;
        l.child = None;
        l.attempts += 1;
        let retry = auto && kind == HostFailure::Transient;
        let delay = retry.then(|| backoff(l.attempts, backoff_base_ms(), now_ms()));
        l.next_retry_unix_ms = delay.map(|d| now_ms() + d.as_millis() as u64);
        delay
    };
    match delay {
        Some(d) => {
            log::warn(format!(
                "hosts: `{}` {why}; retrying in {} ms",
                host.name,
                d.as_millis()
            ));
            tokio::spawn(retry(host.clone(), generation, d));
        }
        None => log::warn(format!(
            "hosts: `{}` {why}; not retrying ({})",
            host.name,
            kind.name()
        )),
    }
}

async fn retry(host: Arc<Host>, generation: u64, delay: Duration) {
    tokio::time::sleep(delay).await;
    {
        let mut l = lock(&host.link);
        if l.generation != generation || l.state != LinkState::Failed as i32 {
            return;
        }
        l.state = LinkState::Connecting as i32;
        l.next_retry_unix_ms = None;
    }
    let _ = attempt(host, generation).await;
}

fn reason(e: &ClientError) -> String {
    match e {
        ClientError::Wire(wire::WireError::Closed) => {
            "ssh ended before the remote daemon answered".into()
        }
        other => other.to_string(),
    }
}

fn touch(host: &Host) {
    lock(&host.link).last_contact_unix_ms = Some(now_ms());
}

/// Keep SSH's last few diagnostic lines, and log them.
async fn collect_diagnostics(
    stderr: ChildStderr,
    diag: Arc<Mutex<VecDeque<String>>>,
    name: String,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line: String = line.chars().take(DIAG_LINE_LEN).collect();
        log::info(format!("hosts: `{name}` ssh: {line}"));
        let mut d = lock(&diag);
        if d.len() == DIAG_LINES {
            d.pop_front();
        }
        d.push_back(line);
    }
}

/// Follow a live connection until it ends, then mark the host failed with
/// the reason and SSH's last words (and retry under an auto policy).
async fn watch(
    host: Arc<Host>,
    generation: u64,
    link: u64,
    mut pushed: tokio::sync::mpsc::Receiver<Pushed>,
) {
    let error = loop {
        match pushed.recv().await {
            Some(Pushed::Closed { error }) => break error,
            Some(p) => dispatch(&host, link, p),
            None => break None,
        }
    };
    {
        // The relays of this link see their channel close: link lost.
        let mut routes = lock(&host.routes);
        if routes.link == link {
            routes.reset(0);
        }
    }
    // Let the last diagnostic lines arrive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let why = host.explain(&match error {
        Some(e) => format!("connection lost: {e}"),
        None => "the remote side closed the connection".into(),
    });
    let child = {
        let mut l = lock(&host.link);
        if l.generation != generation || l.link != link {
            return;
        }
        l.client = None;
        l.child.take()
    };
    if let Some(mut c) = child {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
    failed(&host, generation, HostFailure::Transient, why);
}

/// Route one pushed item of `link` to its relay or topology.
fn dispatch(host: &Host, link: u64, p: Pushed) {
    use prost::Message as _;
    let (sub, item) = match p {
        Pushed::Event { topic, payload, .. } => match topic.as_str() {
            "terminal.records" => match api::RecordBatch::decode(payload.as_slice()) {
                Ok(b) => (b.subscription, RelayItem::Records(b)),
                Err(_) => return,
            },
            "terminal.end" => match api::SubscriptionEnd::decode(payload.as_slice()) {
                Ok(e) => (e.subscription, RelayItem::End(e)),
                Err(_) => return,
            },
            "tmux.topology" => {
                if let Ok(u) = api::TopologyUpdate::decode(payload.as_slice()) {
                    host.publish_topology(link, u);
                }
                return;
            }
            _ => return,
        },
        Pushed::Bulk { topic, data, .. } if topic == "terminal.checkpoint" => {
            match api::CheckpointData::decode(data.as_slice()) {
                Ok(d) => (d.subscription, RelayItem::Checkpoint(d)),
                Err(_) => return,
            }
        }
        _ => return,
    };
    let mut routes = lock(&host.routes);
    if routes.link == link {
        routes.deliver(sub, item);
    }
}

/// Ask the host's daemon for its topology and the changes after it.
async fn watch_topology(host: &Host, client: &Client, generation: u64) {
    if let Ok(api::Response::TmuxWatch(u)) = client
        .call(api::Request::TmuxWatch(api::TopologyParams::default()))
        .await
    {
        host.publish_topology(generation, u);
    }
}

/// The host connection and remote subscription id a relay is on now, so
/// dropping the relay can unsubscribe there.
type Current = Arc<Mutex<Option<(Arc<Client>, u64)>>>;

/// A relayed subscription, held by the local connection that asked for it.
/// Dropping it stops the relay and unsubscribes on the host.
pub struct Relay {
    pub subscription: api::Subscription,
    pub items: tokio::sync::mpsc::Receiver<RelayItem>,
    task: tokio::task::JoinHandle<()>,
    current: Current,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        if let Some((client, sub)) = lock(&self.current).take() {
            tokio::spawn(async move {
                let _ = client
                    .call(api::Request::TerminalUnsubscribe(api::Unsubscribe {
                        subscription: sub,
                    }))
                    .await;
            });
        }
    }
}

fn end(reason: api::EndReason, detail: &str) -> RelayItem {
    RelayItem::End(api::SubscriptionEnd {
        subscription: 0,
        reason: reason as i32,
        detail: Some(detail.to_owned()),
    })
}

/// Subscribe on the host and claim the route for the reply's id.
async fn subscribe_remote(
    host: &Host,
    client: &Arc<Client>,
    link: u64,
    params: api::TerminalSubscribe,
) -> Result<
    (
        api::Subscription,
        tokio::sync::mpsc::Receiver<RelayItem>,
        Arc<std::sync::atomic::AtomicBool>,
    ),
    ClientError,
> {
    let api::Response::TerminalSubscribe(s) =
        client.call(api::Request::TerminalSubscribe(params)).await?
    else {
        return Err(ClientError::Wire(wire::WireError::Unexpected(
            "a reply to another method",
        )));
    };
    let (tx, rx) = tokio::sync::mpsc::channel(RELAY_QUEUE);
    let lagged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut routes = lock(&host.routes);
    if routes.link == link {
        routes.claim(
            s.id,
            Route {
                tx,
                lagged: lagged.clone(),
            },
        );
    }
    // Otherwise the link already ended: `tx` drops here and the relay
    // sees a lost link straight away.
    Ok((s, rx, lagged))
}

/// Carry one subscription across link losses: forward what the host sends,
/// and after a reconnect resume from the last delivered record. The host's
/// mirror stayed valid, so its journal replays only what was missed. When
/// it cannot (the daemon restarted, the journal moved on, the host is
/// gone), end with the reason.
#[allow(clippy::too_many_arguments)]
async fn relay(
    host: Arc<Host>,
    mut params: api::TerminalSubscribe,
    mut sub: api::Subscription,
    mut rx: tokio::sync::mpsc::Receiver<RelayItem>,
    mut lagged: Arc<std::sync::atomic::AtomicBool>,
    mut generation: u64,
    mut instance: Option<String>,
    out: tokio::sync::mpsc::Sender<RelayItem>,
    current: Current,
) {
    let mut last = sub.sequence;
    loop {
        let item = match rx.recv().await {
            Some(RelayItem::Records(b)) => {
                if let Some(r) = b.records.last() {
                    last = last.max(r.sequence);
                }
                RelayItem::Records(b)
            }
            Some(RelayItem::Checkpoint(d)) => {
                last = last.max(d.sequence);
                RelayItem::Checkpoint(d)
            }
            Some(e @ RelayItem::End(_)) => {
                lock(&current).take();
                let _ = out.send(e).await;
                return;
            }
            None if lagged.load(std::sync::atomic::Ordering::SeqCst) => {
                let _ = out
                    .send(end(api::EndReason::Lagged, "the relay fell behind"))
                    .await;
                return;
            }
            None => {
                // The link ended. Wait for the next one, then resume.
                lock(&current).take();
                let Some((client, g, now)) = host.next_live(generation).await else {
                    let _ = out
                        .send(end(
                            api::EndReason::HostLost,
                            "the host is not coming back by itself",
                        ))
                        .await;
                    return;
                };
                params.resume_epoch = Some(sub.epoch);
                params.resume_generation = Some(sub.generation);
                params.resume_sequence = Some(last);
                match subscribe_remote(&host, &client, g, params.clone()).await {
                    Ok((s, r, l)) if s.mode == api::SubscribeMode::Replay as i32 => {
                        log::info(format!(
                            "hosts: `{}` pane %{} resumed after sequence {last}",
                            host.name, s.pane
                        ));
                        *lock(&current) = Some((client, s.id));
                        (sub, rx, lagged, generation) = (s, r, l, g);
                        instance = now;
                        continue;
                    }
                    Ok((s, _, _)) => {
                        let _ = client
                            .call(api::Request::TerminalUnsubscribe(api::Unsubscribe {
                                subscription: s.id,
                            }))
                            .await;
                        let (reason, why) = if now != instance {
                            (api::EndReason::HostRestarted, "the host's daemon restarted")
                        } else if s.epoch != sub.epoch {
                            (
                                api::EndReason::ConnectionLost,
                                "the host's daemon lost its tmux connection",
                            )
                        } else {
                            (
                                api::EndReason::Lagged,
                                "the host's journal no longer holds the missed records",
                            )
                        };
                        let _ = out.send(end(reason, why)).await;
                        return;
                    }
                    Err(ClientError::Rpc(e)) if e.code == code::NOT_FOUND => {
                        let _ = out.send(end(api::EndReason::PaneClosed, &e.message)).await;
                        return;
                    }
                    Err(_) => {
                        // That link died too; wait for the next.
                        generation = g;
                        let (_tx, r) = tokio::sync::mpsc::channel(1);
                        rx = r;
                        lagged = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        continue;
                    }
                }
            }
        };
        if out.send(item).await.is_err() {
            return;
        }
    }
}

impl Hosts {
    /// Follow a pane on a live host through a relay.
    pub async fn subscribe(&self, p: api::HostSubscribe) -> Result<Relay, RpcError> {
        let host = self.get(&p.name).await?;
        let params = p.subscribe.unwrap_or_default();
        let (client, generation, instance) = host.client().ok_or_else(|| {
            RpcError::new(
                code::UNAVAILABLE,
                format!("host `{}` is not connected", p.name),
            )
        })?;
        let (sub, rx, lagged) = subscribe_remote(&host, &client, generation, params.clone())
            .await
            .map_err(|e| match e {
                ClientError::Rpc(e) => e,
                other => RpcError::new(code::UNAVAILABLE, format!("host `{}`: {other}", p.name)),
            })?;
        touch(&host);
        let current = Arc::new(Mutex::new(Some((client, sub.id))));
        let (out, items) = tokio::sync::mpsc::channel(RELAY_QUEUE);
        let task = tokio::spawn(relay(
            host,
            params,
            sub.clone(),
            rx,
            lagged,
            generation,
            instance,
            out,
            current.clone(),
        ));
        Ok(Relay {
            subscription: sub,
            items,
            task,
            current,
        })
    }

    /// The host's topology now, and a receiver of every later one.
    pub async fn watch(
        &self,
        name: &str,
    ) -> Result<
        (
            api::TopologyUpdate,
            tokio::sync::watch::Receiver<Option<Arc<api::TopologyUpdate>>>,
        ),
        RpcError,
    > {
        let host = self.get(name).await?;
        let first = !host
            .watching
            .swap(true, std::sync::atomic::Ordering::SeqCst);
        if first && let Some((client, generation, _)) = host.client() {
            watch_topology(&host, &client, generation).await;
        }
        let rx = host.topology.subscribe();
        let latest = rx.borrow().clone();
        match latest {
            Some(u) => Ok(((*u).clone(), rx)),
            None => Err(RpcError::new(
                code::UNAVAILABLE,
                format!("host `{name}` is not connected"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_and_coordinator_methods_are_not_forwarded() {
        for m in [
            "terminal.subscribe",
            "terminal.unsubscribe",
            "tmux.watch",
            "hosts.call",
            "hosts.connect",
        ] {
            assert!(!forwardable(m), "{m}");
        }
        for m in [
            "tmux.topology",
            "terminal.read",
            "actions.submit",
            "daemon.status",
        ] {
            assert!(forwardable(m), "{m}");
        }
    }

    #[test]
    fn backoff_doubles_to_a_cap_and_jitter_only_shortens() {
        let ms = |a, seed| backoff(a, 1000, seed).as_millis();
        assert_eq!(
            [ms(1, 0), ms(2, 0), ms(3, 0), ms(7, 0)],
            [1000, 2000, 4000, 60_000]
        );
        assert_eq!(ms(40, 0), 60_000);
        assert!((800..=1000).contains(&ms(1, 999)));
    }

    #[test]
    fn only_failures_retrying_can_fix_are_transient() {
        let closed = ClientError::Wire(wire::WireError::Closed);
        let lines = |l: &str| vec![l.to_owned()];
        assert_eq!(
            classify(
                &closed,
                Some(255),
                &lines("me@devbox: Permission denied (publickey).")
            ),
            HostFailure::Auth
        );
        assert_eq!(
            classify(&closed, Some(255), &lines("Host key verification failed.")),
            HostFailure::HostKey
        );
        assert_eq!(
            classify(&closed, Some(127), &lines("sh: dream: command not found")),
            HostFailure::NotInstalled
        );
        assert_eq!(
            classify(
                &closed,
                Some(255),
                &lines("ssh: connect to host devbox port 22: Connection refused")
            ),
            HostFailure::Transient
        );
        assert_eq!(classify(&closed, None, &[]), HostFailure::Transient);
    }
}
