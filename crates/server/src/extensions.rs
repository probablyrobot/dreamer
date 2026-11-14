//! Supervised extensions (spec 12.2): programs from `extensions.yaml`
//! that the daemon starts, watches, and restarts.
//!
//! An extension speaks JSON-RPC 2.0 on its standard output (requests to
//! the daemon, replies to the daemon's requests) and reads the daemon's
//! replies and requests on its standard input; standard error goes to its
//! log. Its connection holds the grant from `extensions.yaml` and the
//! broker identity `extension:<name>`.
//!
//! The daemon asks it `extension.hello` when it starts, `$/ping` on a
//! heartbeat, and `extension.shutdown` before stopping it. A crash, a
//! missed heartbeat (a stall), or a missing hello kills it and starts it
//! again after a growing delay; after `MAX_FAILURES` in a row it is parked
//! as failed until `extensions.restart`. Nothing in the terminal path
//! waits for an extension: its I/O runs on its own tasks with bounded
//! queues.
//!
//! Process separation isolates crashes; it is not an OS sandbox. An
//! extension runs with the user's privileges.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::{ExtensionState, ExtensionStatus, RpcError, code};
use serde_json::{Value, json};
use tokio::process::Child;
use tokio::sync::{Notify, mpsc, oneshot};
use workspace_config::extensions::Extension;

use crate::jsonrpc::PeerCall;
use crate::log;
use crate::service::State;

/// Failures in a row before an extension is parked.
pub const MAX_FAILURES: u32 = 8;
/// Running this long resets the failure count.
const STABLE: Duration = Duration::from_secs(60);
const HELLO: Duration = Duration::from_secs(10);
const SHUTDOWN: Duration = Duration::from_secs(1);
const TERM: Duration = Duration::from_secs(2);

fn env_ms(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// How often the daemon pings, and how long an answer may take.
fn heartbeat() -> Duration {
    env_ms("DREAMER_TEST_EXTENSION_HEARTBEAT_MS").unwrap_or(Duration::from_secs(10))
}

/// The first restart delay; it doubles per failure, up to a minute.
fn backoff_base() -> Duration {
    env_ms("DREAMER_TEST_EXTENSION_BACKOFF_MS").unwrap_or(Duration::from_millis(500))
}

fn backoff(failures: u32) -> Duration {
    backoff_from(backoff_base(), failures)
}

/// `base` doubled per failure after the first, at most a minute, with
/// ±20% jitter so things failing together do not restart together.
pub fn backoff_from(base: Duration, failures: u32) -> Duration {
    let d = base.saturating_mul(1 << failures.saturating_sub(1).min(16));
    let d = d.min(Duration::from_secs(60));
    // ±20% jitter, so extensions failing together do not restart together.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos());
    let jitter = f64::from(nanos % 400) / 1000.0 - 0.2;
    d.mul_f64(1.0 + jitter)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn grant_of(x: &Extension) -> api::Grant {
    api::Grant {
        owner: false,
        methods: x.grant.methods.clone(),
        panes: x.grant.panes.clone(),
        all_panes: x.grant.all_panes,
        expires_unix_ms: None,
    }
}

pub fn log_path(name: &str) -> std::path::PathBuf {
    platform::state_dir()
        .join("extensions")
        .join(format!("{name}.log"))
}

struct Ext {
    config: Extension,
    status: Mutex<ExtensionStatus>,
    /// `extensions.restart` or daemon shutdown.
    wake: Notify,
    restart: AtomicBool,
}

impl Ext {
    fn update(&self, f: impl FnOnce(&mut ExtensionStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn status(&self) -> ExtensionStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Extensions {
    exts: Mutex<Vec<Arc<Ext>>>,
    config_error: Mutex<Option<String>>,
    stopping: AtomicBool,
    tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions").finish_non_exhaustive()
    }
}

/// Load `extensions.yaml` and start every enabled extension.
pub async fn start_all(state: &Arc<State>) {
    let configs = match workspace_config::extensions::load_extensions() {
        Ok(c) => c,
        Err(e) => {
            log::warn(format!("extensions: {e}"));
            if let Ok(mut ce) = state.extensions.config_error.lock() {
                *ce = Some(e.to_string());
            }
            return;
        }
    };
    let mut tasks = state.extensions.tasks.lock().await;
    for config in configs {
        let ext = Arc::new(Ext {
            status: Mutex::new(ExtensionStatus {
                name: config.name.clone(),
                state: if config.enabled {
                    ExtensionState::Starting
                } else {
                    ExtensionState::Disabled
                } as i32,
                log: log_path(&config.name).display().to_string(),
                grant: Some(grant_of(&config)),
                ..ExtensionStatus::default()
            }),
            config,
            wake: Notify::new(),
            restart: AtomicBool::new(false),
        });
        if let Ok(mut e) = state.extensions.exts.lock() {
            e.push(ext.clone());
        }
        if ext.config.enabled {
            tasks.push(tokio::spawn(supervise(state.clone(), ext)));
        }
    }
}

impl Extensions {
    pub fn list(&self) -> api::ExtensionList {
        api::ExtensionList {
            extensions: self
                .exts
                .lock()
                .map(|e| e.iter().map(|x| x.status()).collect())
                .unwrap_or_default(),
            config_error: self.config_error.lock().ok().and_then(|e| e.clone()),
        }
    }

    pub fn restart(&self, name: &str) -> Result<ExtensionStatus, RpcError> {
        let ext = self
            .exts
            .lock()
            .ok()
            .and_then(|e| e.iter().find(|x| x.config.name == name).cloned())
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no extension `{name}`")))?;
        if !ext.config.enabled {
            return Err(RpcError::new(
                code::PRECONDITION,
                format!("`{name}` is disabled in extensions.yaml"),
            ));
        }
        ext.restart.store(true, Ordering::SeqCst);
        ext.wake.notify_one();
        Ok(ext.status())
    }

    /// Stop every extension (asking first) and wait for them.
    pub async fn stop_all(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(e) = self.exts.lock() {
            for x in e.iter() {
                x.wake.notify_one();
            }
        }
        let tasks: Vec<_> = self.tasks.lock().await.drain(..).collect();
        let all = async {
            for t in tasks {
                let _ = t.await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(10), all).await;
    }
}

/// How one run of an extension ended.
enum Ended {
    /// Asked to stop (restart or shutdown); not a failure.
    Stopped,
    Failed(String),
}

async fn supervise(state: Arc<State>, ext: Arc<Ext>) {
    let name = ext.config.name.clone();
    let mut failures = 0u32;
    let mut first = true;
    loop {
        if state.extensions.stopping.load(Ordering::SeqCst) {
            break;
        }
        ext.restart.store(false, Ordering::SeqCst);
        if !first {
            ext.update(|s| s.restarts += 1);
        }
        first = false;
        ext.update(|s| {
            s.state = ExtensionState::Starting as i32;
            s.next_start_unix_ms = None;
            s.started_unix_ms = Some(now_ms());
        });
        let started = Instant::now();
        let ended = run_once(&state, &ext).await;
        ext.update(|s| s.pid = None);
        if state.extensions.stopping.load(Ordering::SeqCst) {
            break;
        }
        match ended {
            Ended::Stopped => {
                failures = 0;
                ext.update(|s| s.failures = 0);
                continue;
            }
            Ended::Failed(why) => {
                if started.elapsed() >= STABLE {
                    failures = 0;
                }
                failures += 1;
                log::warn(format!("extension `{name}`: {why}"));
                ext.update(|s| {
                    s.failures = failures;
                    s.last_error = Some(why.clone());
                });
            }
        }
        if failures >= MAX_FAILURES {
            ext.update(|s| s.state = ExtensionState::Failed as i32);
            log::warn(format!(
                "extension `{name}`: parked after {failures} failures in a row"
            ));
            // Until extensions.restart (or shutdown).
            loop {
                ext.wake.notified().await;
                if state.extensions.stopping.load(Ordering::SeqCst)
                    || ext.restart.load(Ordering::SeqCst)
                {
                    break;
                }
            }
            failures = 0;
            ext.update(|s| s.failures = 0);
            continue;
        }
        let delay = backoff(failures);
        ext.update(|s| {
            s.state = ExtensionState::Restarting as i32;
            s.next_start_unix_ms = Some(now_ms() + delay.as_millis() as u64);
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = ext.wake.notified() => {}
        }
    }
    ext.update(|s| {
        s.state = ExtensionState::Stopped as i32;
        s.next_start_unix_ms = None;
    });
}

/// Ask the extension `method`; its result, or why there is none.
async fn ask(
    calls: &mpsc::Sender<PeerCall>,
    method: &'static str,
    params: Value,
    within: Duration,
) -> Result<Value, String> {
    let (tx, rx) = oneshot::channel();
    calls
        .try_send(PeerCall {
            method,
            params,
            reply: tx,
        })
        .map_err(|_| format!("could not send {method}"))?;
    match tokio::time::timeout(within, rx).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => Err(format!("{method} failed: {e}")),
        Ok(Err(_)) => Err(format!("the connection ended before it answered {method}")),
        Err(_) => Err(format!(
            "no answer to {method} within {} ms",
            within.as_millis()
        )),
    }
}

fn spawn(x: &Extension) -> Result<Child, String> {
    let log = log_path(&x.name);
    if let Some(dir) = log.parent() {
        platform::private_dir(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    platform::owner_only(&log)?;
    let mut cmd = tokio::process::Command::new(&x.argv[0]);
    cmd.args(&x.argv[1..])
        .env("DREAMER_EXTENSION", &x.name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(file)
        .kill_on_drop(true)
        // Its own process group, so stopping it stops what it started.
        .process_group(0);
    if let Some(cwd) = &x.cwd {
        cmd.current_dir(cwd);
    }
    cmd.spawn()
        .map_err(|e| format!("could not start {}: {e}", x.argv[0]))
}

/// Signal the extension's process group.
fn signal(pid: u32, sig: &str) {
    let _ = std::process::Command::new("kill")
        .args([sig, "--", &format!("-{pid}")])
        .stderr(std::process::Stdio::null())
        .status();
}

async fn run_once(state: &Arc<State>, ext: &Arc<Ext>) -> Ended {
    let x = &ext.config;
    let mut child = match spawn(x) {
        Ok(c) => c,
        Err(e) => return Ended::Failed(e),
    };
    let pid = child.id();
    ext.update(|s| s.pid = pid);
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        return Ended::Failed("no standard input or output".into());
    };
    let (calls, calls_rx) = mpsc::channel(16);
    let identity = crate::messages::Identity {
        instance: format!("extension:{}", x.name),
        app_id: Some(x.name.clone()),
        pane: None,
    };
    let mut serving = tokio::spawn(crate::jsonrpc::serve_as(
        stdout,
        stdin,
        state.clone(),
        pid,
        crate::grants::Grant(grant_of(x)),
        Some(identity),
        Some(calls_rx),
    ));
    let stop = |child: &mut Child| {
        if let Some(pid) = pid {
            signal(pid, "-KILL");
        }
        let _ = child.start_kill();
    };
    let hello = ask(
        &calls,
        "extension.hello",
        json!({
            "name": x.name,
            "protocol": format!("{}.{}", api::PROTOCOL_MAJOR, api::PROTOCOL_MINOR),
            "daemon_version": state.identity.software_version,
        }),
        HELLO,
    );
    tokio::select! {
        r = hello => match r {
            Ok(v) => {
                let version = v.get("version").and_then(Value::as_str).map(str::to_owned);
                ext.update(|s| {
                    s.state = ExtensionState::Running as i32;
                    s.version = version;
                });
                log::info(format!("extension `{}` running (pid {})", x.name, pid.unwrap_or(0)));
            }
            Err(e) => {
                stop(&mut child);
                let _ = child.wait().await;
                return Ended::Failed(e);
            }
        },
        status = child.wait() => return Ended::Failed(exited(status)),
    }
    let beat = heartbeat();
    let mut tick = tokio::time::interval(beat);
    tick.tick().await;
    loop {
        tokio::select! {
            status = child.wait() => return Ended::Failed(exited(status)),
            _ = &mut serving => {
                return Ended::Failed(
                    settle(&mut child, &stop, "closed its standard output".into()).await,
                );
            }
            _ = tick.tick() => {
                if let Err(e) = ask(&calls, "$/ping", json!({}), beat).await {
                    return Ended::Failed(settle(&mut child, &stop, format!("stalled: {e}")).await);
                }
            }
            () = ext.wake.notified() => {
                // Asked to stop: say so, then signal, then kill.
                let _ = ask(&calls, "extension.shutdown", json!({}), SHUTDOWN).await;
                if let Some(pid) = pid {
                    signal(pid, "-TERM");
                }
                if tokio::time::timeout(TERM, child.wait()).await.is_err() {
                    stop(&mut child);
                    let _ = child.wait().await;
                }
                serving.abort();
                return Ended::Stopped;
            }
        }
    }
}

/// Why a run ended when its connection did: the exit, if the process is
/// exiting anyway (its pipe closes first), otherwise `why`, after killing
/// it.
async fn settle(child: &mut Child, stop: &impl Fn(&mut Child), why: String) -> String {
    if let Ok(status) = tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
        return exited(status);
    }
    stop(child);
    let _ = child.wait().await;
    why
}

fn exited(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(s) => format!("exited ({s})"),
        Err(e) => format!("could not wait for it: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_delays_grow_to_a_ceiling() {
        let base = backoff_base();
        let d1 = backoff(1);
        let d4 = backoff(4);
        assert!(
            d1 >= base.mul_f64(0.79) && d1 <= base.mul_f64(1.21),
            "{d1:?}"
        );
        assert!(d4 >= base.mul_f64(8.0 * 0.79), "{d4:?}");
        assert!(
            backoff(40) <= Duration::from_secs(73),
            "capped near a minute"
        );
    }
}
