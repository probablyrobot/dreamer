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

/// Methods that are never forwarded: streams (their pushed data has no
/// route back yet) and the coordinator's own methods.
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
}

struct Host {
    name: String,
    config: Mutex<Config>,
    link: Mutex<Link>,
    diag: Arc<Mutex<VecDeque<String>>>,
}

impl Host {
    fn new(name: &str, config: Config) -> Arc<Host> {
        Arc::new(Host {
            name: name.to_owned(),
            config: Mutex::new(config),
            link: Mutex::default(),
            diag: Arc::default(),
        })
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
        {
            let mut l = lock(&host.link);
            if l.generation != generation {
                return Err("superseded by another connect".into());
            }
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
        }
        log::info(format!(
            "hosts: `{}` live: daemon pid {} on {}",
            host.name, client.welcome.pid, client.welcome.host
        ));
        if let Some(rx) = pushed {
            tokio::spawn(watch(host.clone(), generation, rx));
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
async fn watch(host: Arc<Host>, generation: u64, mut pushed: tokio::sync::mpsc::Receiver<Pushed>) {
    let error = loop {
        match pushed.recv().await {
            Some(Pushed::Closed { error }) => break error,
            Some(_) => {}
            None => break None,
        }
    };
    // Let the last diagnostic lines arrive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let why = host.explain(&match error {
        Some(e) => format!("connection lost: {e}"),
        None => "the remote side closed the connection".into(),
    });
    let child = {
        let mut l = lock(&host.link);
        if l.generation != generation {
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
