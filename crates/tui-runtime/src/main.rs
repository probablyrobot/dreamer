//! `dreamer-termapp`: runs an app package's manifest as a terminal app in
//! a tmux pane (spec 13.3). It is the pane's real program, so the app
//! works through any plain tmux attach.
//!
//! ```text
//! dreamer-termapp MANIFEST                     run it
//! dreamer-termapp --check FILE [--json]        validate only
//! dreamer-termapp --preview FILE [--size WxH]  draw it once from fixtures
//! dreamer-termapp --replay FILE [--size WxH]   replay fixtures/events.jsonl, as JSON screens
//! dreamer-termapp --live FILE [--status F]     a live preview from fixtures (dream app preview --open)
//! ```
//!
//! A package may have a Luau module beside its manifest (sandboxed, with
//! the app's own grant) for `transform(rows)` and command handlers. A
//! script app's `app.ts` (built to JavaScript by `dream app check`) runs
//! in the TypeScript runtime (`app-script`), and a process app's program
//! speaks the app process protocol; both draw their own view tree. The
//! manifest and module are reloaded when they change: a version that does
//! not validate or load leaves the running one in place, and says why.
//!
//! It talks to its host's daemon. Launched by `dream app run`, it first
//! registers with its single-use ticket and then holds that narrow grant.
//! When the daemon goes away it keeps showing the last data, marked
//! stale, and reconnects with backoff; an app that had registered does
//! not reconnect with more than it was granted, so it stays stale and
//! asks to be relaunched.

mod bound;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;

use app_model::manifest::Manifest;
use app_model::{Input, check_module, load};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
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
        ["--check", path, "--json"] => match checked(path) {
            Ok(m) => {
                let n = m.needs();
                let out = serde_json::json!({
                    "ok": true,
                    "id": m.id,
                    "version": m.version,
                    "title": m.title,
                    "needs": { "methods": n.methods, "panes": n.panes, "all_panes": n.all_panes },
                });
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("{}", serde_json::json!({ "ok": false, "error": e }));
                ExitCode::FAILURE
            }
        },
        ["--preview", path, rest @ ..] => match preview(path, rest) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        ["--replay", path, rest @ ..] => match replay(path, rest) {
            Ok(screens) => {
                println!("{}", serde_json::json!({ "screens": screens }));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        ["--check", path] => match checked(path) {
            Ok(m) => {
                println!("ok: {} {} ({})", m.id, m.version, m.title);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        ["--live", path, rest @ ..] => {
            let status_file = match rest {
                [] => None,
                ["--status", f] => Some(PathBuf::from(f)),
                _ => {
                    eprintln!("usage: dreamer-termapp --live MANIFEST [--status FILE]");
                    return ExitCode::from(2);
                }
            };
            match checked(path) {
                // A web app's view is its page: the host runs it as
                // usual, and the desktop draws the page.
                Ok(m) if m.is_web_app() => match run(PathBuf::from(path), false, None) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("dreamer-termapp: {e}");
                        ExitCode::FAILURE
                    }
                },
                Ok(m) if !m.draws_own_view() => {
                    eprintln!(
                        "a live preview runs script, process, and web apps; preview a manifest app with --preview"
                    );
                    ExitCode::from(2)
                }
                Ok(_) => {
                    let preview = app_model::Preview { status_file };
                    match run(PathBuf::from(path), true, Some(preview)) {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            eprintln!("dreamer-termapp: {e}");
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(2)
                }
            }
        }
        [path] => match checked(path) {
            Ok(m) => match run(PathBuf::from(path), m.draws_own_view(), None) {
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
            eprintln!(
                "usage: dreamer-termapp MANIFEST | --check MANIFEST [--json] | --preview MANIFEST [--size WxH] | --replay MANIFEST [--size WxH] | --live MANIFEST [--status FILE] | --version"
            );
            ExitCode::from(2)
        }
    }
}

/// Load and validate the package at `path`: its manifest, its Luau
/// module, and a script app's JavaScript, which must compile.
fn checked(path: &str) -> Result<Manifest, String> {
    let m = load(path)?;
    check_module(path, &m)?;
    let dir = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    app_model::embeds::check_web(dir, &m)?;
    if m.is_script() {
        let dir = std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let (name, js) = app_model::script_js(dir, &m)?;
        app_script::check(&name, &js).map_err(|e| format!("{}: {e}", dir.join(&name).display()))?;
    }
    Ok(m)
}

/// Draw the package once, as it first looks, from its fixtures and with
/// no daemon; the screen as text, one line per row, trailing blanks
/// trimmed.
fn preview(path: &str, args: &[&str]) -> Result<String, String> {
    let (w, h) = size_arg(args, "--preview")?;
    let manifest = checked(path)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
        .map_err(|e| e.to_string())?;
    if manifest.draws_own_view() {
        // A process or script app's first view, from its code, with no
        // daemon.
        let tree = rt.block_on(app_model::preview_process(
            std::path::Path::new(path),
            Some(app_script::engine()),
        ))?;
        terminal
            .draw(|f| {
                view::draw_tree(
                    &tree,
                    &app_model::embeds::Embeds::default(),
                    &mut view::ViewState::default(),
                    f,
                )
            })
            .map_err(|e| e.to_string())?;
    } else {
        let app = rt.block_on(app_model::preview(std::path::Path::new(path)))?;
        terminal
            .draw(|f| view::draw(&app, f))
            .map_err(|e| e.to_string())?;
    }
    Ok(screen_text(&terminal))
}

/// `[--size WxH]`: the screen's size, 80x24 when not given.
fn size_arg(args: &[&str], mode: &str) -> Result<(u16, u16), String> {
    match args {
        [] => Ok((80, 24)),
        ["--size", size] => {
            let (a, b) = size.split_once('x').ok_or("--size is WIDTHxHEIGHT")?;
            let w: u16 = a.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
            let h: u16 = b.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
            if !(20..=500).contains(&w) || !(5..=200).contains(&h) {
                return Err("--size is 20x5 to 500x200".into());
            }
            Ok((w, h))
        }
        _ => Err(format!(
            "usage: dreamer-termapp {mode} MANIFEST [--size WxH]"
        )),
    }
}

/// The screen as text, one line per row, trailing blanks trimmed.
fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        let line: String = (0..area.width)
            .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The events of a test: `fixtures/events.jsonl` beside the manifest, one
/// app protocol event per line (`{"kind":"key","key":"Down"}`); blank
/// lines and lines starting with `#` are skipped. No file is no events.
fn test_events(path: &str) -> Result<Vec<app_protocol::Event>, String> {
    let file = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("fixtures")
        .join("events.jsonl");
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let mut events = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        events.push(
            serde_json::from_str(line)
                .map_err(|e| format!("fixtures/events.jsonl:{}: {e}", n + 1))?,
        );
    }
    Ok(events)
}

/// An event as input for a manifest app's model, which has no component
/// events of its own to take.
fn as_input(e: app_protocol::Event) -> Result<Input, String> {
    Ok(match e {
        app_protocol::Event::Key { key } => Input::Key(key),
        app_protocol::Event::Text { text } => Input::Text(text),
        app_protocol::Event::Select { node, index } => Input::Select {
            node,
            index: usize::try_from(index).unwrap_or(usize::MAX),
        },
        app_protocol::Event::Scroll { node, steps } => Input::Scroll { node, steps },
        app_protocol::Event::Component { .. } => {
            return Err(
                "a component event is for script and process apps; use keys, text, select, or scroll"
                    .into(),
            );
        }
    })
}

/// What a screen's text does not show of its tree: the focused node, and
/// each table's and list's selected row, one `--- ` line each.
fn tree_notes(tree: Option<&view_tree::Frame>) -> String {
    fn walk(n: &view_tree::Node, out: &mut String) {
        if let view_tree::Kind::Table {
            selected: Some(row),
            ..
        }
        | view_tree::Kind::List {
            selected: Some(row),
        } = &n.kind
        {
            out.push_str(&format!("--- {}: row {row} selected\n", n.key));
        }
        for c in &n.children {
            walk(c, out);
        }
    }
    let mut out = String::new();
    if let Some(t) = tree {
        if let Some(f) = &t.focus {
            out.push_str(&format!("--- focus: {f}\n"));
        }
        walk(&t.root, &mut out);
    }
    out
}

/// Replay the package's `fixtures/events.jsonl` from its fixtures, with no
/// daemon: the screen as it first looks and after each event, as text,
/// with the [`tree_notes`] under it.
fn replay(path: &str, args: &[&str]) -> Result<Vec<String>, String> {
    let (w, h) = size_arg(args, "--replay")?;
    let manifest = checked(path)?;
    let events = test_events(path)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
        .map_err(|e| e.to_string())?;
    let mut state = view::ViewState::default();
    let mut screens = Vec::new();
    let manifest_path = std::path::Path::new(path);
    if manifest.draws_own_view() {
        let mut each = |tree: &view_tree::Frame| {
            terminal
                .draw(|f| {
                    view::draw_tree(tree, &app_model::embeds::Embeds::default(), &mut state, f)
                })
                .map_err(|e| e.to_string())?;
            screens.push(screen_text(&terminal) + &tree_notes(Some(tree)));
            Ok(())
        };
        rt.block_on(app_model::replay_process(
            manifest_path,
            Some(app_script::engine()),
            events,
            &mut each,
        ))?;
    } else {
        let inputs = events
            .into_iter()
            .enumerate()
            .map(|(n, e)| as_input(e).map_err(|e| format!("event {}: {e}", n + 1)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut each = |app: &app_model::App| {
            terminal
                .draw(|f| view::draw_with(app, &mut state, f))
                .map_err(|e| e.to_string())?;
            screens.push(screen_text(&terminal) + &tree_notes(app.frame().as_ref()));
            Ok(())
        };
        rt.block_on(app_model::replay(manifest_path, inputs, &mut each))?;
    }
    Ok(screens)
}

/// Puts the terminal back however the app ends.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
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
            crossterm::event::DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn run(path: PathBuf, own_view: bool, preview: Option<app_model::Preview>) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let screen = Screen::enter()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .map_err(|e| e.to_string())?;
    // Keys and resizes, read on a thread of their own.
    let (input, mut inputs) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            let sent = match ev {
                Event::Key(k) if k.kind == KeyEventKind::Release => continue,
                Event::Key(k) => match key_name(&k) {
                    Some(name) => input.send(Input::Key(name)),
                    None => continue,
                },
                // A paste is text for the focused input, never keys.
                Event::Paste(text) => input.send(Input::Text(text)),
                _ => input.send(Input::Redraw),
            };
            if sent.is_err() {
                return;
            }
        }
    });
    // Scroll positions, by node key, kept across updates.
    let mut state = view::ViewState::default();
    let streams = state.streams.clone();
    if own_view {
        // A process or script app: its code draws the tree.
        let mut publish = |tree: &view_tree::Frame, embeds: &app_model::embeds::Embeds| {
            terminal
                .draw(|f| view::draw_tree(tree, embeds, &mut state, f))
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        let result = rt.block_on(app_model::run_process_with(
            path,
            app_model::Endpoint::pane_app(),
            Some(app_script::engine()),
            preview,
            &mut inputs,
            &mut publish,
            &streams,
        ));
        drop(screen);
        return result;
    }
    let mut publish = |app: &app_model::App| {
        terminal
            .draw(|f| view::draw_with(app, &mut state, f))
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let result = rt.block_on(app_model::run_with(
        path,
        app_model::Endpoint::pane_app(),
        &mut inputs,
        &mut publish,
        &streams,
    ));
    drop(screen);
    result
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

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dreamer-termapp "));
    }
}
