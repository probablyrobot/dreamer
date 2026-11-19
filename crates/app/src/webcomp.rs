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
//!
//! The daemon hears the status whenever it changes ([`report_to`],
//! `web.component`), so an agent's `web.open` waits while it is being
//! added or offered here, and fails at once when it is turned off or its
//! install failed.

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
    /// The status was looked at (until then it is only assumed missing,
    /// and not reported).
    looked: bool,
    /// Where the status is reported to the daemon, and what was sent last.
    to: Option<async_channel::Sender<api::WebComponentState>>,
    sent: Option<api::WebComponentState>,
    /// Copies already on the system are not used (a smoke run, which must
    /// add the component itself and never link the user's own).
    no_system_copies: bool,
}

static STATE: Mutex<Shared> = Mutex::new(Shared {
    status: Status::Missing(None),
    offer: false,
    looked: false,
    to: None,
    sent: None,
    no_system_copies: false,
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

/// What the daemon is told of the status, once it was looked at.
fn report(s: &Shared, mode: WebComponent) -> Option<api::WebComponentState> {
    use api::WebComponentStatus as W;
    if !can_add() || !s.looked {
        return None;
    }
    let (status, detail) = match &s.status {
        Status::Ready => (W::Ready, None),
        Status::Adding(_) => (W::Adding, None),
        Status::Failed(why) => (W::Failed, Some(why.clone())),
        Status::Missing(_) if mode == WebComponent::Never => (
            W::Off,
            Some(
                "it is turned off in the desktop (web.component: never in dreamer.yaml)".to_owned(),
            ),
        ),
        Status::Missing(why) => (W::Missing, why.clone()),
    };
    Some(api::WebComponentState {
        status: status as i32,
        detail,
    })
}

/// Tell the daemon the status, if it changed since it was last told.
pub(crate) fn publish() {
    let mode = mode();
    with(|s| {
        let Some(now) = report(s, mode) else {
            return;
        };
        if s.sent.as_ref() == Some(&now) {
            return;
        }
        if let Some(to) = &s.to
            && to.try_send(now.clone()).is_ok()
        {
            s.sent = Some(now);
        }
    });
}

/// Report the status to the daemon through `to` from now on (the capture
/// connection, which reports it again whenever it connects).
pub(crate) fn report_to(to: async_channel::Sender<api::WebComponentState>) {
    with(|s| {
        s.to = Some(to);
        s.sent = None;
    });
    publish();
}

/// A smoke run adds the component itself: copies already on the system
/// (the user's own) are neither used nor linked.
pub(crate) fn without_system_copies() {
    with(|s| s.no_system_copies = true);
}

/// This build's installer, without the system's copies in a smoke run.
fn installer(no_system_copies: bool) -> Result<Installer, String> {
    let mut i = Installer::for_this_build()?;
    if no_system_copies {
        i.system.clear();
    }
    Ok(i)
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
    let ready = with(|s| {
        match (&s.status, &now) {
            (Status::Adding(_), _) | (Status::Failed(_), Status::Missing(_)) => {}
            _ => s.status = now,
        }
        s.looked = true;
        s.status == Status::Ready
    });
    publish();
    ready
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
            return None;
        }
        s.status = Status::Adding(Step::Looking);
        s.looked = true;
        Some(s.no_system_copies)
    });
    let Some(no_system_copies) = start else {
        return;
    };
    publish();
    cx.refresh_windows();
    let setting = crate::live::with(|l| l.web.url.clone()).flatten();
    let (tx, rx) = async_channel::unbounded::<Report>();
    let thread = std::thread::Builder::new()
        .name("dreamer-web-install".into())
        .spawn(move || {
            let result = installer(no_system_copies).and_then(|i| {
                let url = web_component::download_url(setting.as_deref(), &i.want.target);
                i.install(&Source::Find(url), &mut |step| {
                    let _ = tx.try_send(Report::Step(step));
                })
            });
            let _ = tx.try_send(Report::Done(result));
        });
    if let Err(e) = thread {
        with(|s| s.status = Status::Failed(format!("cannot start the install: {e}")));
        publish();
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
                publish();
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
/// `web.component`) add it or offer it once. With `never` it only looks,
/// for the daemon to hear that it is not there.
pub(crate) fn startup(cx: &mut App) {
    if !can_add() {
        return;
    }
    let no_system_copies = with(|s| s.no_system_copies);
    let (tx, rx) = async_channel::bounded::<(Status, bool)>(1);
    let looked = std::thread::Builder::new()
        .name("dreamer-web-look".into())
        .spawn(move || {
            let mut now = look();
            if now != Status::Ready
                && mode() != WebComponent::Never
                && let Ok(Some(_)) =
                    installer(no_system_copies).and_then(|i| i.use_system_copy(&mut |_| {}))
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
                s.looked = true;
            });
            publish();
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

/// Whether the first-start offer stands, and the status it shows.
pub(crate) fn offered() -> Option<Status> {
    let (shown, status) = with(|s| (s.offer, s.status.clone()));
    (shown && mode() == WebComponent::Ask && status != Status::Ready).then_some(status)
}

/// The first-start offer, for the tab strip, while it stands.
pub(crate) fn offer(t: &Theme) -> Option<gpui::Div> {
    let status = offered()?;
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

/// What a web pane waiting for the component shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InPane {
    /// Nothing: it is there, or this build cannot add it.
    Nothing,
    /// Why it cannot draw the page: the component is turned off.
    Off,
    /// Why it is missing, and the button that adds it.
    Add(String),
    /// The install's progress.
    Adding(Step),
    /// Why the install failed, and the button that tries again.
    Failed(String),
}

/// What a web pane waiting for the component shows now.
pub(crate) fn in_pane_state() -> InPane {
    if !can_add() {
        return InPane::Nothing;
    }
    match status() {
        Status::Ready => InPane::Nothing,
        Status::Missing(_) if mode() == WebComponent::Never => InPane::Off,
        Status::Missing(why) => InPane::Add(why.unwrap_or_else(|| {
            "Web pages need Dreamer Web (Chromium), which is added apart from Dreamer.".to_owned()
        })),
        Status::Adding(step) => InPane::Adding(step),
        Status::Failed(why) => InPane::Failed(why),
    }
}

/// The in-pane button's click: add the component, or try again.
pub(crate) fn press_add(cx: &mut App) {
    add(cx);
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
            .on_click(|_, _, cx| press_add(cx))
    };
    let col = div().flex().flex_col().items_start().gap_2().pt_1();
    let add_label = format!("Add Dreamer Web ({})", size_label());
    match in_pane_state() {
        InPane::Nothing => col,
        InPane::Off => col.child(div().text_color(c(t.muted)).child(
            "Web pages need Dreamer Web, which is turned off here (web.component: never in \
             dreamer.yaml). \"Add Dreamer Web\" in the palette adds it anyway.",
        )),
        InPane::Add(why) => col
            .child(div().text_color(c(t.muted)).child(why))
            .child(button(add_label)),
        InPane::Adding(step) => {
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
        InPane::Failed(why) => col
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
