//! `dreamer-termapp`: runs an app package's manifest as a terminal app in
//! a tmux pane (spec 13.3). It is the pane's real program, so the app
//! works through any plain tmux attach.
//!
//! ```text
//! dreamer-termapp MANIFEST       run it
//! dreamer-termapp --check FILE   validate only
//! ```
//!
//! A package may have a Luau module beside its manifest (sandboxed, with
//! the app's own grant) for `transform(rows)` and command handlers. The
//! manifest and module are reloaded when they change: a version that does
//! not validate or load leaves the running one in place, and says why.
//!
//! It talks to its host's daemon. Launched by `dream app run`, it first
//! registers with its single-use ticket and then holds that narrow grant.
//! When the daemon goes away it keeps showing the last data, marked
//! stale, and reconnects with backoff; an app that had registered does
//! not reconnect with more than it was granted, so it stays stale and
//! asks to be relaunched.

mod manifest;
mod view;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

use crate::manifest::{Manifest, field, fill, to_json};
use crate::view::{App, Link};

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version" | "-V"] => {
            println!("{}", version_line());
            ExitCode::SUCCESS
        }
        ["--check", path] => match load(path).and_then(|m| check_module(path, &m).map(|()| m)) {
            Ok(m) => {
                println!("ok: {} {} ({})", m.id, m.version, m.title);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        [path] => match load(path).and_then(|m| check_module(path, &m).map(|()| m)) {
            Ok(_) => match run(PathBuf::from(path)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dreamer-termapp: {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("usage: dreamer-termapp MANIFEST | --check MANIFEST | --version");
            ExitCode::from(2)
        }
    }
}

fn load(path: &str) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    Manifest::parse(&text).map_err(|errors| {
        errors
            .iter()
            .map(|e| format!("{path}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn module_path(manifest: &Path, m: &Manifest) -> Option<PathBuf> {
    m.module
        .as_ref()
        .map(|f| manifest.parent().unwrap_or(Path::new(".")).join(f))
}

/// The module compiles (it is not run here).
fn check_module(path: &str, m: &Manifest) -> Result<(), String> {
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
    script: Option<script_runtime::Script>,
}

async fn load_package(path: &Path, current: &Current) -> Result<Package, String> {
    let manifest = load(&path.display().to_string())?;
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
    Ok(Package { manifest, script })
}

/// When the package's files last changed.
fn stamps(path: &Path, m: &Manifest) -> Vec<Option<std::time::SystemTime>> {
    std::iter::once(path.to_path_buf())
        .chain(module_path(path, m))
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

/// Puts the terminal back however the app ends.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )
        .map_err(|e| e.to_string())?;
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn run(path: PathBuf) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let screen = Screen::enter()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .map_err(|e| e.to_string())?;
    // Keys and resizes, read on a thread of their own.
    let (input, mut events) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input.send(ev).is_err() {
                return;
            }
        }
    });
    let result = rt.block_on(event_loop(path, &mut terminal, &mut events));
    drop(screen);
    result
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

async fn connect(registered: &mut bool, events: &[String]) -> Result<Conn, String> {
    let mut c = tokio::time::timeout(
        Duration::from_secs(2),
        client::Client::connect(&platform::socket_path(), "termapp"),
    )
    .await
    .map_err(|_| "the daemon did not answer".to_owned())?
    .map_err(|e| e.to_string())?;
    if let Some((launch_id, ticket)) = ticket() {
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
async fn call(c: &client::Client, method: &str, params: Value) -> Result<Value, (bool, String)> {
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
    let result = call(
        c,
        &src.method,
        to_json(&toml::Value::Table(src.params.clone())),
    )
    .await?;
    let data = match &src.path {
        Some(p) => field(&result, p).cloned().unwrap_or(Value::Null),
        None => result,
    };
    let mut rows = match data {
        Value::Array(a) => a,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    rows.truncate(src.limit);
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

fn key_name(k: &KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Char(c) if ctrl => format!("Ctrl-{c}"),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    })
}

fn source_events(m: &Manifest) -> (Vec<String>, Option<Duration>) {
    let source = m.data.get(&m.view.source);
    (
        source.map(|s| s.events.clone()).unwrap_or_default(),
        source.and_then(|s| s.refresh_ms).map(Duration::from_millis),
    )
}

async fn event_loop<B: ratatui::backend::Backend>(
    path: PathBuf,
    terminal: &mut ratatui::Terminal<B>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
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
    loop {
        if dirty {
            terminal.draw(|f| app.draw(f)).map_err(|e| e.to_string())?;
            dirty = false;
        }
        let pushed = async {
            match conn.as_mut() {
                Some(c) => c.pushed.recv().await,
                None => std::future::pending().await,
            }
        };
        let mut lost: Option<String> = None;
        let mut refetch = false;
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else { return Ok(()) };
                dirty = true;
                let Event::Key(k) = ev else { continue };
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                let Some(name) = key_name(&k) else { continue };
                let form = app.manifest.view.kind == manifest::ViewKind::Form;
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
                    if let Ok(api::Notification::EventsBatch(b)) = api::Notification::decode(&topic, &payload)
                        && (b.gap.is_some() || !b.events.is_empty())
                    {
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
                    match connect(&mut registered, &watch).await {
                        Ok(c) => {
                            *current.borrow_mut() = Some(c.client.clone());
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

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dreamer-termapp "));
    }
}
