//! `dreamer-termapp`: runs an app package's manifest as a terminal app in
//! a tmux pane (spec 13.3). It is the pane's real program, so the app
//! works through any plain tmux attach.
//!
//! ```text
//! dreamer-termapp MANIFEST       run it
//! dreamer-termapp --check FILE   validate only
//! ```
//!
//! It talks to its host's daemon. Launched by `dream app run`, it first
//! registers with its single-use ticket and then holds that narrow grant.
//! When the daemon goes away it keeps showing the last data, marked
//! stale, and reconnects with backoff; an app that had registered does
//! not reconnect with more than it was granted, so it stays stale and
//! asks to be relaunched.

mod manifest;
mod view;

use std::process::ExitCode;
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
        ["--check", path] => match load(path) {
            Ok(m) => {
                println!("ok: {} {} ({})", m.id, m.version, m.title);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        [path] => match load(path) {
            Ok(m) => match run(m) {
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

fn run(manifest: Manifest) -> Result<(), String> {
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
    let result = rt.block_on(event_loop(manifest, &mut terminal, &mut events));
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

async fn fetch(app: &mut App, c: &client::Client) -> Result<(), (bool, String)> {
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

async fn event_loop<B: ratatui::backend::Backend>(
    manifest: Manifest,
    terminal: &mut ratatui::Terminal<B>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> Result<(), String> {
    let source = manifest.data.get(&manifest.view.source).cloned();
    let watch: Vec<String> = source
        .as_ref()
        .map(|s| s.events.clone())
        .unwrap_or_default();
    let refresh = source
        .as_ref()
        .and_then(|s| s.refresh_ms)
        .map(Duration::from_millis);
    let mut app = App::new(manifest);
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
                match name.as_str() {
                    "q" | "Ctrl-c" => return Ok(()),
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
                        let params = match fill(&cmd.params, app.selected_row()) {
                            Ok(p) => p,
                            Err(e) => {
                                app.status = Some(format!("{label}: {e}"));
                                continue;
                            }
                        };
                        match call(&c.client, &cmd.method, params).await {
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
                if conn.is_none() && !gave_up && now >= retry {
                    match connect(&mut registered, &watch).await {
                        Ok(c) => {
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
            match fetch(&mut app, &client).await {
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
