//! Dreamer Web in the desktop: whether the Chromium component web panes
//! need is there, and adding it without the CLI.
//!
//! - **First start.** Off the UI thread, the desktop looks for the
//!   component; when it is missing it uses a copy already on the system
//!   in place if there is one (no download, nothing asked), and otherwise
//!   offers it once, in the tab strip ([`offer`]): "Add" installs it,
//!   "Later" is remembered in the desktop's state directory and it is not
//!   offered at a start again.
//! - **In a pane.** A web pane (or an app's web view) opened without it
//!   shows the page's address and a button to add it ([`in_pane`]), with
//!   the progress, and an error with a retry; the page loads once it is
//!   added, with no restart.
//! - **On demand.** The palette's "Add Dreamer Web".
//!
//! `web.component` in `dreamer.yaml` decides what is offered: `ask` (the
//! default) as above; `auto` adds it, without asking, whenever it is
//! missing; `never` offers nothing, and web panes say it is turned off.
//! Nothing downloads without a click, unless the setting is `auto`.
//!
//! The install itself is the one `dream web install` runs
//! ([`web_component::install`]), on a thread of its own: it downloads with
//! `curl`, checks, and swaps the component in, keeping what was there on
//! any failure.

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::{
    App, AppContext as _, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use web_component::install::{Installed, Installer, Source, Step};
use workspace_config::WebComponent;

use crate::theme::{Theme, c};

/// Where the component is, as far as the desktop knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// Ready to draw pages.
    Ready,
    /// Not there; or there but another version's or not usable, and why.
    Missing(Option<String>),
    /// Being added.
    Adding(Step),
    /// The last try to add it failed, and why.
    Failed(String),
}

struct Shared {
    status: Status,
    /// The first-start offer shows in the tab strip.
    offer: bool,
}

static STATE: Mutex<Shared> = Mutex::new(Shared {
    status: Status::Missing(None),
    offer: false,
});

fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut s = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut s)
}

/// Whether this build can add the component (it has a web engine, on a
/// platform the component is built for).
pub(crate) fn can_add() -> bool {
    web_engine::AVAILABLE && web_component::TARGET.is_some()
}

/// What `web.component` says.
pub(crate) fn mode() -> WebComponent {
    crate::live::with(|l| l.web.component).unwrap_or_default()
}

/// The component's status, as last looked at.
pub(crate) fn status() -> Status {
    with(|s| s.status.clone())
}

/// What is installed now: a manifest read and a few files looked at.
fn look() -> Status {
    let dir = web_component::installed_dir();
    let state = web_component::check(&dir);
    match state {
        web_component::State::Ready(_) => Status::Ready,
        web_component::State::Missing => Status::Missing(None),
        _ => Status::Missing(state.problem(&dir)),
    }
}

/// Look at what is installed now (a web pane opening), unless it is being
/// added; a failure stays shown until it is tried again. Whether it is
/// ready.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn look_now() -> bool {
    let now = look();
    with(|s| {
        match (&s.status, &now) {
            (Status::Adding(_), _) | (Status::Failed(_), Status::Missing(_)) => {}
            _ => s.status = now,
        }
        s.status == Status::Ready
    })
}

/// What an install thread reports.
enum Report {
    Step(Step),
    Done(Result<Installed, String>),
}

/// Add the component: a copy on the system if there is one, else the
/// download (`DREAMER_WEB_URL`, `web.url`, or the release). Runs on a
/// thread of its own; every window repaints as it goes. Does nothing while
/// it is already being added.
pub(crate) fn add(cx: &mut App) {
    if !can_add() {
        return;
    }
    let start = with(|s| {
        if matches!(s.status, Status::Adding(_)) {
            return false;
        }
        s.status = Status::Adding(Step::Looking);
        true
    });
    if !start {
        return;
    }
    cx.refresh_windows();
    let setting = crate::live::with(|l| l.web.url.clone()).flatten();
    let (tx, rx) = async_channel::unbounded::<Report>();
    let thread = std::thread::Builder::new()
        .name("dreamer-web-install".into())
        .spawn(move || {
            let result = Installer::for_this_build().and_then(|i| {
                let url = web_component::download_url(setting.as_deref(), &i.want.target);
                i.install(&Source::Find(url), &mut |step| {
                    let _ = tx.try_send(Report::Step(step));
                })
            });
            let _ = tx.try_send(Report::Done(result));
        });
    if let Err(e) = thread {
        with(|s| s.status = Status::Failed(format!("cannot start the install: {e}")));
        return;
    }
    cx.spawn(async move |cx| {
        while let Ok(report) = rx.recv().await {
            let done = matches!(report, Report::Done(_));
            cx.update(|cx| {
                with(|s| match report {
                    Report::Step(step) => s.status = Status::Adding(step),
                    Report::Done(Ok(_)) => {
                        s.status = Status::Ready;
                        s.offer = false;
                    }
                    Report::Done(Err(e)) => s.status = Status::Failed(e),
                });
                // Web panes waiting for it load their pages as they repaint.
                cx.refresh_windows();
            });
            if done {
                return;
            }
        }
    })
    .detach();
}

/// Where the answer to the first-start offer is kept.
fn answer_path() -> Option<PathBuf> {
    workspace_config::desktop_state::state_dir().map(|d| d.join("web-component-offer"))
}

/// Keep the answer to the first-start offer, off the UI thread.
fn answer(cx: &mut App, answer: &'static str) {
    with(|s| s.offer = false);
    if let Some(path) = answer_path() {
        cx.background_spawn(async move {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, format!("{answer}\n")) {
                eprintln!("dreamer: {}: {e}", path.display());
            }
        })
        .detach();
    }
    cx.refresh_windows();
}

/// At the desktop's start: look for the component off the UI thread; when
/// it is missing, use a copy on the system in place, else (per
/// `web.component`) add it or offer it once.
pub(crate) fn startup(cx: &mut App) {
    if !can_add() || mode() == WebComponent::Never {
        return;
    }
    let (tx, rx) = async_channel::bounded::<(Status, bool)>(1);
    let looked = std::thread::Builder::new()
        .name("dreamer-web-look".into())
        .spawn(move || {
            let mut now = look();
            if now != Status::Ready
                && let Ok(Some(_)) =
                    Installer::for_this_build().and_then(|i| i.use_system_copy(&mut |_| {}))
            {
                now = Status::Ready;
            }
            let answered = answer_path().is_some_and(|p| p.exists());
            let _ = tx.try_send((now, answered));
        });
    if looked.is_err() {
        return;
    }
    cx.spawn(async move |cx| {
        let Ok((now, answered)) = rx.recv().await else {
            return;
        };
        cx.update(|cx| {
            with(|s| {
                if !matches!(s.status, Status::Adding(_)) {
                    s.status = now.clone();
                }
            });
            if now != Status::Ready {
                match mode() {
                    WebComponent::Auto => add(cx),
                    WebComponent::Ask if !answered => with(|s| s.offer = true),
                    _ => {}
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

fn size_label() -> String {
    format!("about {} MB", web_component::DOWNLOAD_MB)
}

/// A small text button in the tab strip.
fn strip_button(
    id: &'static str,
    label: &'static str,
    color: u32,
    on: impl Fn(&mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .px_1()
        .text_color(c(color))
        .cursor_pointer()
        .child(label)
        // A press here is not a window drag.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| on(cx))
}

/// The first-start offer, for the tab strip, while it stands.
pub(crate) fn offer(t: &Theme) -> Option<gpui::Div> {
    let (shown, status) = with(|s| (s.offer, s.status.clone()));
    if !shown || mode() != WebComponent::Ask || status == Status::Ready {
        return None;
    }
    let row = div().flex().items_center().gap_1().px_2();
    Some(match status {
        Status::Adding(step) => row.text_color(c(t.muted)).child(step.to_string()),
        Status::Failed(why) => row
            .child(
                div()
                    .text_color(c(t.warning))
                    .child(format!("Dreamer Web was not added: {}", shorten(&why, 60))),
            )
            .child(strip_button("web-offer-retry", "Try again", t.accent, add))
            .child(strip_button("web-offer-later", "Later", t.muted, |cx| {
                answer(cx, "later");
            })),
        _ => row
            .child(
                div()
                    .text_color(c(t.muted))
                    .child(format!("Add web panes? Dreamer Web, {}", size_label())),
            )
            .child(strip_button("web-offer-add", "Add", t.accent, |cx| {
                answer(cx, "yes");
                add(cx);
            }))
            .child(strip_button("web-offer-later", "Later", t.muted, |cx| {
                answer(cx, "later");
            })),
    })
}

/// What a web pane shows instead of its page while the component is not
/// there: why, and the button to add it, the progress, or the error and a
/// retry.
pub(crate) fn in_pane(t: &Theme) -> gpui::Div {
    let button = |label: String| {
        div()
            .id("web-add")
            .flex_none()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(c(t.divider))
            .bg(c(t.raised))
            .text_color(c(t.text))
            .hover(|s| s.border_color(c(t.focus)))
            .cursor_pointer()
            .child(label)
            .on_click(|_, _, cx| add(cx))
    };
    let col = div().flex().flex_col().items_start().gap_2().pt_1();
    if !can_add() {
        return col;
    }
    let add_label = format!("Add Dreamer Web ({})", size_label());
    match status() {
        Status::Ready => col,
        Status::Missing(_) if mode() == WebComponent::Never => {
            col.child(div().text_color(c(t.muted)).child(
                "Web pages need Dreamer Web, which is turned off here (web.component: never in \
                 dreamer.yaml). \"Add Dreamer Web\" in the palette adds it anyway.",
            ))
        }
        Status::Missing(why) => col
            .child(div().text_color(c(t.muted)).child(why.unwrap_or_else(|| {
                "Web pages need Dreamer Web (Chromium), which is added apart from Dreamer."
                    .to_owned()
            })))
            .child(button(add_label)),
        Status::Adding(step) => {
            let frac = match step {
                Step::Downloading {
                    got,
                    total: Some(total),
                } if total > 0 => Some((got as f32 / total as f32).clamp(0.0, 1.0)),
                _ => None,
            };
            const BAR: f32 = 220.0;
            col.child(div().text_color(c(t.muted)).child(step.to_string()))
                .children(frac.map(|f| {
                    div()
                        .w(px(BAR))
                        .h(px(3.0))
                        .rounded_sm()
                        .bg(c(t.divider))
                        .child(div().w(px(BAR * f)).h_full().rounded_sm().bg(c(t.accent)))
                }))
        }
        Status::Failed(why) => col
            .child(div().text_color(c(t.danger)).child(why))
            .child(button("Try again".into())),
    }
}

/// At most `max` characters of `s`, with an ellipsis when cut.
fn shorten(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_owned(),
    }
}
