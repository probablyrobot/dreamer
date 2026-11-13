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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{LinkState, RpcError, code};
use client::ssh::{Mode, Remote};
use client::{Client, ClientError, Pushed};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};

use crate::log;

/// SSH plus the remote handshake: connecting, authenticating, and possibly
/// starting the remote daemon.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// SSH diagnostic lines kept per host, and the length kept of each.
const DIAG_LINES: usize = 8;
const DIAG_LINE_LEN: usize = 300;

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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    /// Bumped on every connect and disconnect, so a watcher of an earlier
    /// connection cannot overwrite the state of a later one.
    generation: u64,
}

struct Host {
    name: String,
    destination: String,
    link: Mutex<Link>,
    diag: Arc<Mutex<VecDeque<String>>>,
}

impl Host {
    fn status(&self) -> api::HostStatus {
        let l = self.link.lock().unwrap_or_else(|e| e.into_inner());
        api::HostStatus {
            name: self.name.clone(),
            destination: self.destination.clone(),
            state: l.state,
            remote: l.remote.clone(),
            remote_host: l.remote_host.clone(),
            remote_pid: l.remote_pid,
            remote_instance: l.remote_instance.clone(),
            last_contact_unix_ms: l.last_contact_unix_ms,
            detail: l.detail.clone(),
        }
    }

    /// `reason`, followed by SSH's last diagnostic lines, if any.
    fn explain(&self, reason: &str) -> String {
        let diag = self.diag.lock().unwrap_or_else(|e| e.into_inner());
        if diag.is_empty() {
            reason.to_owned()
        } else {
            let lines: Vec<&str> = diag.iter().map(String::as_str).collect();
            format!("{reason}; ssh: {}", lines.join(" | "))
        }
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

impl Hosts {
    pub async fn list(&self) -> api::HostList {
        let hosts = self.hosts.lock().await;
        let mut list: Vec<api::HostStatus> = hosts.values().map(|h| h.status()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        api::HostList { hosts: list }
    }

    pub async fn connect(&self, p: api::HostConnect) -> Result<api::HostStatus, RpcError> {
        if p.name.is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "a host needs a name"));
        }
        let remote = Remote {
            destination: p.destination.clone(),
            dream: p.dream.clone(),
            tmux: p.tmux.clone(),
            tmux_socket: p.tmux_socket.clone(),
            tmux_socket_path: p.tmux_socket_path.clone(),
        };
        // Automatic or not, a connection made by the daemon has no terminal
        // to prompt on: SSH must fail rather than wait for an answer.
        let argv = client::ssh::command(&ssh_program(), &remote, Mode::Batch)
            .map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))?;

        let host = {
            let mut hosts = self.hosts.lock().await;
            if let Some(h) = hosts.get(&p.name) {
                let state = h.link.lock().unwrap_or_else(|e| e.into_inner()).state;
                if state == LinkState::Live as i32 || state == LinkState::Connecting as i32 {
                    return Err(RpcError::new(
                        code::CONFLICT,
                        format!("host `{}` is already connected or connecting", p.name),
                    ));
                }
            }
            let h = Arc::new(Host {
                name: p.name.clone(),
                destination: p.destination.clone(),
                link: Mutex::new(Link {
                    state: LinkState::Connecting as i32,
                    ..Link::default()
                }),
                diag: Arc::default(),
            });
            if let Some(old) = hosts.insert(p.name.clone(), h.clone()) {
                // Carry the generation forward so the old watcher stays stale.
                let g = old
                    .link
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .generation;
                h.link.lock().unwrap_or_else(|e| e.into_inner()).generation = g + 1;
            }
            h
        };
        let generation = host
            .link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation;
        log::info(format!("hosts: connecting `{}` with {argv:?}", p.name));

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
                fail(&host, generation, why.clone());
                return Err(RpcError::new(code::UNAVAILABLE, why));
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(RpcError::new(code::INTERNAL, "ssh pipes missing"));
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
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                let _ = tokio::time::timeout(Duration::from_secs(1), diag_done).await;
                let why = host.explain(&reason(&e));
                fail(&host, generation, why.clone());
                return Err(RpcError::new(code::UNAVAILABLE, why));
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
            let mut l = host.link.lock().unwrap_or_else(|e| e.into_inner());
            if l.generation != generation {
                return Err(RpcError::new(
                    code::CONFLICT,
                    "superseded by another connect",
                ));
            }
            l.state = LinkState::Live as i32;
            l.remote = version;
            l.remote_host = Some(client.welcome.host.clone());
            l.remote_pid = Some(client.welcome.pid);
            l.remote_instance = Some(client.welcome.instance.clone());
            l.last_contact_unix_ms = Some(now_ms());
            l.detail = None;
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
        Ok(host.status())
    }

    pub async fn disconnect(&self, name: &str) -> Result<api::HostStatus, RpcError> {
        let host = self.get(name).await?;
        let (client, child) = {
            let mut l = host.link.lock().unwrap_or_else(|e| e.into_inner());
            l.generation += 1;
            l.state = LinkState::Closed as i32;
            l.detail = Some("disconnected".into());
            (l.client.take(), l.child.take())
        };
        // Ending our side closes SSH's input: the remote bridge ends, and
        // the remote daemon and its tmux keep running.
        drop(client);
        if let Some(mut child) = child
            && tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
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
        let client = {
            let l = host.link.lock().unwrap_or_else(|e| e.into_inner());
            l.client.clone().ok_or_else(|| {
                RpcError::new(
                    code::UNAVAILABLE,
                    format!("host `{}` is not connected", p.name),
                )
            })?
        };
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

fn reason(e: &ClientError) -> String {
    match e {
        ClientError::Wire(wire::WireError::Closed) => {
            "ssh ended before the remote daemon answered".into()
        }
        other => other.to_string(),
    }
}

fn touch(host: &Host) {
    host.link
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_contact_unix_ms = Some(now_ms());
}

fn fail(host: &Host, generation: u64, why: String) {
    let mut l = host.link.lock().unwrap_or_else(|e| e.into_inner());
    if l.generation == generation {
        l.state = LinkState::Failed as i32;
        l.detail = Some(why);
        l.client = None;
        l.child = None;
    }
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
        let mut d = diag.lock().unwrap_or_else(|e| e.into_inner());
        if d.len() == DIAG_LINES {
            d.pop_front();
        }
        d.push_back(line);
    }
}

/// Follow a live connection until it ends, then mark the host failed with
/// the reason and SSH's last words.
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
        let mut l = host.link.lock().unwrap_or_else(|e| e.into_inner());
        if l.generation != generation {
            return;
        }
        l.state = LinkState::Failed as i32;
        l.detail = Some(why.clone());
        l.client = None;
        l.child.take()
    };
    log::warn(format!("hosts: `{}` {why}", host.name));
    if let Some(mut c) = child {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::forwardable;

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
}
