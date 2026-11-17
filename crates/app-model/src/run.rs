//! Running an app package against a daemon (spec 13.3): the connection,
//! data sources, events, commands, a Luau module, and hot reload. The
//! presentation only turns keys into [`Input`]s and draws what is
//! published.
//!
//! When the daemon goes away the app keeps the last data, marked stale,
//! and reconnects with backoff. An app that registered with a launch
//! ticket does not reconnect with more than it was granted, so it stays
//! stale and asks to be relaunched.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::manifest::{Manifest, Source, ViewKind, field, fill, to_json};
use crate::model::{App, Link};

/// What the presentation sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A key by name: `a`, `Enter`, `Ctrl-c`, `F2`, `Up`, `Space`, ...
    Key(String),
    /// Draw again (the presentation was resized or exposed).
    Redraw,
    Quit,
}

/// Which daemon the app talks to, and as what.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub socket: PathBuf,
    /// Shown by the daemon as the connection's role.
    pub role: &'static str,
    /// Register with the launch ticket from the environment, if one is
    /// there (a pane app started by `dream app run`).
    pub ticket: bool,
    /// Give up the connection's grant down to the package's declared
    /// `capabilities` (and following events), with every pane in scope.
    /// Used where the connection would otherwise be the user's own (a
    /// native panel), so a package can do no more than it declares. Read
    /// when connecting: a reload cannot widen it.
    pub narrow: bool,
}

impl Endpoint {
    /// This user's local daemon, as a pane app.
    pub fn pane_app() -> Endpoint {
        Endpoint {
            socket: platform::socket_path(),
            role: "termapp",
            ticket: true,
            narrow: false,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn load(path: &str) -> Result<Manifest, String> {
    load_with_text(path).map(|(m, _)| m)
}

/// The manifest at `path`, and its text.
fn load_with_text(path: &str) -> Result<(Manifest, String), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let m = Manifest::parse(&text).map_err(|errors| {
        errors
            .iter()
            .map(|e| format!("{path}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    Ok((m, text))
}

fn module_path(manifest: &Path, m: &Manifest) -> Option<PathBuf> {
    m.module
        .as_ref()
        .map(|f| manifest.parent().unwrap_or(Path::new(".")).join(f))
}

/// The module compiles (it is not run here).
pub fn check_module(path: &str, m: &Manifest) -> Result<(), String> {
    let Some(p) = module_path(Path::new(path), m) else {
        return Ok(());
    };
    let source = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    script_runtime::check(&p.display().to_string(), &source)
}

/// A module's `dreamer.*` calls go over the app's current connection.
type Current = Rc<RefCell<Option<Arc<client::Client>>>>;

struct ClientHost(Current);

impl script_runtime::Host for ClientHost {
    fn call(&self, method: String, params: Value) -> script_runtime::HostFuture {
        let c = self.0.borrow().clone();
        Box::pin(async move {
            let Some(c) = c else {
                return Err(script_runtime::HostError {
                    code: api::code::UNAVAILABLE,
                    message: "disconnected from the daemon".into(),
                });
            };
            call(&c, &method, params)
                .await
                .map_err(|(_, message)| script_runtime::HostError {
                    code: api::code::UNAVAILABLE,
                    message,
                })
        })
    }
}

/// A manifest with its module loaded and run.
struct Package {
    manifest: Manifest,
    /// The manifest's text, published with the model.
    source: String,
    script: Option<script_runtime::Script>,
}

async fn load_package(path: &Path, current: &Current) -> Result<Package, String> {
    let (manifest, source) = load_with_text(&path.display().to_string())?;
    let script = match module_path(path, &manifest) {
        None => None,
        Some(p) => {
            let source =
                std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let s = script_runtime::Script::with_host(
                script_runtime::Limits::default(),
                Rc::new(ClientHost(current.clone())),
            )
            .map_err(|e| e.to_string())?;
            s.run(&p.display().to_string(), &source)
                .await
                .map_err(|e| format!("{}: {e}", p.display()))?;
            Some(s)
        }
    };
    Ok(Package {
        manifest,
        source,
        script,
    })
}

/// When the package's files last changed.
fn stamps(path: &Path, m: &Manifest) -> Vec<Option<std::time::SystemTime>> {
    std::iter::once(path.to_path_buf())
        .chain(module_path(path, m))
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

struct Conn {
    client: Arc<client::Client>,
    pushed: tokio::sync::mpsc::Receiver<client::Pushed>,
}

/// The launch ticket, when started by `dream app run`.
fn ticket() -> Option<(String, String)> {
    let id = std::env::var("DREAMER_LAUNCH_ID").ok()?;
    let path = std::env::var("DREAMER_LAUNCH_TICKET").ok()?;
    let t = std::fs::read_to_string(path).ok()?;
    Some((id, t.trim().to_owned()))
}

async fn connect(
    endpoint: &Endpoint,
    registered: &mut bool,
    events: &[String],
    capabilities: &[String],
) -> Result<Conn, String> {
    let mut c = tokio::time::timeout(
        Duration::from_secs(2),
        client::Client::connect(&endpoint.socket, endpoint.role),
    )
    .await
    .map_err(|_| "the daemon did not answer".to_owned())?
    .map_err(|e| e.to_string())?;
    if let Some((launch_id, ticket)) = endpoint.ticket.then(ticket).flatten() {
        if *registered {
            return Err("the daemon restarted, so this app's grant ended; relaunch it".into());
        }
        c.call(api::Request::AppsRegister(api::RegisterApp {
            launch_id,
            ticket,
        }))
        .await
        .map_err(|e| format!("could not register: {e}"))?;
        *registered = true;
    }
    if endpoint.narrow {
        let mut methods = capabilities.to_vec();
        if !events.is_empty() {
            methods.extend(["events.subscribe".into(), "events.unsubscribe".into()]);
        }
        c.call(api::Request::GrantsNarrow(api::Grant {
            owner: false,
            methods,
            all_panes: true,
            ..api::Grant::default()
        }))
        .await
        .map_err(|e| format!("could not narrow the grant: {e}"))?;
    }
    let pushed = c.pushed().ok_or("no event stream")?;
    if !events.is_empty() {
        c.call(api::Request::EventsSubscribe(api::EventSubscribe {
            kinds: events.to_vec(),
            ..api::EventSubscribe::default()
        }))
        .await
        .map_err(|e| format!("events.subscribe: {e}"))?;
    }
    Ok(Conn {
        client: Arc::new(c),
        pushed,
    })
}

/// Call `method` with `params`; the result as JSON. `Err(true)` means the
/// connection is gone.
pub(crate) async fn call(
    c: &client::Client,
    method: &str,
    params: Value,
) -> Result<Value, (bool, String)> {
    let req = api::Request::from_json(method, params).map_err(|e| (false, e.to_string()))?;
    match tokio::time::timeout(Duration::from_secs(5), c.call(req)).await {
        Err(_) => Err((true, "no answer within 5 s".into())),
        Ok(Ok(r)) => Ok(r.to_json()),
        Ok(Err(client::ClientError::Rpc(e))) => Err((false, e.to_string())),
        Ok(Err(e)) => Err((true, e.to_string())),
    }
}

/// The view's rows, through the module's `transform` when it has one.
async fn fetch(
    app: &mut App,
    c: &client::Client,
    script: Option<&script_runtime::Script>,
) -> Result<(), (bool, String)> {
    let name = app.manifest.view.source.clone();
    let Some(src) = app.manifest.data.get(&name).cloned() else {
        return Ok(());
    };
    let mut rows = rows_of(c, &src).await?;
    if let Some(s) = script {
        match s.handle("transform", &Value::Array(rows.clone())).await {
            Ok(None) => {}
            Ok(Some(Value::Array(t))) => rows = t,
            Ok(Some(_)) => return Err((false, "transform must return a list of rows".into())),
            Err(e) => return Err((false, format!("transform: {e}"))),
        }
    }
    app.set_rows(rows, now_ms());
    Ok(())
}

/// A data source's rows: its method's result at its path, at most its
/// limit.
async fn rows_of(c: &client::Client, src: &Source) -> Result<Vec<Value>, (bool, String)> {
    let result = call(
        c,
        src.method_name(),
        to_json(&toml::Value::Table(src.call_params())),
    )
    .await?;
    Ok(rows_in(result, src))
}

/// The rows in a method's `result`, as `src` takes them: a collector's
/// rows with their fields as members.
fn rows_in(result: Value, src: &Source) -> Vec<Value> {
    let data = match src.rows_path() {
        Some(p) => field(&result, &p).cloned().unwrap_or(Value::Null),
        None => result,
    };
    let mut rows = match data {
        Value::Array(a) => a,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    rows.truncate(src.limit);
    if src.collector.is_some() {
        rows = rows
            .into_iter()
            .map(crate::manifest::collector_row)
            .collect();
    }
    rows
}

/// The app as it first looks, with no daemon: the view's data source is
/// read from `fixtures/<source>.json` beside the manifest (the method's
/// result as the daemon would return it), taken at the source's `path`,
/// and passed through the module's `transform`. A view with a source
/// needs its fixture; the error names the file.
pub async fn preview(path: &Path) -> Result<App, String> {
    let current: Current = Rc::default();
    let pkg = load_package(path, &current).await?;
    let mut app = App::new(pkg.manifest.clone());
    crate::agent::setup_preview(&mut app);
    let name = app.manifest.view.source.clone();
    if let Some(src) = app.manifest.data.get(&name).cloned() {
        let file = path
            .parent()
            .unwrap_or(Path::new("."))
            .join("fixtures")
            .join(format!("{name}.json"));
        let text = std::fs::read_to_string(&file).map_err(|e| {
            format!(
                "data source `{name}` needs a fixture to preview: {}: {e}",
                file.display()
            )
        })?;
        let result: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        let mut rows = rows_in(result, &src);
        if let Some(s) = &pkg.script {
            match s.handle("transform", &Value::Array(rows.clone())).await {
                Ok(None) => {}
                Ok(Some(Value::Array(t))) => rows = t,
                Ok(Some(_)) => return Err("transform must return a list of rows".into()),
                Err(e) => return Err(format!("transform: {e}")),
            }
        }
        app.set_rows(rows, now_ms());
    }
    app.link = Link::Preview;
    Ok(app)
}

/// One connection for a package's contributions (spec 12.2.1): its
/// commands and data sources, without a view. Narrowed like a panel's
/// when the endpoint says so.
#[derive(Debug)]
pub struct Session {
    client: client::Client,
}

impl Session {
    pub async fn open(endpoint: &Endpoint, manifest: &Manifest) -> Result<Session, String> {
        let conn = connect(endpoint, &mut false, &[], &manifest.methods()).await?;
        let client = Arc::try_unwrap(conn.client).map_err(|_| "connection shared".to_owned())?;
        Ok(Session { client })
    }

    /// Run the package's command `name` (a method with fixed parameters).
    /// `Err(true, ..)` means the connection is gone.
    pub async fn command(&self, manifest: &Manifest, name: &str) -> Result<(), (bool, String)> {
        let cmd = manifest
            .commands
            .get(name)
            .ok_or_else(|| (false, format!("no command `{name}`")))?;
        let method = cmd
            .method
            .as_deref()
            .ok_or_else(|| (false, format!("`{name}` has no method")))?;
        let params = fill(&cmd.params, None, None).map_err(|e| (false, e))?;
        call(&self.client, method, params).await.map(|_| ())
    }

    /// The rows of the package's data source `source`.
    pub async fn source(
        &self,
        manifest: &Manifest,
        source: &str,
    ) -> Result<Vec<Value>, (bool, String)> {
        let src = manifest
            .data
            .get(source)
            .ok_or_else(|| (false, format!("no data source `{source}`")))?;
        rows_of(&self.client, src).await
    }
}

fn source_events(m: &Manifest) -> (Vec<String>, Option<Duration>) {
    let source = m.data.get(&m.view.source);
    let mut events = source.map(|s| s.events.clone()).unwrap_or_default();
    // A filter pane shows what its filters match.
    if m.agent
        .as_ref()
        .is_some_and(|a| a.kind == crate::manifest::AgentKind::Filter)
    {
        events.extend(["filters.matched".into(), "filters.tripped".into()]);
    }
    (
        events,
        source.and_then(|s| s.refresh_ms).map(Duration::from_millis),
    )
}

/// The next input: the presentation's, or a key a desktop sent.
async fn next_input(
    inputs: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
    remote: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
) -> Option<Input> {
    tokio::select! {
        i = inputs.recv() => i,
        Some(i) = remote.recv() => Some(i),
    }
}

/// Publish the newest model given to the returned sender, one at a time:
/// a model superseded while one is being sent is skipped. Ends when the
/// connection does.
fn host_views(client: Arc<client::Client>) -> tokio::sync::watch::Sender<String> {
    let (tx, mut rx) = tokio::sync::watch::channel(String::new());
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let view = rx.borrow_and_update().clone();
            let sent = client
                .call(api::Request::AppsPublish(api::AppViewPublish { view }))
                .await;
            if let Err(client::ClientError::Rpc(_)) | Ok(_) = sent {
                continue;
            }
            return;
        }
    });
    tx
}

/// Run the package at `path` against the daemon at `endpoint`: connect,
/// fetch, follow events, run commands for keys from `inputs`, and reload
/// the package when its files change. `publish` gets the model whenever
/// it changed, to draw it. Returns when the app quits (`q`, Esc in a form,
/// Ctrl-c, or `Input::Quit`) or `inputs` closes.
pub async fn run(
    path: PathBuf,
    endpoint: Endpoint,
    inputs: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
    publish: &mut dyn FnMut(&App) -> Result<(), String>,
) -> Result<(), String> {
    let current: Current = Rc::default();
    let mut pkg = load_package(&path, &current).await?;
    let mut stamp = stamps(&path, &pkg.manifest);
    let (mut watch, mut refresh) = source_events(&pkg.manifest);
    let mut app = App::new(pkg.manifest.clone());
    let mut ticks = 0u32;
    let mut conn: Option<Conn> = None;
    let mut registered = false;
    let mut gave_up = false;
    let mut retry = tokio::time::Instant::now();
    let mut backoff = Duration::from_millis(500);
    let mut next_fetch: Option<tokio::time::Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut dirty = true;
    // An agentic pane's provider, chosen once; and its run in progress.
    crate::agent::setup(&mut app).await;
    let mut agent_run: Option<providers::Run> = None;
    // A registered pane app is its pane's host: it publishes its model to
    // the daemon, and keys typed in desktops come back as events.
    let mut hosting: Option<tokio::sync::watch::Sender<String>> = None;
    let (remote_tx, mut remote) = tokio::sync::mpsc::unbounded_channel::<Input>();
    loop {
        if dirty {
            publish(&app)?;
            if let Some(h) = &hosting {
                h.send_replace(app.to_published(&pkg.source));
            }
            dirty = false;
        }
        let pushed = async {
            match conn.as_mut() {
                Some(c) => c.pushed.recv().await,
                None => std::future::pending().await,
            }
        };
        let agent_events = async {
            match agent_run.as_mut() {
                Some(r) => r.events.recv().await,
                None => std::future::pending().await,
            }
        };
        let mut lost: Option<String> = None;
        let mut refetch = false;
        tokio::select! {
            e = agent_events => {
                dirty = true;
                let ended = match e {
                    Some(e) => crate::agent::apply(&mut app, e),
                    None => {
                        app.agent.running = false;
                        true
                    }
                };
                if ended {
                    agent_run = None;
                    // A filter pane's model may have changed its filters.
                    if let Some(c) = conn.as_ref() {
                        let client = c.client.clone();
                        crate::agent::refresh_filters(&mut app, &client).await;
                    }
                }
            }
            ev = next_input(inputs, &mut remote) => {
                dirty = true;
                let name = match ev {
                    None | Some(Input::Quit) => return Ok(()),
                    Some(Input::Redraw) => continue,
                    Some(Input::Key(name)) => name,
                };
                if app.manifest.view.kind == ViewKind::Agent {
                    match crate::agent::key(&mut app, &name) {
                        crate::agent::Key::Handled => {}
                        crate::agent::Key::Quit => return Ok(()),
                        crate::agent::Key::Cancel => {
                            if let Some(r) = &agent_run {
                                r.cancel();
                                // What the operator started stops with it.
                                if let Some(c) = conn.as_ref() {
                                    let client = c.client.clone();
                                    crate::agent::cancel_jobs(&mut app, &client).await;
                                }
                            }
                        }
                        crate::agent::Key::Submit(prompt) => {
                            app.agent.say(crate::model::Said::You, &prompt);
                            app.agent.plan = None;
                            let spec = app.manifest.agent.clone();
                            match (spec, app.agent.provider.clone()) {
                                (Some(spec), Some(p)) => {
                                    let kind = providers::Kind::parse(&p).unwrap_or(providers::Kind::ClaudeCli);
                                    let req = crate::agent::request(&spec, &prompt, &endpoint.socket);
                                    match providers::start(kind, &req) {
                                        Ok(r) => {
                                            app.agent.running = true;
                                            agent_run = Some(r);
                                        }
                                        Err(e) => app.agent.say(crate::model::Said::Failed, &e.to_string()),
                                    }
                                }
                                _ => match conn.as_ref() {
                                    Some(c) => {
                                        let client = c.client.clone();
                                        crate::agent::scripted(&mut app, &client, &prompt).await;
                                    }
                                    None => app.agent.say(crate::model::Said::Failed, "not connected to the daemon"),
                                },
                            }
                        }
                    }
                    continue;
                }
                let form = app.manifest.view.kind == ViewKind::Form;
                if form && !matches!(name.as_str(), "Enter" | "Esc" | "Ctrl-c")
                    && !name.starts_with('F')
                {
                    app.form_key(if name == " " { "Space" } else { &name });
                    continue;
                }
                let name = if form && name == "Enter" {
                    app.manifest
                        .view
                        .submit
                        .as_ref()
                        .and_then(|c| app.manifest.commands.get(c))
                        .map_or(name, |c| c.key.clone())
                } else {
                    name
                };
                match name.as_str() {
                    "Ctrl-c" => return Ok(()),
                    "q" if !form => return Ok(()),
                    "Esc" if form => return Ok(()),
                    "Up" | "k" => app.move_by(-1),
                    "Down" | "j" => app.move_by(1),
                    "PageUp" => app.move_by(-10),
                    "PageDown" => app.move_by(10),
                    _ => {
                        let Some((label, cmd)) = app
                            .manifest
                            .commands
                            .iter()
                            .find(|(_, c)| c.key == name)
                            .map(|(n, c)| (c.label.clone().unwrap_or_else(|| n.clone()), c.clone()))
                        else {
                            continue;
                        };
                        let Some(c) = conn.as_ref() else {
                            app.status = Some(format!("{label}: not sent (disconnected)"));
                            continue;
                        };
                        let form_value = if form {
                            match app.form_json() {
                                Ok(v) => Some(v),
                                Err(e) => {
                                    app.status = Some(format!("{label}: {e}"));
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        if let Some(h) = &cmd.handler {
                            let Some(script) = pkg.script.as_ref() else { continue };
                            let arg = serde_json::json!({
                                "selected": app.selected_row().cloned(),
                                "form": form_value,
                            });
                            app.status = Some(match script.handle(h, &arg).await {
                                Ok(Some(Value::String(said))) => format!("{label}: {said}"),
                                Ok(_) => format!("{label}: done"),
                                Err(e) => format!("{label}: {e}"),
                            });
                            refetch = true;
                        } else if let Some(method) = cmd.method.clone() {
                            let params = match fill(&cmd.params, app.selected_row(), form_value.as_ref()) {
                                Ok(p) => p,
                                Err(e) => {
                                    app.status = Some(format!("{label}: {e}"));
                                    continue;
                                }
                            };
                            match call(&c.client, &method, params).await {
                                Ok(_) => {
                                    app.status = Some(format!("{label}: done"));
                                    refetch = true;
                                }
                                Err((gone, e)) => {
                                    app.status = Some(format!("{label}: {e}"));
                                    if gone {
                                        lost = Some(e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            p = pushed => match p {
                Some(client::Pushed::Event { topic, payload, .. }) => {
                    let n = api::Notification::decode(&topic, &payload);
                    if let Ok(api::Notification::AppsInput(i)) = &n {
                        let _ = remote_tx.send(Input::Key(i.key.clone()));
                    }
                    if let Ok(api::Notification::EventsBatch(b)) = n
                        && (b.gap.is_some() || !b.events.is_empty())
                    {
                        if crate::agent::filter_events(&mut app, &b.events) {
                            dirty = true;
                        }
                        refetch = true;
                    }
                }
                Some(client::Pushed::Closed { error }) => {
                    lost = Some(error.unwrap_or_else(|| "the daemon closed the connection".into()));
                }
                Some(_) => {}
                None => lost = Some("the daemon closed the connection".into()),
            },
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                ticks += 1;
                // An operator's actions and jobs, read again until they
                // settle, once a second.
                if ticks.is_multiple_of(5)
                    && app.agent.taken.iter().any(|t| t.open)
                    && let Some(c) = conn.as_ref()
                {
                    let client = c.client.clone();
                    if crate::agent::refresh(&mut app, &client).await {
                        dirty = true;
                    }
                }
                // Look for a new version of the package twice a second.
                if ticks.is_multiple_of(3) {
                    let now_stamp = stamps(&path, &pkg.manifest);
                    if now_stamp != stamp {
                        stamp = now_stamp;
                        match load_package(&path, &current).await {
                            Ok(new) => {
                                let values = app.form_values();
                                let (new_watch, new_refresh) = source_events(&new.manifest);
                                if let Some(c) = conn.as_ref()
                                    && new_watch != watch
                                    && !new_watch.is_empty()
                                {
                                    let _ = c
                                        .client
                                        .call(api::Request::EventsSubscribe(api::EventSubscribe {
                                            kinds: new_watch.clone(),
                                            ..api::EventSubscribe::default()
                                        }))
                                        .await;
                                }
                                (watch, refresh) = (new_watch, new_refresh);
                                app.manifest = new.manifest.clone();
                                app.reset_form(&values);
                                app.version += 1;
                                pkg = new;
                                stamp = stamps(&path, &pkg.manifest);
                                app.status = Some(format!("reloaded (version {})", app.version));
                                refetch = true;
                            }
                            Err(e) => {
                                app.status = Some(format!(
                                    "reload failed; version {} keeps running: {e}",
                                    app.version
                                ));
                            }
                        }
                        dirty = true;
                    }
                }
                if conn.is_none() && !gave_up && now >= retry {
                    match connect(&endpoint, &mut registered, &watch, &app.manifest.methods()).await {
                        Ok(c) => {
                            *current.borrow_mut() = Some(c.client.clone());
                            if registered && hosting.is_none() {
                                hosting = Some(host_views(c.client.clone()));
                            }
                            crate::agent::refresh_filters(&mut app, &c.client).await;
                            conn = Some(c);
                            app.link = Link::Live;
                            backoff = Duration::from_millis(500);
                            refetch = true;
                            dirty = true;
                        }
                        Err(e) => {
                            gave_up = registered;
                            let since = match &app.link {
                                Link::Stale(t, _) => *t,
                                _ => now_ms(),
                            };
                            app.link = Link::Stale(since, e);
                            retry = now + backoff;
                            backoff = (backoff * 2).min(Duration::from_secs(5));
                            dirty = true;
                        }
                    }
                }
                if conn.is_some() && next_fetch.is_some_and(|t| now >= t) {
                    refetch = true;
                }
            }
        }
        if refetch && let Some(c) = conn.as_ref() {
            let client = c.client.clone();
            match fetch(&mut app, &client, pkg.script.as_ref()).await {
                Ok(()) => {
                    if app
                        .status
                        .as_deref()
                        .is_some_and(|s| s.starts_with("fetch: "))
                    {
                        app.status = None;
                    }
                }
                Err((true, e)) => lost = Some(e),
                Err((false, e)) => app.status = Some(format!("fetch: {e}")),
            }
            next_fetch = refresh.map(|r| tokio::time::Instant::now() + r);
            dirty = true;
        }
        if let Some(why) = lost {
            conn = None;
            *current.borrow_mut() = None;
            app.link = Link::Stale(now_ms(), why);
            retry = tokio::time::Instant::now() + backoff;
            dirty = true;
        }
    }
}
