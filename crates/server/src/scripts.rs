//! The `scripts` service: sandboxed Luau scripts run by the
//! daemon.
//!
//! Each script instance has its own VM on its own worker thread, never on
//! a terminal owner worker. Its `dreamer.*` calls go through an in-process
//! connection that holds the script's grant and the identity
//! `script:<name>`, so a script reaches exactly what its grant allows,
//! through the same checks as every other client.
//!
//! A budget overrun terminates the instance (its VM and connection go
//! away), records a `scripts.failed` event, and starts it again from its
//! last-good source after a growing delay; after `MAX_FAILURES` in a row
//! it is parked. A reload starts the new source first and swaps only once
//! it has loaded, so a bad update leaves the last-good version running.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{RpcError, ScriptState, ScriptStatus, code};
use script_runtime::{Host, HostError, HostFuture, Limits, Script, ScriptError};
use tokio::sync::{mpsc, oneshot, watch};

use crate::log;
use crate::service::State;

/// Budget overruns in a row before a script is parked.
pub const MAX_FAILURES: u32 = 8;

fn backoff_base() -> Duration {
    std::env::var("DREAMER_TEST_SCRIPT_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_millis(500), Duration::from_millis)
}

/// What a running instance tells its supervisor.
enum Report {
    Handled,
    HandlerError(String),
    /// The instance ended: over a budget, or its connection went away.
    Ended(String),
}

enum Control {
    Reload(String, oneshot::Sender<Result<ScriptStatus, RpcError>>),
    Stop(oneshot::Sender<ScriptStatus>),
}

struct Entry {
    name: String,
    grant: api::Grant,
    events: Vec<String>,
    limits: Limits,
    status: Mutex<ScriptStatus>,
    control: mpsc::Sender<Control>,
}

impl Entry {
    fn update(&self, f: impl FnOnce(&mut ScriptStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn status(&self) -> ScriptStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Scripts {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

impl std::fmt::Debug for Scripts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scripts").finish_non_exhaustive()
    }
}

fn limits_of(l: Option<&api::ScriptLimits>) -> Limits {
    let d = Limits::default();
    let Some(l) = l else { return d };
    Limits {
        memory_bytes: l
            .memory_bytes
            .map_or(d.memory_bytes, |b| (b as usize).clamp(1 << 20, 256 << 20)),
        call_time: l.call_ms.map_or(d.call_time, |ms| {
            Duration::from_millis(u64::from(ms).clamp(1, 10_000))
        }),
        host_call: l.host_call_ms.map_or(d.host_call, |ms| {
            Duration::from_millis(u64::from(ms).clamp(1, 60_000))
        }),
    }
}

impl Scripts {
    pub fn list(&self) -> api::ScriptList {
        let mut scripts: Vec<ScriptStatus> = self
            .entries
            .lock()
            .map(|e| e.values().map(|x| x.status()).collect())
            .unwrap_or_default();
        scripts.sort_by(|a, b| a.name.cmp(&b.name));
        api::ScriptList { scripts }
    }

    fn get(&self, name: &str) -> Result<Arc<Entry>, RpcError> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(name).cloned())
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no script `{name}`")))
    }

    pub async fn reload(&self, p: api::ScriptSource) -> Result<ScriptStatus, RpcError> {
        script_runtime::check(&p.name, &p.source)
            .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
        let entry = self.get(&p.name)?;
        let (tx, rx) = oneshot::channel();
        entry
            .control
            .send(Control::Reload(p.source, tx))
            .await
            .map_err(|_| RpcError::new(code::UNAVAILABLE, "the script is stopping"))?;
        rx.await
            .map_err(|_| RpcError::new(code::UNAVAILABLE, "the script is stopping"))?
    }

    pub async fn stop(&self, name: &str) -> Result<ScriptStatus, RpcError> {
        let entry = self.get(name)?;
        let (tx, rx) = oneshot::channel();
        let _ = entry.control.send(Control::Stop(tx)).await;
        let status = rx.await.unwrap_or_else(|_| entry.status());
        if let Ok(mut e) = self.entries.lock() {
            e.remove(name);
        }
        Ok(status)
    }

    pub async fn stop_all(&self) {
        let names: Vec<String> = self
            .entries
            .lock()
            .map(|e| e.keys().cloned().collect())
            .unwrap_or_default();
        for n in names {
            let _ = self.stop(&n).await;
        }
    }
}

pub async fn load(state: &Arc<State>, p: api::ScriptLoad) -> Result<ScriptStatus, RpcError> {
    let label = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    if p.name.is_empty() || !p.name.chars().all(label) {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "a script name is letters, digits, `-`, `_`, or `.`",
        ));
    }
    script_runtime::check(&p.name, &p.source)
        .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
    let g = p.grant.unwrap_or_default();
    let grant = api::Grant {
        owner: false,
        methods: g.methods,
        panes: g.panes,
        all_panes: g.all_panes,
        expires_unix_ms: None,
        level: 0,
    };
    if !p.events.is_empty()
        && !crate::grants::Grant(grant.clone()).method_allowed(api::method::EVENTS_SUBSCRIBE)
    {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            "a script that receives events needs events.subscribe in its grant",
        ));
    }
    if state
        .scripts
        .entries
        .lock()
        .is_ok_and(|e| e.contains_key(&p.name))
    {
        return Err(RpcError::new(
            code::CONFLICT,
            format!("a script named `{}` is loaded; reload or stop it", p.name),
        ));
    }
    let limits = limits_of(p.limits.as_ref());
    let (control, control_rx) = mpsc::channel(8);
    let entry = Arc::new(Entry {
        name: p.name.clone(),
        grant: grant.clone(),
        events: p.events.clone(),
        limits,
        status: Mutex::new(ScriptStatus {
            name: p.name.clone(),
            state: ScriptState::Running as i32,
            revision: 1,
            grant: Some(grant),
            events: p.events,
            ..ScriptStatus::default()
        }),
        control,
    });
    let running = start(state, &entry, p.source.clone())
        .await
        .map_err(|e| RpcError::new(code::PRECONDITION, format!("the script did not load: {e}")))?;
    {
        let mut entries = state
            .scripts
            .entries
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "scripts lock"))?;
        if entries.contains_key(&p.name) {
            return Err(RpcError::new(
                code::CONFLICT,
                format!("a script named `{}` is loaded", p.name),
            ));
        }
        entries.insert(p.name.clone(), entry.clone());
    }
    log::info(format!("scripts: `{}` loaded", p.name));
    tokio::spawn(supervise(
        state.clone(),
        entry.clone(),
        p.source,
        running,
        control_rx,
    ));
    Ok(entry.status())
}

/// A running instance: its stop switch and what it reports.
struct Running {
    stop: watch::Sender<bool>,
    reports: mpsc::UnboundedReceiver<Report>,
}

/// Start an instance of `source` and wait until its top level has run.
async fn start(state: &Arc<State>, e: &Entry, source: String) -> Result<Running, String> {
    let (inner, outer) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
    inner.set_nonblocking(true).map_err(|e| e.to_string())?;
    outer.set_nonblocking(true).map_err(|e| e.to_string())?;
    let inner = tokio::net::UnixStream::from_std(inner).map_err(|e| e.to_string())?;
    crate::conn::spawn_connection(
        inner,
        state.clone(),
        Some(std::process::id()),
        crate::grants::Grant(e.grant.clone()),
        Some(crate::messages::Identity {
            instance: format!("script:{}", e.name),
            app_id: Some(e.name.clone()),
            pane: None,
        }),
    );
    let (loaded_tx, loaded) = oneshot::channel();
    let (stop, stop_rx) = watch::channel(false);
    let (reports_tx, reports) = mpsc::unbounded_channel();
    let job = Job {
        name: e.name.clone(),
        source,
        events: e.events.clone(),
        limits: e.limits,
        state: state.clone(),
    };
    std::thread::Builder::new()
        .name(format!("script-{}", e.name))
        .spawn(move || worker(outer, job, loaded_tx, stop_rx, reports_tx))
        .map_err(|e| format!("could not start a worker thread: {e}"))?;
    match loaded.await {
        Ok(Ok(())) => Ok(Running { stop, reports }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the script worker ended while loading".into()),
    }
}

struct Job {
    name: String,
    source: String,
    events: Vec<String>,
    limits: Limits,
    state: Arc<State>,
}

/// The script's calls, made on its own connection.
struct ClientHost {
    client: Rc<client::Client>,
    state: Arc<State>,
    /// The filter whose `filters.matched` event `on_event` is handling.
    cause: Rc<RefCell<Option<String>>>,
}

/// The text a `terminal.input` request types, if it types text.
fn typed_text(r: &api::Request) -> Option<(u32, String)> {
    let api::Request::TerminalInput(i) = r else {
        return None;
    };
    match &i.event {
        Some(api::terminal_input::Event::Text(t)) => Some((i.pane, t.text.clone())),
        Some(api::terminal_input::Event::Paste(p)) => {
            Some((i.pane, String::from_utf8_lossy(&p.data).into_owned()))
        }
        _ => None,
    }
}

impl Host for ClientHost {
    fn call(&self, method: String, params: serde_json::Value) -> HostFuture {
        let c = self.client.clone();
        let cause = self.cause.borrow().clone();
        let state = self.state.clone();
        Box::pin(async move {
            let request = api::Request::from_json(&method, params).map_err(|e| HostError {
                code: match e {
                    api::CallError::UnknownMethod(_) => code::METHOD_NOT_FOUND,
                    _ => code::INVALID_PARAMS,
                },
                message: e.to_string(),
            })?;
            // Typing in answer to a filter's match is that filter's own
            // input, for its loop protection. Recorded before the input
            // goes out, so its echo can never come back first.
            if let Some(filter) = &cause
                && let Some((pane, text)) = typed_text(&request)
            {
                state.filters.caused_input(filter, pane, &text);
            }
            match c.call(request).await {
                Ok(r) => Ok(r.to_json()),
                Err(client::ClientError::Rpc(e)) => Err(HostError {
                    code: e.code,
                    message: e.message,
                }),
                Err(e) => Err(HostError {
                    code: code::UNAVAILABLE,
                    message: e.to_string(),
                }),
            }
        })
    }
}

/// One instance, on its own thread: a VM, a connection, and a loop
/// passing events to `on_event` until it is stopped or over a budget.
fn worker(
    stream: std::os::unix::net::UnixStream,
    job: Job,
    loaded: oneshot::Sender<Result<(), String>>,
    mut stop: watch::Receiver<bool>,
    reports: mpsc::UnboundedSender<Report>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = loaded.send(Err(e.to_string()));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let cause: Rc<RefCell<Option<String>>> = Rc::default();
        let setup = async {
            let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| e.to_string())?;
            let (r, w) = stream.into_split();
            let mut client = client::Client::over(r, w, "script", 1024)
                .await
                .map_err(|e| e.to_string())?;
            let pushed = client.pushed().ok_or("no event stream")?;
            let client = Rc::new(client);
            let script = Script::with_host(
                job.limits,
                Rc::new(ClientHost {
                    client: client.clone(),
                    state: job.state.clone(),
                    cause: cause.clone(),
                }),
            )
            .map_err(|e| e.to_string())?;
            script
                .run(&job.name, &job.source)
                .await
                .map_err(|e| e.to_string())?;
            if !job.events.is_empty() {
                client
                    .call(api::Request::EventsSubscribe(api::EventSubscribe {
                        kinds: job.events.clone(),
                        ..api::EventSubscribe::default()
                    }))
                    .await
                    .map_err(|e| format!("events.subscribe: {e}"))?;
            }
            Ok::<_, String>((client, script, pushed))
        };
        let (_client, script, mut pushed) = match setup.await {
            Ok(v) => {
                let _ = loaded.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = loaded.send(Err(e));
                return;
            }
        };
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                p = pushed.recv() => {
                    let Some(client::Pushed::Event { topic, payload, .. }) = p else {
                        let _ = reports.send(Report::Ended("its connection closed".into()));
                        return;
                    };
                    let Ok(api::Notification::EventsBatch(batch)) =
                        api::Notification::decode(&topic, &payload)
                    else {
                        continue;
                    };
                    let mut items: Vec<serde_json::Value> = batch
                        .events
                        .iter()
                        .filter_map(|e| serde_json::to_value(e).ok())
                        .collect();
                    if let Some(gap) = &batch.gap {
                        items.insert(0, serde_json::json!({
                            "kind": "events.gap",
                            "first": gap.first,
                            "last": gap.last,
                        }));
                    }
                    for e in items {
                        *cause.borrow_mut() = (e["kind"] == crate::events::kind::FILTER_MATCHED)
                            .then(|| e["subject"].as_str().map(str::to_owned))
                            .flatten();
                        let handled = script.handle("on_event", &e).await;
                        cause.borrow_mut().take();
                        match handled {
                            Ok(_) => {
                                let _ = reports.send(Report::Handled);
                            }
                            Err(ScriptError::Budget(b)) => {
                                let why = ScriptError::Budget(b).to_string();
                                let _ = reports.send(Report::Ended(why));
                                // Dropping the VM and the connection ends the instance.
                                return;
                            }
                            Err(e) => {
                                let _ = reports.send(Report::HandlerError(e.to_string()));
                            }
                        }
                    }
                }
            }
        }
    });
}

async fn supervise(
    state: Arc<State>,
    entry: Arc<Entry>,
    source: String,
    running: Running,
    mut control: mpsc::Receiver<Control>,
) {
    let name = entry.name.clone();
    let mut good = source;
    let mut running = Some(running);
    let mut retry: Option<tokio::time::Instant> = None;
    let mut failures = 0u32;
    loop {
        tokio::select! {
            r = async {
                match running.as_mut() {
                    Some(x) => x.reports.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match r {
                    Some(Report::Handled) => entry.update(|s| s.handled += 1),
                    Some(Report::HandlerError(e)) => entry.update(|s| {
                        s.errors += 1;
                        s.last_error = Some(e);
                    }),
                    ended => {
                        let why = match ended {
                            Some(Report::Ended(w)) => w,
                            _ => "the script worker ended".to_owned(),
                        };
                        running = None;
                        failures += 1;
                        retry = fail(&state, &entry, failures, &why);
                    }
                }
            }
            () = async {
                match retry {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                retry = None;
                match start(&state, &entry, good.clone()).await {
                    Ok(r) => {
                        running = Some(r);
                        entry.update(|s| {
                            s.restarts += 1;
                            s.state = ScriptState::Running as i32;
                        });
                    }
                    Err(why) => {
                        failures += 1;
                        retry = fail(&state, &entry, failures, &why);
                    }
                }
            }
            c = control.recv() => match c {
                Some(Control::Reload(source, reply)) => match start(&state, &entry, source.clone()).await {
                    Ok(r) => {
                        if let Some(old) = running.replace(r) {
                            let _ = old.stop.send(true);
                        }
                        retry = None;
                        failures = 0;
                        good = source;
                        entry.update(|s| {
                            s.revision += 1;
                            s.failures = 0;
                            s.state = ScriptState::Running as i32;
                        });
                        log::info(format!("scripts: `{name}` reloaded"));
                        let _ = reply.send(Ok(entry.status()));
                    }
                    Err(e) => {
                        // The running (last-good) version stays.
                        let _ = reply.send(Err(RpcError::new(
                            code::PRECONDITION,
                            format!("the new version did not load, so the last-good one keeps running: {e}"),
                        )));
                    }
                },
                Some(Control::Stop(reply)) => {
                    if let Some(r) = running.take() {
                        let _ = r.stop.send(true);
                    }
                    entry.update(|s| s.state = ScriptState::Stopped as i32);
                    log::info(format!("scripts: `{name}` stopped"));
                    let _ = reply.send(entry.status());
                    return;
                }
                None => return,
            },
        }
    }
}

/// Record a failure; when to try again, or `None` once parked.
fn fail(state: &State, entry: &Entry, failures: u32, why: &str) -> Option<tokio::time::Instant> {
    log::warn(format!("scripts: `{}`: {why}", entry.name));
    state.events.script_failed(&entry.name, why);
    entry.update(|s| {
        s.failures = failures;
        s.last_error = Some(why.to_owned());
    });
    if failures >= MAX_FAILURES {
        entry.update(|s| s.state = ScriptState::Failed as i32);
        log::warn(format!(
            "scripts: `{}` parked after {failures} failures in a row",
            entry.name
        ));
        return None;
    }
    entry.update(|s| s.state = ScriptState::Restarting as i32);
    Some(tokio::time::Instant::now() + crate::extensions::backoff_from(backoff_base(), failures))
}
