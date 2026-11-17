//! `dreamer-termapp`: runs an app package's manifest as a terminal app in
//! a tmux pane (spec 13.3). It is the pane's real program, so the app
//! works through any plain tmux attach.
//!
//! ```text
//! dreamer-termapp MANIFEST                     run it
//! dreamer-termapp --check FILE [--json]        validate only
//! dreamer-termapp --preview FILE [--size WxH]  draw it once from fixtures
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

mod view;

use std::path::PathBuf;
use std::process::ExitCode;

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
        ["--check", path, "--json"] => {
            match load(path).and_then(|m| check_module(path, &m).map(|()| m)) {
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
            }
        }
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
            eprintln!(
                "usage: dreamer-termapp MANIFEST | --check MANIFEST [--json] | --preview MANIFEST [--size WxH] | --version"
            );
            ExitCode::from(2)
        }
    }
}

/// Draw the package once, as it first looks, from its fixtures and with
/// no daemon; the screen as text, one line per row, trailing blanks
/// trimmed.
fn preview(path: &str, args: &[&str]) -> Result<String, String> {
    let (mut w, mut h) = (80u16, 24u16);
    match args {
        [] => {}
        ["--size", size] => {
            let (a, b) = size.split_once('x').ok_or("--size is WIDTHxHEIGHT")?;
            w = a.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
            h = b.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
            if !(20..=500).contains(&w) || !(5..=200).contains(&h) {
                return Err("--size is 20x5 to 500x200".into());
            }
        }
        _ => return Err("usage: dreamer-termapp --preview MANIFEST [--size WxH]".into()),
    }
    let manifest = load(path)?;
    check_module(path, &manifest)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let app = rt.block_on(app_model::preview(std::path::Path::new(path)))?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
        .map_err(|e| e.to_string())?;
    terminal
        .draw(|f| view::draw(&app, f))
        .map_err(|e| e.to_string())?;
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let line: String = (0..w)
            .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
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
    let (input, mut inputs) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            let sent = match ev {
                Event::Key(k) if k.kind == KeyEventKind::Release => continue,
                Event::Key(k) => match key_name(&k) {
                    Some(name) => input.send(Input::Key(name)),
                    None => continue,
                },
                _ => input.send(Input::Redraw),
            };
            if sent.is_err() {
                return;
            }
        }
    });
    // Scroll positions, by node key, kept across updates.
    let mut state = view::ViewState::default();
    let mut publish = |app: &app_model::App| {
        terminal
            .draw(|f| view::draw_with(app, &mut state, f))
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let result = rt.block_on(app_model::run(
        path,
        app_model::Endpoint::pane_app(),
        &mut inputs,
        &mut publish,
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
