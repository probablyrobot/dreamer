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

use crate::manifest::{Manifest, Source, ViewKind, field, fill, fill_with, to_json};
use crate::model::{App, Event, Link};

/// What the presentation sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A key by name: `a`, `Enter`, `Ctrl-c`, `F2`, `Up`, `Space`, ...
    Key(String),
    /// Text for the focused input as one edit (committed by the platform's
    /// text input, or pasted); a view with no input ignores it.
    Text(String),
    /// A click: select this row, or focus this form field, by index. In a
    /// view tree, `node` names the node clicked (else the focused one).
    Select {
        node: Option<String>,
        index: usize,
    },
    /// Wheel steps; positive moves the selection down. In a view tree,
    /// `node` names the node under the pointer (else the focused one).
    Scroll {
        node: Option<String>,
        steps: i64,
    },
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

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn load(path: &str) -> Result<Manifest, String> {
    load_with_text(path).map(|(m, _)| m)
}

/// The manifest at `path`, and its text.
pub(crate) fn load_with_text(path: &str) -> Result<(Manifest, String), String> {
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

pub(crate) struct Conn {
    pub(crate) client: Arc<client::Client>,
    pub(crate) pushed: tokio::sync::mpsc::Receiver<client::Pushed>,
}

/// The launch ticket, when started by `dream app run`.
pub(crate) fn ticket() -> Option<(String, String)> {
    let id = std::env::var("DREAMER_LAUNCH_ID").ok()?;
    let path = std::env::var("DREAMER_LAUNCH_TICKET").ok()?;
    let t = std::fs::read_to_string(path).ok()?;
    Some((id, t.trim().to_owned()))
}

pub(crate) async fn connect(
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

/// The view's rows, through the module's `transform` when it has one; in
/// a tree, every data source shown.
async fn fetch(
    app: &mut App,
    c: &client::Client,
    script: Option<&script_runtime::Script>,
) -> Result<(), (bool, String)> {
    if app.manifest.is_tree() {
        for name in app.manifest.shown_sources() {
            let Some(src) = app.manifest.data.get(&name).cloned() else {
                continue;
            };
            let rows = rows_of(c, &src).await?;
            app.set_source(&name, rows, now_ms());
        }
        return Ok(());
    }
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
    if app.manifest.is_tree() {
        for name in app.manifest.shown_sources() {
            let src = app.manifest.data[&name].clone();
            let result = fixture(path, &name)?;
            app.set_source(&name, rows_in(result, &src), now_ms());
        }
        app.link = Link::Preview;
        return Ok(app);
    }
    let name = app.manifest.view.source.clone();
    if let Some(src) = app.manifest.data.get(&name).cloned() {
        let result = fixture(path, &name)?;
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

/// The fixture for data source `name`: `fixtures/<name>.json` beside the
/// manifest at `path`.
fn fixture(path: &Path, name: &str) -> Result<Value, String> {
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
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))
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
    if m.is_tree() {
        let mut events: Vec<String> = Vec::new();
        let mut refresh: Option<u64> = None;
        for s in m.shown_sources().iter().filter_map(|n| m.data.get(n)) {
            for e in &s.events {
                if !events.contains(e) {
                    events.push(e.clone());
                }
            }
            if let Some(r) = s.refresh_ms {
                refresh = Some(refresh.map_or(r, |x| x.min(r)));
            }
        }
        return (events, refresh.map(Duration::from_millis));
    }
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
/// An input a desktop sent through the daemon (`apps.input`).
pub(crate) fn from_remote(i: &api::AppInput) -> Option<Input> {
    if let Some(t) = &i.text {
        Some(Input::Text(t.clone()))
    } else if let Some(s) = i.select {
        Some(Input::Select {
            node: i.node.clone(),
            index: s as usize,
        })
    } else if let Some(s) = i.scroll {
        Some(Input::Scroll {
            node: i.node.clone(),
            steps: i64::from(s),
        })
    } else {
        (!i.key.is_empty()).then(|| Input::Key(i.key.clone()))
    }
}

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
pub(crate) fn host_views(client: Arc<client::Client>) -> tokio::sync::watch::Sender<String> {
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

/// Publish the newest view tree given to the returned sender as keyed
/// diffs (`apps.publish_tree`): the whole tree first, then the changes
/// from the last one sent. A daemon that lost track asks for the whole
/// tree again; one without the method ends the task. Ends when the
/// connection does.
pub(crate) fn host_trees(
    client: Arc<client::Client>,
) -> tokio::sync::watch::Sender<Option<view_tree::Frame>> {
    let (tx, mut rx) = tokio::sync::watch::channel(None::<view_tree::Frame>);
    tokio::spawn(async move {
        let mut out = view_tree::Outgoing::default();
        while rx.changed().await.is_ok() {
            let Some(frame) = rx.borrow_and_update().clone() else {
                continue;
            };
            // At most twice: the changes, then the whole tree if refused.
            for _ in 0..2 {
                let Some(update) = out.next(&frame) else {
                    break;
                };
                let p = match update {
                    view_tree::Update::Full { seq, frame } => api::AppTreePublish {
                        seq,
                        tree: serde_json::to_string(&frame).unwrap_or_default(),
                        patch: String::new(),
                    },
                    view_tree::Update::Patch { seq, ops } => api::AppTreePublish {
                        seq,
                        tree: String::new(),
                        patch: serde_json::to_string(&ops).unwrap_or_default(),
                    },
                };
                match client.call(api::Request::AppsPublishTree(p)).await {
                    Ok(_) => break,
                    Err(client::ClientError::Rpc(e)) if e.code == api::code::PRECONDITION => {
                        out.resync();
                    }
                    Err(client::ClientError::Rpc(e)) if e.code == api::code::METHOD_NOT_FOUND => {
                        return;
                    }
                    Err(client::ClientError::Rpc(_)) => {
                        out.resync();
                        break;
                    }
                    Err(_) => return,
                }
            }
        }
    });
    tx
}

/// Run the package's command `name`: a key's, a form's submit, or an
/// input component's `event`'s. A method gets its parameters filled from
/// the selected row, the form, and the input components' values; a
/// handler gets them as `selected`, `form`, `values`, and `event`. The
/// outcome goes to the status line. Returns whether to fetch again, and
/// why the connection is gone if it is.
async fn run_command(
    app: &mut App,
    pkg: &Package,
    conn: Option<&Conn>,
    name: &str,
    event: Option<&Event>,
) -> (bool, Option<String>) {
    let Some(cmd) = app.manifest.commands.get(name).cloned() else {
        return (false, None);
    };
    let label = cmd.label.clone().unwrap_or_else(|| name.to_owned());
    let Some(c) = conn else {
        app.status = Some(format!("{label}: not sent (disconnected)"));
        return (false, None);
    };
    let form_value = if app.in_form() {
        match app.form_json() {
            Ok(v) => Some(v),
            Err(e) => {
                app.status = Some(format!("{label}: {e}"));
                return (false, None);
            }
        }
    } else {
        None
    };
    let values = app.values_json();
    if let Some(h) = &cmd.handler {
        let Some(script) = pkg.script.as_ref() else {
            return (false, None);
        };
        let arg = serde_json::json!({
            "selected": app.selected_row().cloned(),
            "form": form_value,
            "values": values,
            "event": event,
        });
        app.status = Some(match script.handle(h, &arg).await {
            Ok(Some(Value::String(said))) => format!("{label}: {said}"),
            Ok(_) => format!("{label}: done"),
            Err(e) => format!("{label}: {e}"),
        });
        return (true, None);
    }
    let Some(method) = cmd.method.clone() else {
        return (false, None);
    };
    let params = match fill_with(
        &cmd.params,
        app.selected_row(),
        form_value.as_ref(),
        Some(&values),
    ) {
        Ok(p) => p,
        Err(e) => {
            app.status = Some(format!("{label}: {e}"));
            return (false, None);
        }
    };
    match call(&c.client, &method, params).await {
        Ok(_) => {
            app.status = Some(format!("{label}: done"));
            (true, None)
        }
        Err((gone, e)) => {
            app.status = Some(format!("{label}: {e}"));
            (false, gone.then_some(e))
        }
    }
}

/// What `input` does to `app` (not an agentic pane's keys): the commands
/// it runs, by name, with the component event that ran each (a key's, a
/// form's submit, or an input component's event's); none when it quits
/// the app.
fn take_input(app: &mut App, input: Input) -> Option<Vec<(String, Option<Event>)>> {
    let mut commands: Vec<(String, Option<Event>)> = Vec::new();
    match input {
        Input::Quit => return None,
        Input::Redraw => return Some(commands),
        Input::Text(text) => {
            app.insert_text(&text);
        }
        Input::Select { node, index } => {
            app.select_at(node.as_deref(), index);
        }
        Input::Scroll { node, steps } => {
            app.scroll(node.as_deref(), steps);
        }
        Input::Key(name) => {
            // A tree: Tab moves the focus, Left and Right switch tabs.
            if app.manifest.is_tree() {
                match name.as_str() {
                    "Tab" => {
                        app.focus_next();
                        return Some(commands);
                    }
                    "Left" | "Right" => {
                        app.switch_tab(if name == "Left" { -1 } else { 1 });
                        return Some(commands);
                    }
                    _ => {}
                }
            }
            let form = app.in_form();
            let typing = app.in_input();
            if app.control_key(&name) {
                // The component's events are handled below.
            } else if form
                && !matches!(name.as_str(), "Enter" | "Esc" | "Ctrl-c")
                && !name.starts_with('F')
            {
                app.form_key(if name == " " { "Space" } else { &name });
            } else if form && name == "Enter" {
                commands.extend(app.form_submit().map(|c| (c, None)));
            } else {
                match name.as_str() {
                    "Ctrl-c" => return None,
                    "q" if !form && !typing => return None,
                    "Esc" if form || typing => return None,
                    "Up" | "k" => app.move_by(-1),
                    "Down" | "j" => app.move_by(1),
                    "PageUp" => app.move_by(-10),
                    "PageDown" => app.move_by(10),
                    _ => commands.extend(
                        app.manifest
                            .commands
                            .iter()
                            .find(|(_, c)| !c.key.is_empty() && c.key == name)
                            .map(|(n, _)| (n.clone(), None)),
                    ),
                }
            }
        }
    }
    for e in app.take_events() {
        if let Some(c) = app.command_for(&e) {
            commands.push((c, Some(e)));
        }
    }
    Some(commands)
}

/// Replay `inputs` against the package at `path` as [`preview`] draws it,
/// from its fixtures and with no daemon: `each` gets the app as it first
/// looks and after every input. The commands an input runs are not run;
/// the status line names them. Input after the app quit is an error.
pub async fn replay(
    path: &Path,
    inputs: Vec<Input>,
    each: &mut dyn FnMut(&App) -> Result<(), String>,
) -> Result<(), String> {
    let mut app = preview(path).await?;
    each(&app)?;
    let mut quit = false;
    for (n, input) in inputs.into_iter().enumerate() {
        if quit {
            return Err(format!("event {}: the app had quit", n + 1));
        }
        if let Input::Key(name) = &input
            && app.manifest.view.kind == ViewKind::Agent
        {
            let name = name.clone();
            quit = matches!(crate::agent::key(&mut app, &name), crate::agent::Key::Quit);
        } else {
            match take_input(&mut app, input) {
                None => quit = true,
                Some(commands) if commands.is_empty() => {}
                Some(commands) => {
                    let names: Vec<String> = commands
                        .into_iter()
                        .map(|(c, _)| {
                            let label = app.manifest.commands.get(&c).and_then(|c| c.label.clone());
                            label.unwrap_or(c)
                        })
                        .collect();
                    app.status = Some(format!("{}: not run in a test", names.join(", ")));
                }
            }
        }
        each(&app)?;
    }
    Ok(())
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
    run_with(
        path,
        endpoint,
        inputs,
        publish,
        &crate::bound::Streams::default(),
    )
    .await
}

/// [`run`], following the streams the tree's bound nodes name into
/// `streams` (for the presentation to draw them); `publish` is called
/// again when they change.
pub async fn run_with(
    path: PathBuf,
    endpoint: Endpoint,
    inputs: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
    publish: &mut dyn FnMut(&App) -> Result<(), String>,
    streams: &crate::bound::Streams,
) -> Result<(), String> {
    let mut follower = crate::bound::Follower::new(None);
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
    let mut hosting_tree: Option<tokio::sync::watch::Sender<Option<view_tree::Frame>>> = None;
    let (remote_tx, mut remote) = tokio::sync::mpsc::unbounded_channel::<Input>();
    loop {
        if dirty {
            let frame = app.frame();
            // The streams the tree binds are followed before it is drawn.
            if let (Some(c), Some(f)) = (conn.as_ref(), frame.as_ref())
                && let Err(why) = follower.sync(&c.client, f, streams).await
            {
                conn = None;
                *current.borrow_mut() = None;
                follower.reset();
                app.link = Link::Stale(now_ms(), why);
                retry = tokio::time::Instant::now() + backoff;
            }
            publish(&app)?;
            if let Some(h) = &hosting {
                h.send_replace(app.to_published(&pkg.source));
            }
            if let Some(h) = &hosting_tree {
                h.send_replace(frame);
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
                let ev = match ev {
                    None | Some(Input::Quit) => return Ok(()),
                    Some(Input::Redraw) => continue,
                    Some(ev) => ev,
                };
                if let Input::Key(name) = &ev
                    && app.manifest.view.kind == ViewKind::Agent
                {
                    let name = name.clone();
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
                let Some(commands) = take_input(&mut app, ev) else {
                    return Ok(());
                };
                // What transcripts and composers asked: on the app's own
                // connection, under its grant.
                for ask in app.take_asks() {
                    let Some(c) = conn.as_ref() else {
                        app.status = Some("not sent (disconnected)".into());
                        break;
                    };
                    let client = c.client.clone();
                    if let Some(gone) = crate::bound::perform(&mut app, &client, streams, ask).await {
                        lost = Some(gone);
                        break;
                    }
                }
                for (name, event) in commands {
                    let (again, gone) =
                        run_command(&mut app, &pkg, conn.as_ref(), &name, event.as_ref()).await;
                    refetch |= again;
                    if gone.is_some() {
                        lost = gone;
                        break;
                    }
                }
            }
            p = pushed => match p {
                Some(client::Pushed::Event { topic, payload, .. }) => {
                    let n = api::Notification::decode(&topic, &payload);
                    if let Ok(n) = &n
                        && streams.take(n)
                    {
                        dirty = true;
                    }
                    if let Ok(api::Notification::AppsInput(i)) = &n
                        && let Some(input) = from_remote(i)
                    {
                        let _ = remote_tx.send(input);
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
                                hosting_tree = Some(host_trees(c.client.clone()));
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
            follower.reset();
            app.link = Link::Stale(now_ms(), why);
            retry = tokio::time::Instant::now() + backoff;
            dirty = true;
        }
    }
}
