//! Filters (spec 12.8): a text tap, the Rust match stage, and outputs.
//!
//! Each filter is its own in-process connection with the filter's grant
//! and the identity `filter:<name>`: its tap, its messages, and its input
//! go through the same checks as any client's. A filter only reads; it
//! never changes what a program wrote, the mirror, or what a tmux client
//! sees.
//!
//! Loop protection (spec 12.3):
//! - matches acted on are rate-limited per filter;
//! - lines that echo input the filter itself typed are skipped;
//! - a circuit breaker trips (pauses) a filter whose triggers come faster
//!   than its breaker allows in any 10 s, and records why.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::{FilterState, FilterStatus, RpcError, code};
use matcher::{Matcher, Spec};
use tokio::sync::{oneshot, watch};

use crate::log;
use crate::service::State;

const WINDOW: Duration = Duration::from_secs(10);
/// How long typed input counts as the filter's own when echoed back.
const OWN_INPUT: Duration = Duration::from_secs(5);
const DEFAULT_PER_SECOND: u32 = 20;
const DEFAULT_BREAKER: u32 = 50;

struct Filter {
    spec: api::FilterSpec,
    matcher: Matcher,
    grant: api::Grant,
    status: Mutex<FilterStatus>,
    /// Running (true) or paused, tripped, or removed (false).
    active: watch::Sender<bool>,
    removed: std::sync::atomic::AtomicBool,
    /// Installed by the user's own client, not by an app with
    /// `filters.manage`; no app may change it.
    by_owner: bool,
}

impl Filter {
    fn update(&self, f: impl FnOnce(&mut FilterStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn status(&self) -> FilterStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Filters {
    entries: Mutex<HashMap<String, Arc<Filter>>>,
}

impl std::fmt::Debug for Filters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filters").finish_non_exhaustive()
    }
}

fn invalid(why: impl Into<String>) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, why)
}

impl Filters {
    pub fn list(&self) -> api::FilterList {
        let mut filters: Vec<FilterStatus> = self
            .entries
            .lock()
            .map(|e| e.values().map(|f| f.status()).collect())
            .unwrap_or_default();
        filters.sort_by(|a, b| a.name.cmp(&b.name));
        api::FilterList { filters }
    }

    /// Whether an app holding `allowed` (the most an app's filter may be
    /// given: `taps.subscribe` on the app's own panes) may pause, resume,
    /// or remove the filter `name`: one an app installed, within that. An
    /// unknown name is left to the method, which reports it.
    pub fn app_may_manage(&self, name: &str, allowed: &api::Grant) -> Result<(), String> {
        let Ok(f) = self.get(name) else {
            return Ok(());
        };
        if f.by_owner {
            return Err(format!("filter `{name}` is the user's own"));
        }
        crate::grants::within(allowed, &f.grant)
            .map_err(|why| format!("filter `{name}` reaches beyond this app: {why}"))
    }

    /// Only the filters an app may manage, for an app's `filters.list`.
    pub fn list_for_app(&self, allowed: &api::Grant) -> api::FilterList {
        let mut l = self.list();
        l.filters
            .retain(|f| self.app_may_manage(&f.name, allowed).is_ok());
        l
    }

    fn get(&self, name: &str) -> Result<Arc<Filter>, RpcError> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(name).cloned())
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no filter `{name}`")))
    }

    pub fn pause(&self, name: &str) -> Result<FilterStatus, RpcError> {
        let f = self.get(name)?;
        f.update(|s| {
            s.state = FilterState::Paused as i32;
            s.reason = Some("paused".into());
        });
        f.active.send_replace(false);
        Ok(f.status())
    }

    pub fn resume(&self, name: &str) -> Result<FilterStatus, RpcError> {
        let f = self.get(name)?;
        f.update(|s| {
            s.state = FilterState::Running as i32;
            s.reason = None;
        });
        f.active.send_replace(true);
        Ok(f.status())
    }

    pub fn remove(&self, name: &str) -> Result<FilterStatus, RpcError> {
        let f = self.get(name)?;
        f.removed.store(true, std::sync::atomic::Ordering::SeqCst);
        f.active.send_replace(false);
        if let Ok(mut e) = self.entries.lock() {
            e.remove(name);
        }
        Ok(f.status())
    }

    pub fn remove_all(&self) {
        let names: Vec<String> = self
            .entries
            .lock()
            .map(|e| e.keys().cloned().collect())
            .unwrap_or_default();
        for n in names {
            let _ = self.remove(&n);
        }
    }
}

pub async fn install(state: &Arc<State>, spec: api::FilterSpec) -> Result<FilterStatus, RpcError> {
    let label = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    if spec.name.is_empty() || !spec.name.chars().all(label) {
        return Err(invalid(
            "a filter name is letters, digits, `-`, `_`, or `.`",
        ));
    }
    if spec.panes.is_empty() && !spec.every_pane {
        return Err(invalid("give the panes to filter, or every_pane"));
    }
    let matcher = Matcher::new(&Spec {
        pattern: spec.pattern.clone(),
        literals: spec.literals.clone(),
        literal: spec.literal,
        ignore_case: spec.ignore_case,
    })
    .map_err(invalid)?;
    let g = spec.grant.clone().unwrap_or_default();
    let grant = api::Grant {
        owner: false,
        methods: g.methods,
        panes: g.panes,
        all_panes: g.all_panes,
        expires_unix_ms: None,
        level: 0,
    };
    let checker = crate::grants::Grant(grant.clone());
    let need = |method: &str, why: &str| {
        if checker.method_allowed(method) {
            Ok(())
        } else {
            Err(invalid(format!(
                "{why} needs {method} in the filter's grant"
            )))
        }
    };
    need(api::method::TAPS_SUBSCRIBE, "reading panes")?;
    if spec.topic.is_some() || spec.recipient.is_some() {
        need(api::method::MESSAGES_PUBLISH, "publishing matches")?;
    }
    if let Some(i) = &spec.input {
        need(api::method::TERMINAL_INPUT, "typing into a pane")?;
        if !grant.all_panes && !grant.panes.contains(&i.pane) {
            return Err(invalid(format!(
                "pane %{} is outside the filter's grant",
                i.pane
            )));
        }
    }
    if state
        .filters
        .entries
        .lock()
        .is_ok_and(|e| e.contains_key(&spec.name))
    {
        return Err(RpcError::new(
            code::CONFLICT,
            format!("a filter named `{}` is installed", spec.name),
        ));
    }
    let (active, _) = watch::channel(true);
    let filter = Arc::new(Filter {
        status: Mutex::new(FilterStatus {
            name: spec.name.clone(),
            state: FilterState::Running as i32,
            ..FilterStatus::default()
        }),
        spec,
        matcher,
        grant,
        active,
        removed: std::sync::atomic::AtomicBool::new(false),
        by_owner: crate::leases::caller().is_none_or(|c| c.owner),
    });
    let (started, ready) = oneshot::channel();
    tokio::spawn(supervise(state.clone(), filter.clone(), started));
    match ready.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(RpcError::new(code::PRECONDITION, e)),
        Err(_) => return Err(RpcError::new(code::INTERNAL, "the filter stopped")),
    }
    let name = filter.spec.name.clone();
    {
        let mut entries = state
            .filters
            .entries
            .lock()
            .map_err(|_| RpcError::new(code::INTERNAL, "filters lock"))?;
        entries.insert(name.clone(), filter.clone());
    }
    log::info(format!("filters: `{name}` installed"));
    Ok(filter.status())
}

async fn supervise(
    state: Arc<State>,
    f: Arc<Filter>,
    started: oneshot::Sender<Result<(), String>>,
) {
    let mut started = Some(started);
    let mut active = f.active.subscribe();
    loop {
        if f.removed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if !*active.borrow() {
            if active.changed().await.is_err() {
                return;
            }
            continue;
        }
        let r = run(&state, &f, &mut active, &mut started).await;
        if let Some(s) = started.take() {
            let _ = s.send(r.clone());
            if r.is_err() {
                f.removed.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        }
        if let Err(e) = r {
            log::warn(format!("filters: `{}`: {e}", f.spec.name));
            f.update(|s| s.last_error = Some(e));
            // The tap ended (a pane went away, the connection closed):
            // look again shortly.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// One run: connect, tap, and act on matches until paused, tripped, or
/// removed (`Ok`), or until the tap ends (`Err`).
async fn run(
    state: &Arc<State>,
    f: &Arc<Filter>,
    active: &mut watch::Receiver<bool>,
    started: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    let (inner, outer) = tokio::net::UnixStream::pair().map_err(|e| e.to_string())?;
    crate::conn::spawn_connection(
        inner,
        state.clone(),
        Some(std::process::id()),
        crate::grants::Grant(f.grant.clone()),
        Some(crate::messages::Identity {
            instance: format!("filter:{}", f.spec.name),
            app_id: Some(f.spec.name.clone()),
            pane: None,
        }),
    );
    let (r, w) = outer.into_split();
    let mut client = client::Client::over(r, w, "filter", 1024)
        .await
        .map_err(|e| e.to_string())?;
    let mut pushed = client.pushed().ok_or("no event stream")?;
    client
        .call(api::Request::TapsSubscribe(api::TapSubscribe {
            level: api::TapLevel::Text as i32,
            panes: f.spec.panes.clone(),
            every_pane: f.spec.every_pane,
            interval_ms: Some(100),
        }))
        .await
        .map_err(|e| format!("taps.subscribe: {e}"))?;
    if let Some(s) = started.take() {
        let _ = s.send(Ok(()));
    }
    let per_second = f.spec.per_second.unwrap_or(DEFAULT_PER_SECOND).max(1);
    let breaker = f.spec.breaker.unwrap_or(DEFAULT_BREAKER).max(1) as usize;
    let mut tokens = f64::from(per_second);
    let mut refilled = Instant::now();
    let mut triggers: VecDeque<Instant> = VecDeque::new();
    let mut own: Vec<(u32, String, Instant)> = Vec::new();
    loop {
        let item = tokio::select! {
            changed = active.changed() => {
                if changed.is_err() || !*active.borrow() {
                    return Ok(());
                }
                continue;
            }
            item = pushed.recv() => item,
        };
        let Some(client::Pushed::Event { topic, payload, .. }) = item else {
            return Err("its tap connection ended".into());
        };
        let Ok(api::Notification::TapsData(d)) = api::Notification::decode(&topic, &payload) else {
            continue;
        };
        if d.ended {
            if !f.spec.every_pane && f.spec.panes.len() == 1 {
                return Err(format!("pane %{} is gone", d.pane));
            }
            continue;
        }
        for (index, line) in d.lines.iter().enumerate() {
            let now = Instant::now();
            own.retain(|(_, _, at)| now.duration_since(*at) < OWN_INPUT);
            if own.iter().any(|(pane, text, _)| {
                *pane == d.pane && !text.is_empty() && line.contains(text.as_str())
            }) {
                f.update(|s| s.skipped_own += 1);
                continue;
            }
            let Some(_) = f.matcher.find(line) else {
                continue;
            };
            f.update(|s| s.matched += 1);
            if f.spec.decorate != api::DecorationStyle::Unspecified as i32
                && let Some(&row) = d.rows.get(index)
            {
                state.decorations.add(api::Decoration {
                    pane: d.pane,
                    generation: d.generation,
                    row,
                    text: line.clone(),
                    style: f.spec.decorate,
                    badge: f.spec.badge.clone(),
                    filter: f.spec.name.clone(),
                    cols: d.cols,
                    ..api::Decoration::default()
                });
                state.events.decorations_changed(d.pane);
            }
            tokens = (tokens + now.duration_since(refilled).as_secs_f64() * f64::from(per_second))
                .min(f64::from(per_second));
            refilled = now;
            if tokens < 1.0 {
                f.update(|s| s.dropped += 1);
                continue;
            }
            tokens -= 1.0;
            let fired = act(state, f, &client, d.pane, line, &mut own).await;
            if fired == 0 {
                continue;
            }
            f.update(|s| s.triggered += fired as u64);
            for _ in 0..fired {
                triggers.push_back(now);
            }
            while triggers
                .front()
                .is_some_and(|t| now.duration_since(*t) > WINDOW)
            {
                triggers.pop_front();
            }
            if triggers.len() > breaker {
                let why = format!(
                    "tripped: {} triggers within {} s (more than {breaker}); a loop?",
                    triggers.len(),
                    WINDOW.as_secs()
                );
                log::warn(format!("filters: `{}` {why}", f.spec.name));
                state.events.filter_tripped(&f.spec.name, &why);
                f.update(|s| {
                    s.state = FilterState::Tripped as i32;
                    s.reason = Some(why);
                });
                f.active.send_replace(false);
                return Ok(());
            }
        }
    }
}

/// Act on one matched line; the number of triggers fired.
async fn act(
    state: &Arc<State>,
    f: &Arc<Filter>,
    client: &client::Client,
    pane: u32,
    line: &str,
    own: &mut Vec<(u32, String, Instant)>,
) -> usize {
    let mut fired = 0;
    let fail = |e: client::ClientError| f.update(|s| s.last_error = Some(e.to_string()));
    for (topic, recipient) in [
        (f.spec.topic.clone(), None),
        (None, f.spec.recipient.clone()),
    ] {
        if topic.is_none() && recipient.is_none() {
            continue;
        }
        let r = client
            .call(api::Request::MessagesPublish(api::MessagePublish {
                topic,
                recipient,
                payload: line.as_bytes().to_vec(),
                ..api::MessagePublish::default()
            }))
            .await;
        match r {
            Ok(_) => fired += 1,
            Err(e) => fail(e),
        }
    }
    if f.spec.event {
        state.events.filter_matched(&f.spec.name, pane, line);
    }
    if let Some(i) = &f.spec.input {
        let r = client
            .call(api::Request::TerminalInput(api::TerminalInput {
                pane: i.pane,
                event: Some(api::terminal_input::Event::Text(api::TextInput {
                    text: i.text.clone(),
                })),
                ..api::TerminalInput::default()
            }))
            .await;
        match r {
            Ok(_) => {
                fired += 1;
                own.push((i.pane, i.text.trim().to_owned(), Instant::now()));
            }
            Err(e) => fail(e),
        }
    }
    fired
}
