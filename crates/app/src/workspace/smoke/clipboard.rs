//! `dreamer --smoke-clipboard`: copy and paste in every kind of pane, with
//! an in-memory clipboard (the user's is never read or replaced). Against
//! an isolated tmux whose session `main` has one shell pane that printed
//! `READY-%0`, and a private configuration whose harness drawer runs a
//! shell.
//!
//! Every gesture is dispatched to the window as a person's would be
//! (keystrokes, mouse events, the platform text input), so the window's
//! own input policy decides what each one does.

use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, AppContext as _, AsyncApp, EntityInputHandler as _, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, WeakEntity, point, px,
};

use super::super::{PaneView, Workspace};
use super::{fail, read, tmux, tmux_output, wait_for, wait_long};

/// The clipboard shortcuts on this platform.
const COPY: &str = if cfg!(target_os = "macos") {
    "cmd-c"
} else {
    "ctrl-shift-c"
};
const PASTE: &str = if cfg!(target_os = "macos") {
    "cmd-v"
} else {
    "ctrl-shift-v"
};
const PALETTE: &str = if cfg!(target_os = "macos") {
    "cmd-shift-p"
} else {
    "ctrl-shift-p"
};

/// The line the terminal checks select in: a word at each end and one in
/// the middle (columns 0-4, 6-10, 12-18).
const WORDS: &str = "ALPHA BRAVO CHARLIE";

fn press(any: AnyWindowHandle, cx: &mut AsyncApp, keys: &[&'static str]) {
    for k in keys {
        let _ = cx.update_window(any, |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse(k) {
                window.dispatch_keystroke(ks, cx);
            }
        });
    }
}

fn dispatch(any: AnyWindowHandle, cx: &mut AsyncApp, events: Vec<PlatformInput>) {
    for ev in events {
        let _ = cx.update_window(any, move |_, window, cx| {
            window.dispatch_event(ev, cx);
        });
    }
}

fn down(at: gpui::Point<gpui::Pixels>, button: MouseButton, n: usize) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button,
        position: at,
        modifiers: Modifiers::default(),
        click_count: n,
        first_mouse: false,
    })
}

fn up(at: gpui::Point<gpui::Pixels>, button: MouseButton, n: usize) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button,
        position: at,
        modifiers: Modifiers::default(),
        click_count: n,
    })
}

fn drag_to(at: gpui::Point<gpui::Pixels>) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    })
}

async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

/// What the in-memory clipboard holds now.
fn clipboard() -> Option<String> {
    crate::clip::memory().and_then(|m| m.clipboard)
}

/// How many times `text` was written to the in-memory clipboard.
fn writes_of(text: &str) -> usize {
    crate::clip::memory().map_or(0, |m| m.writes.iter().filter(|w| *w == text).count())
}

fn pane_has_line(w: &Workspace, cx: &gpui::App, pane: u32, line: &str) -> bool {
    w.pane_shell(pane).is_some_and(|s| {
        s.read(cx)
            .frame
            .text_dump()
            .lines()
            .any(|l| l.trim_end() == line)
    })
}

/// Text into a terminal pane through its feed (setup, not the check).
fn type_into(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    text: &str,
) {
    let text = text.to_owned();
    let _ = cx.update_window(any, move |_, _, cx| {
        let _ = ws.read_with(cx, |w, cx| {
            if let Some(s) = w.pane_shell(pane) {
                s.read(cx)
                    .send_all([api::terminal_input::Event::Text(api::TextInput { text })]);
            }
        });
    });
}

/// Where cell (`col`, the middle of `row`) of a terminal pane is drawn, for
/// the row that shows `line` last.
fn cell_of(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    line: &str,
) -> impl Fn(f32) -> gpui::Point<gpui::Pixels> + use<> {
    let found = read(any, ws, cx, |w, cx| {
        let s = w.pane_shell(pane)?;
        let s = s.read(cx);
        let row = s
            .frame
            .rows
            .iter()
            .rposition(|r| r.text().trim_end() == line)?;
        Some((s.grid?, row))
    })
    .flatten();
    let Some((grid, row)) = found else {
        fail(&format!("no row showing {line:?} in pane %{pane}"));
    };
    move |col: f32| {
        point(
            grid.origin.x + grid.cell.width * col,
            grid.origin.y + grid.cell.height * (row as f32 + 0.5),
        )
    }
}

/// tmux's newest paste buffer.
fn newest_buffer(daemon: &[String]) -> String {
    tmux_output(daemon, &["show-buffer"])
}

/// Run a program in `pane` that asks for bracketed paste, then wait for
/// it to be ready to read `n` bytes and print them with ESC shown as `E`.
async fn paste_probe(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    pane: u32,
    ready: &str,
    n: usize,
) {
    let (a, b) = ready.split_at(ready.len() / 2);
    type_into(
        any,
        ws,
        cx,
        pane,
        &format!(
            "clear; printf '\\033[?2004h'; stty raw -echo; printf '{a}''{b}\\r\\n'; head -c {n} | tr '\\033' E; printf '\\r\\n'; stty sane; printf '\\033[?2004l'\r"
        ),
    );
    let ready = ready.to_owned();
    wait_for(any, ws, cx, "the paste probe ready", move |w, cx| {
        pane_has_line(w, cx, pane, &ready)
    })
    .await;
}

/// A text pasted bracketed, as the probe prints it.
fn bracketed(text: &str) -> String {
    format!("E[200~{text}E[201~")
}

pub(in crate::workspace) async fn run(
    any: AnyWindowHandle,
    ws: WeakEntity<Workspace>,
    daemon: Vec<String>,
    cx: &mut AsyncApp,
) {
    let ws = &ws;
    if crate::clip::memory().is_none() {
        fail("the clipboard smoke needs the in-memory clipboard");
    }
    wait_for(any, ws, cx, "the shell pane ready", |w, cx| {
        w.focused == Some(0) && pane_has_line(w, cx, 0, "READY-%0")
    })
    .await;
    // Selection and clicks land where panes were drawn: a window that is
    // not being drawn (screen locked, display asleep, behind another app)
    // cannot be checked, which is "not run", never a failure.
    let before = terminal_render::paint_count();
    let _ = cx.update_window(any, |_, window, _| window.refresh());
    pause(cx, 500).await;
    let painted = read(any, ws, cx, |w, cx| {
        w.pane_shell(0).is_some_and(|s| s.read(cx).grid.is_some())
    });
    if terminal_render::paint_count() == before || painted != Some(true) {
        println!(
            "smoke: the display did not refresh (screen locked or asleep?); clipboard checks not run"
        );
        std::process::exit(5);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !crate::prefix::is_prefix(&replica::keys::Key {
        code: replica::keys::Code::Char('b'),
        ctrl: true,
        alt: false,
        shift: false,
    }) {
        if Instant::now() > deadline {
            fail("tmux's prefix (C-b) was never read");
        }
        pause(cx, 50).await;
    }

    terminal(any, ws, &daemon, cx).await;
    tmux_sync(any, ws, &daemon, cx).await;
    drawer(any, ws, cx).await;
    app_pane(any, ws, &daemon, cx).await;
    println!("smoke: web pane: not applicable (Dreamer has no web panes yet)");
    std::process::exit(0);
}

/// A terminal pane: drag, double-click, and triple-click select and Copy
/// copies; copy on select when enabled; Paste is a bracketed paste; the
/// middle button pastes tmux's newest buffer; a program's OSC 52 write.
async fn terminal(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    daemon: &[String],
    cx: &mut AsyncApp,
) {
    type_into(any, ws, cx, 0, &format!("clear; echo '{WORDS}'\r"));
    wait_for(any, ws, cx, "the words to select", |w, cx| {
        pane_has_line(w, cx, 0, WORDS)
    })
    .await;
    pause(cx, 200).await;
    let at = cell_of(any, ws, cx, 0, WORDS);

    // A drag over ALPHA: tmux's newest buffer at once (as marking text in
    // tmux does), the clipboard only on Copy (copy on select is off).
    let before = crate::clip::memory().map_or(0, |m| m.writes.len());
    dispatch(
        any,
        cx,
        vec![
            down(at(0.1), MouseButton::Left, 1),
            drag_to(at(2.5)),
            drag_to(at(4.9)),
            up(at(4.9), MouseButton::Left, 1),
        ],
    );
    let d = daemon.to_vec();
    wait_for(
        any,
        ws,
        cx,
        "the dragged word as tmux's newest buffer",
        move |_, _| newest_buffer(&d) == "ALPHA",
    )
    .await;
    pause(cx, 300).await;
    if crate::clip::memory().map_or(0, |m| m.writes.len()) != before {
        fail("a selection reached the clipboard with copy on select off");
    }
    if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        if crate::clip::memory().and_then(|m| m.primary).as_deref() != Some("ALPHA") {
            fail("a finished selection did not become the primary selection");
        }
        println!("smoke: a finished selection became the primary selection (\"ALPHA\")");
    }
    press(any, cx, &[COPY]);
    wait_for(any, ws, cx, "the dragged word copied", |_, _| {
        clipboard().as_deref() == Some("ALPHA")
    })
    .await;
    println!("smoke: terminal pane: a drag selected \"ALPHA\"; {COPY} copied it");

    // A double click selects BRAVO; Copy also makes it tmux's newest
    // buffer, and tmux does not echo it back to the clipboard.
    let d = daemon.to_vec();
    pause(cx, 600).await;
    dispatch(
        any,
        cx,
        vec![
            down(at(7.5), MouseButton::Left, 1),
            up(at(7.5), MouseButton::Left, 1),
            down(at(7.5), MouseButton::Left, 2),
            up(at(7.5), MouseButton::Left, 2),
        ],
    );
    wait_for(
        any,
        ws,
        cx,
        "the double-clicked word in tmux",
        move |_, _| newest_buffer(&d) == "BRAVO",
    )
    .await;
    press(any, cx, &[COPY]);
    wait_for(any, ws, cx, "the double-clicked word copied", |_, _| {
        clipboard().as_deref() == Some("BRAVO")
    })
    .await;
    pause(cx, 800).await;
    if writes_of("BRAVO") != 1 || newest_buffer(daemon) != "BRAVO" {
        fail(&format!(
            "a Dreamer copy reached the clipboard {} times (tmux's newest buffer {:?})",
            writes_of("BRAVO"),
            newest_buffer(daemon)
        ));
    }
    println!(
        "smoke: terminal pane: a double click selected \"BRAVO\"; the copy became tmux's newest buffer, with no echo to the clipboard"
    );

    // A triple click selects the line.
    pause(cx, 600).await;
    dispatch(
        any,
        cx,
        vec![
            down(at(14.5), MouseButton::Left, 1),
            up(at(14.5), MouseButton::Left, 1),
            down(at(14.5), MouseButton::Left, 2),
            up(at(14.5), MouseButton::Left, 2),
            down(at(14.5), MouseButton::Left, 3),
            up(at(14.5), MouseButton::Left, 3),
        ],
    );
    pause(cx, 300).await;
    press(any, cx, &[COPY]);
    wait_for(any, ws, cx, "the triple-clicked line copied", |_, _| {
        clipboard().as_deref() == Some(WORDS)
    })
    .await;
    println!("smoke: terminal pane: a triple click selected the line; {COPY} copied {WORDS:?}");

    // Copy on select, when enabled: the drag alone reaches the clipboard.
    crate::live::adjust(|l| l.copy_on_select = true);
    pause(cx, 600).await;
    dispatch(
        any,
        cx,
        vec![
            down(at(12.1), MouseButton::Left, 1),
            drag_to(at(15.5)),
            drag_to(at(18.9)),
            up(at(18.9), MouseButton::Left, 1),
        ],
    );
    wait_for(any, ws, cx, "the selection copied on select", |_, _| {
        clipboard().as_deref() == Some("CHARLIE")
    })
    .await;
    crate::live::adjust(|l| l.copy_on_select = false);
    println!("smoke: terminal pane: with copy_on_select, a drag copied \"CHARLIE\" with no key");

    // Paste: the program asked for bracketed paste, and gets it.
    crate::clip::set_memory("PASTE-ME");
    paste_probe(any, ws, cx, 0, "PASTE-READY", 20).await;
    press(any, cx, &[PASTE]);
    let want = bracketed("PASTE-ME");
    wait_for(any, ws, cx, "the bracketed paste", move |w, cx| {
        pane_has_line(w, cx, 0, &want)
    })
    .await;
    println!(
        "smoke: terminal pane: {PASTE} pasted as a bracketed paste ({})",
        bracketed("PASTE-ME")
    );

    // The middle button pastes tmux's newest buffer.
    tmux(daemon, &["set-buffer", "MIDDLE-TEXT"]);
    paste_probe(any, ws, cx, 0, "MIDDLE-READY", 23).await;
    let at = cell_of(any, ws, cx, 0, "MIDDLE-READY");
    dispatch(
        any,
        cx,
        vec![
            down(at(2.5), MouseButton::Middle, 1),
            up(at(2.5), MouseButton::Middle, 1),
        ],
    );
    let want = bracketed("MIDDLE-TEXT");
    wait_for(any, ws, cx, "the middle-click paste", move |w, cx| {
        pane_has_line(w, cx, 0, &want)
    })
    .await;
    println!("smoke: terminal pane: a middle click pasted tmux's newest buffer (\"MIDDLE-TEXT\")");

    // A program writes the clipboard (OSC 52).
    let b64 = crate::base64(b"OSC52-TEXT");
    type_into(
        any,
        ws,
        cx,
        0,
        &format!("clear; printf '\\033]52;c;{b64}\\007'; echo OSC''-DONE\r"),
    );
    wait_for(any, ws, cx, "the program's clipboard write", |w, cx| {
        pane_has_line(w, cx, 0, "OSC-DONE") && clipboard().as_deref() == Some("OSC52-TEXT")
    })
    .await;
    println!(
        "smoke: terminal pane: a program's OSC 52 write reached the clipboard (\"OSC52-TEXT\")"
    );
}

/// A buffer set in tmux by another client reaches the clipboard.
async fn tmux_sync(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    daemon: &[String],
    cx: &mut AsyncApp,
) {
    tmux(daemon, &["set-buffer", "FROM-TMUX"]);
    wait_for(any, ws, cx, "tmux's buffer on the clipboard", |_, _| {
        clipboard().as_deref() == Some("FROM-TMUX")
    })
    .await;
    pause(cx, 500).await;
    if writes_of("FROM-TMUX") != 1 {
        fail(&format!(
            "tmux's buffer reached the clipboard {} times",
            writes_of("FROM-TMUX")
        ));
    }
    println!("smoke: tmux: a buffer set by another client reached the clipboard once");
}

/// The harness drawer (a shell here) copies and pastes as a terminal pane
/// does.
async fn drawer(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
    let _ = cx.update_window(any, |_, window, cx| {
        if let Some(ws) = ws.upgrade() {
            ws.update(cx, |w, cx| w.toggle_drawer(false, window, cx));
        }
    });
    let drawer_pane = |w: &Workspace| {
        let session = w.session?;
        w.current
            .as_ref()?
            .panes
            .iter()
            .find(|p| p.drawer_of == Some(session))
            .map(|p| p.id)
    };
    wait_long(
        any,
        ws,
        cx,
        "the drawer shown and focused",
        Duration::from_secs(20),
        move |w, cx| {
            drawer_pane(w).is_some_and(|d| {
                w.focused == Some(d)
                    && w.pane_shell(d)
                        .is_some_and(|s| s.read(cx).frame.text_dump().contains('$'))
            })
        },
    )
    .await;
    let pane = read(any, ws, cx, |w, _| drawer_pane(w))
        .flatten()
        .unwrap_or_else(|| fail("no drawer"));
    type_into(any, ws, cx, pane, "clear; echo DRAWER-''OK\r");
    wait_for(any, ws, cx, "the drawer's line", move |w, cx| {
        pane_has_line(w, cx, pane, "DRAWER-OK")
    })
    .await;
    pause(cx, 200).await;
    let at = cell_of(any, ws, cx, pane, "DRAWER-OK");
    dispatch(
        any,
        cx,
        vec![
            down(at(0.1), MouseButton::Left, 1),
            drag_to(at(4.5)),
            drag_to(at(8.9)),
            up(at(8.9), MouseButton::Left, 1),
        ],
    );
    pause(cx, 200).await;
    press(any, cx, &[COPY]);
    wait_for(any, ws, cx, "the drawer's selection copied", |_, _| {
        clipboard().as_deref() == Some("DRAWER-OK")
    })
    .await;
    crate::clip::set_memory("echo DRAWER-''PASTED");
    press(any, cx, &[PASTE, "enter"]);
    wait_for(any, ws, cx, "the paste run in the drawer", move |w, cx| {
        pane_has_line(w, cx, pane, "DRAWER-PASTED")
    })
    .await;
    let _ = cx.update_window(any, |_, window, cx| {
        if let Some(ws) = ws.upgrade() {
            ws.update(cx, |w, cx| w.toggle_drawer(false, window, cx));
        }
    });
    wait_long(
        any,
        ws,
        cx,
        "the drawer hidden",
        Duration::from_secs(10),
        move |w, _| !w.panes.contains_key(&pane) && w.focused == Some(0),
    )
    .await;
    println!(
        "smoke: harness drawer: a drag and {COPY} copied \"DRAWER-OK\"; {PASTE} pasted into its shell, as in a terminal pane"
    );
}

const FORM: &str = r#"schema_version = 1
id = "smoke.form"
version = "0.1.0"
title = "Smoke form"
capabilities = ["messages.publish"]

[view]
type = "form"
source = "none"
submit = "send"
fields = [{ name = "name", kind = "text" }, { name = "note", kind = "text" }]

[commands.send]
key = "Enter"
method = "messages.publish"
params = { topic = "smoke.form", payload = "${form.name}" }
"#;

/// A native app pane: click focuses it, typing and an IME commit go into
/// the focused field, Dreamer's shortcuts still work, Paste and a middle
/// click type into the focused field, Copy takes the field, and the
/// prefix and an arrow move focus to the terminal and back.
async fn app_pane(
    any: AnyWindowHandle,
    ws: &WeakEntity<Workspace>,
    daemon: &[String],
    cx: &mut AsyncApp,
) {
    let (socket, session) = read(any, ws, cx, |w, _| Some((w.socket.clone(), w.session?)))
        .flatten()
        .unwrap_or_else(|| fail("no session for the app pane"));
    let dir = std::env::temp_dir().join(format!("dreamer-smoke-form-{}", std::process::id()));
    let manifest = dir.join("app.toml");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&manifest, FORM)) {
        fail(&format!("could not write the app package: {e}"));
    }
    let termapp = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("dreamer-termapp")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| fail("no dreamer-termapp beside dreamer (build it first)"));
    let argv = vec![
        termapp.display().to_string(),
        manifest.display().to_string(),
    ];
    let launched = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let c = client::Client::connect(&socket, "smoke")
                .await
                .map_err(|e| e.to_string())?;
            match c
                .call(api::Request::AppsLaunch(api::LaunchApp {
                    app_id: "smoke.form".into(),
                    grant: Some(api::Grant::default()),
                    argv,
                    session,
                    ..api::LaunchApp::default()
                }))
                .await
            {
                Ok(api::Response::AppsLaunch(l)) => Ok(l.pane),
                Ok(_) => Err("unexpected answer".to_owned()),
                Err(e) => Err(e.to_string()),
            }
        })
    })
    .join()
    .unwrap_or_else(|_| Err("the launch thread panicked".into()));
    let pane = launched.unwrap_or_else(|e| fail(&format!("could not launch the form app: {e}")));
    // Beside the terminal pane, in the window shown.
    tmux(
        daemon,
        &[
            "join-pane",
            "-d",
            "-h",
            "-s",
            &format!("%{pane}"),
            "-t",
            "%0",
        ],
    );
    let form = move |w: &Workspace, cx: &gpui::App| -> Option<(Vec<String>, usize)> {
        match w.panes.get(&pane)? {
            PaneView::App { view, .. } => {
                let shown = view.read(cx).shown.clone()?;
                (shown.link == app_model::Link::Live).then_some((shown.form, shown.focus))
            }
            PaneView::Terminal(_) => None,
        }
    };
    wait_long(
        any,
        ws,
        cx,
        "the form drawn natively in its pane",
        Duration::from_secs(20),
        move |w, cx| form(w, cx).is_some(),
    )
    .await;

    // A click on the pane (its title) focuses it.
    let title = read(any, ws, cx, |w, _| {
        let p = w.current.as_ref()?.panes.iter().find(|p| p.id == pane)?;
        let (origin, (cw, ch)) = (w.origin?, w.cell?);
        Some(point(
            origin.x + cw * p.left as f32 + px(8.0),
            origin.y + ch * p.top as f32 + px(6.0),
        ))
    })
    .flatten()
    .unwrap_or_else(|| fail("the app pane has no place on screen"));
    dispatch(
        any,
        cx,
        vec![
            down(title, MouseButton::Left, 1),
            up(title, MouseButton::Left, 1),
        ],
    );
    wait_for(
        any,
        ws,
        cx,
        "the app pane focused by a click",
        move |w, _| w.focused == Some(pane),
    )
    .await;

    // Typing, and an IME composition committed, go into the focused field
    // (once a frame has registered its text input).
    pause(cx, 300).await;
    press(any, cx, &["h", "i"]);
    let _ = cx.update_window(any, |_, window, cx| {
        let view = ws
            .upgrade()
            .and_then(|w| match w.read(cx).panes.get(&pane) {
                Some(PaneView::App { view, .. }) => Some(view.clone()),
                _ => None,
            });
        if let Some(view) = view {
            view.update(cx, |p, cx| {
                p.replace_and_mark_text_in_range(None, "日", None, window, cx);
                p.replace_text_in_range(None, "日本", window, cx);
            });
        }
    });
    wait_for(
        any,
        ws,
        cx,
        "typed text in the form's field",
        move |w, cx| form(w, cx).is_some_and(|(f, _)| f[0] == "hi日本"),
    )
    .await;
    println!(
        "smoke: app pane: a click focused it; typed and IME-committed text reached its field (\"hi日本\")"
    );

    // Dreamer's shortcuts work in an app pane, and focus comes back to it.
    press(any, cx, &[PALETTE]);
    wait_for(any, ws, cx, "the palette over the app pane", |w, _| {
        w.palette.is_some()
    })
    .await;
    press(any, cx, &["escape"]);
    wait_for(
        any,
        ws,
        cx,
        "the palette closed, the app pane focused",
        move |w, _| w.palette.is_none() && w.focused == Some(pane),
    )
    .await;

    // Tab to the next field; Paste and a middle click type into it.
    press(any, cx, &["tab"]);
    wait_for(any, ws, cx, "the second field focused", move |w, cx| {
        form(w, cx).is_some_and(|(_, focus)| focus == 1)
    })
    .await;
    crate::clip::set_memory("pasted\nwords");
    press(any, cx, &[PASTE]);
    wait_for(any, ws, cx, "the paste in the field", move |w, cx| {
        form(w, cx).is_some_and(|(f, _)| f[1] == "pasted words")
    })
    .await;
    tmux(daemon, &["set-buffer", "+MID"]);
    pause(cx, 300).await;
    dispatch(
        any,
        cx,
        vec![
            down(title, MouseButton::Middle, 1),
            up(title, MouseButton::Middle, 1),
        ],
    );
    wait_for(
        any,
        ws,
        cx,
        "the middle-click paste in the field",
        move |w, cx| form(w, cx).is_some_and(|(f, _)| f[1] == "pasted words+MID"),
    )
    .await;
    println!(
        "smoke: app pane: {PALETTE} opened the palette; {PASTE} and a middle click typed into the focused field (\"pasted words+MID\")"
    );

    // Copy takes the focused field, to the clipboard and tmux.
    press(any, cx, &[COPY]);
    let d = daemon.to_vec();
    wait_for(any, ws, cx, "the field copied", move |_, _| {
        clipboard().as_deref() == Some("pasted words+MID")
            && newest_buffer(&d) == "pasted words+MID"
    })
    .await;
    println!(
        "smoke: app pane: {COPY} copied the focused field to the clipboard and tmux's newest buffer"
    );

    // The prefix and an arrow: tmux selects the terminal pane, and focus
    // follows; and back again from the terminal.
    press(any, cx, &["ctrl-b", "left"]);
    wait_for(
        any,
        ws,
        cx,
        "focus on the terminal pane by the keyboard",
        |w, _| w.focused == Some(0),
    )
    .await;
    press(any, cx, &["ctrl-b", "right"]);
    wait_for(
        any,
        ws,
        cx,
        "focus back on the app pane by the keyboard",
        move |w, _| w.focused == Some(pane),
    )
    .await;
    println!("smoke: app pane: the prefix and an arrow moved focus to the terminal pane and back");
    tmux(daemon, &["kill-pane", "-t", &format!("%{pane}")]);
    let _ = std::fs::remove_dir_all(&dir);
}
