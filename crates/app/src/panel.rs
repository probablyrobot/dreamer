//! Native panels (spec 12.2.1): an app package drawn by the desktop from
//! the same model the pane app draws (`app-model`). The model runs on a
//! thread of its own, with its own connection to the daemon; the panel
//! only sends it keys and draws what it published. No package code runs
//! on the UI thread or in the paint loop.
//!
//! The same view draws an app pane: a tmux pane whose program is a
//! registered app host. There the model is the host's own, as it published
//! it to the daemon (`apps.view`), and keys go back to the host
//! (`apps.input`). The desktop runs none of the package.
//!
//! What is drawn is the app's view tree: for an app pane, the tree its
//! host publishes (`apps.tree`, the whole tree and then keyed patches); for
//! a panel, or a host that publishes no tree, the tree of its model.
//! Scroll positions are kept per node key, so they survive updates.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use app_model::manifest::{ViewKind, field};
use app_model::{App, Endpoint, Input, text};
use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use view_tree::{Direction, Frame as Tree, Kind, Node};

use crate::theme::{self, c};

/// Rows drawn at most; a longer source shows the rows around the
/// selection (a log, its newest rows).
const MAX_ROWS: usize = 200;

pub(crate) struct AppPanel {
    pub(crate) path: PathBuf,
    /// The app pane drawn, and its app instance; `None` for a panel.
    pub(crate) pane: Option<(u32, String)>,
    /// The model as last published, and why the app ended, if it did.
    latest: Arc<Mutex<Option<App>>>,
    ended: Arc<Mutex<Option<String>>>,
    inputs: tokio::sync::mpsc::UnboundedSender<Input>,
    pub(crate) focus: FocusHandle,
    /// The model drawn last (smoke evidence).
    pub(crate) shown: Option<App>,
    /// An app pane's view tree as its host last published it.
    tree: Arc<Mutex<Option<Tree>>>,
    /// Per node key: scroll handles, and the selection last scrolled to.
    scrolls: HashMap<String, gpui::ScrollHandle>,
    scrolled_to: HashMap<String, Option<String>>,
}

impl std::fmt::Debug for AppPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppPanel")
            .field("path", &self.path)
            .finish()
    }
}

impl AppPanel {
    /// Start the package at `path` against the daemon at `socket`. The
    /// view is woken on every published model and when the app ends.
    pub(crate) fn start(path: PathBuf, socket: PathBuf, cx: &mut Context<Self>) -> AppPanel {
        let latest: Arc<Mutex<Option<App>>> = Arc::default();
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let (inputs, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, wake) = async_channel::bounded::<()>(1);
        let (l, e, p) = (latest.clone(), ended.clone(), path.clone());
        let spawned = std::thread::Builder::new()
            .name("dreamer-panel".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|rt| {
                        let endpoint = Endpoint {
                            socket,
                            role: "panel",
                            ticket: false,
                            narrow: true,
                        };
                        let mut publish = |app: &App| {
                            if let Ok(mut l) = l.lock() {
                                *l = Some(app.clone());
                            }
                            let _ = wake_tx.try_send(());
                            Ok(())
                        };
                        rt.block_on(app_model::run(p, endpoint, &mut rx, &mut publish))
                    });
                if let Ok(mut e) = e.lock() {
                    *e = Some(match result {
                        Ok(()) => "the app quit".into(),
                        Err(why) => why,
                    });
                }
                let _ = wake_tx.try_send(());
            });
        if let Err(why) = spawned
            && let Ok(mut e) = ended.lock()
        {
            *e = Some(format!("could not start: {why}"));
        }
        cx.spawn(async move |this, cx| {
            while wake.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
        AppPanel {
            path,
            pane: None,
            latest,
            ended,
            inputs,
            focus: cx.focus_handle(),
            shown: None,
            tree: Arc::default(),
            scrolls: HashMap::new(),
            scrolled_to: HashMap::new(),
        }
    }

    /// Draw the app pane `pane`, whose host is the app `instance`, from the
    /// views its host publishes through the daemon at `socket`. When the
    /// host stops, its last view stays, marked stale.
    pub(crate) fn follow(
        pane: u32,
        instance: String,
        socket: PathBuf,
        cx: &mut Context<Self>,
    ) -> AppPanel {
        let latest: Arc<Mutex<Option<App>>> = Arc::default();
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let (inputs, rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, wake) = async_channel::bounded::<()>(1);
        let tree: Arc<Mutex<Option<Tree>>> = Arc::default();
        let (l, e, tr) = (latest.clone(), ended.clone(), tree.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("dreamer-app-pane-{pane}"))
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let wake = move || {
                    let _ = wake_tx.try_send(());
                };
                rt.block_on(follow_pane(pane, socket, rx, l, e, tr, wake));
            });
        if let Err(why) = spawned
            && let Ok(mut e) = ended.lock()
        {
            *e = Some(format!("could not start: {why}"));
        }
        cx.spawn(async move |this, cx| {
            while wake.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
        AppPanel {
            path: PathBuf::new(),
            pane: Some((pane, instance)),
            latest,
            ended,
            inputs,
            focus: cx.focus_handle(),
            shown: None,
            tree,
            scrolls: HashMap::new(),
            scrolled_to: HashMap::new(),
        }
    }

    /// Why the app ended, once it has.
    pub(crate) fn ended(&self) -> Option<String> {
        self.ended.lock().ok().and_then(|e| e.clone())
    }

    /// Send a key by the model's names (`a`, `Enter`, `Up`, `Ctrl-c`).
    pub(crate) fn key(&self, name: &str) {
        let _ = self.inputs.send(Input::Key(name.to_owned()));
    }

    /// Copy what the panel has selected: the selected row (a table's
    /// columns separated by tabs), the focused form field, or an agent
    /// view's latest answer.
    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.shown.as_ref().and_then(copy_text) {
            crate::to_tmux_buffer(&text);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    /// Type the clipboard's text into the focused field or prompt, one
    /// character at a time as the model takes keys; line breaks become
    /// spaces, other control characters are dropped.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        for k in paste_keys(&text) {
            self.key(&k);
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = key_name(&ev.keystroke) {
            self.key(&name);
            cx.stop_propagation();
        }
    }
}

impl Drop for AppPanel {
    fn drop(&mut self) {
        let _ = self.inputs.send(Input::Quit);
    }
}

/// Follow an app pane's published views until `inputs` closes or says
/// Quit (the view went away; the host keeps running), and send the host
/// the keys from `inputs`, in order. `ended` says why the view is stale,
/// while it is. Its view tree is followed beside the model, as patches
/// to the copy in `tree`; a daemon without trees leaves `tree` empty.
async fn follow_pane(
    pane: u32,
    socket: PathBuf,
    mut inputs: tokio::sync::mpsc::UnboundedReceiver<Input>,
    latest: Arc<Mutex<Option<App>>>,
    ended: Arc<Mutex<Option<String>>>,
    tree: Arc<Mutex<Option<Tree>>>,
    wake: impl Fn(),
) {
    use std::time::Duration;
    let set_ended = |why: Option<String>| {
        if let Ok(mut e) = ended.lock()
            && *e != why
        {
            *e = why;
            wake();
        }
    };
    let mut cache: app_model::ManifestCache = None;
    let mut backoff = Duration::from_millis(200);
    loop {
        let c = match client::Client::connect(&socket, "app-pane").await {
            Ok(c) => Arc::new(c),
            Err(e) => {
                set_ended(Some(format!("no daemon: {e}")));
                tokio::select! {
                    i = inputs.recv() => if matches!(i, None | Some(Input::Quit)) { return },
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        backoff = Duration::from_millis(200);
        // Keys go on their own task, one call after another, so they keep
        // their order and never wait for a view.
        let (keys, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sender = c.clone();
        let key_task = tokio::spawn(async move {
            while let Some(key) = key_rx.recv().await {
                let _ = sender
                    .call(api::Request::AppsInput(api::AppInput { pane, key }))
                    .await;
            }
        });
        let next = |after: u64| {
            let c = c.clone();
            Box::pin(async move {
                c.call(api::Request::AppsView(api::AppViewQuery {
                    pane,
                    after,
                    wait_ms: None,
                }))
                .await
            })
        };
        let mut view = next(0);
        let next_tree = |after: u64, seq: u64| {
            let c = c.clone();
            Box::pin(async move {
                c.call(api::Request::AppsTree(api::AppTreeQuery {
                    pane,
                    after,
                    seq,
                    wait_ms: None,
                }))
                .await
            })
        };
        let mut mirror = view_tree::Mirror::default();
        let mut trees = next_tree(0, 0);
        let mut follow_trees = true;
        let lost = loop {
            tokio::select! {
                i = inputs.recv() => match i {
                    None | Some(Input::Quit) => {
                        key_task.abort();
                        return;
                    }
                    Some(Input::Redraw) => {}
                    Some(Input::Key(key)) => {
                        let _ = keys.send(key);
                    }
                },
                r = &mut view => match r {
                    Ok(api::Response::AppsView(v)) => {
                        view = next(v.revision);
                        let read = (!v.view.is_empty())
                            .then(|| App::from_published(&v.view, &mut cache))
                            .transpose();
                        match read {
                            Ok(app) => {
                                if let Some(app) = app
                                    && let Ok(mut l) = latest.lock()
                                {
                                    *l = Some(app);
                                }
                                wake();
                                set_ended((!v.live).then(|| "the app's host stopped".to_owned()));
                            }
                            Err(e) => set_ended(Some(format!("unreadable view: {e}"))),
                        }
                    }
                    Ok(_) => view = next(0),
                    Err(e) => break e.to_string(),
                },
                r = &mut trees, if follow_trees => match r {
                    Ok(api::Response::AppsTree(t)) => {
                        let whole = !t.tree.is_empty();
                        match mirror.take(t.seq, &t.tree, &t.patches) {
                            Ok(changed) => {
                                if changed && let Ok(mut l) = tree.lock() {
                                    l.clone_from(&mirror.frame);
                                }
                                if changed {
                                    wake();
                                }
                                trees = next_tree(t.revision, mirror.seq);
                            }
                            // An unreadable whole tree: draw the model.
                            Err(_) if whole => follow_trees = false,
                            // Lost track: ask for the whole tree.
                            Err(_) => trees = next_tree(0, 0),
                        }
                    }
                    Ok(_) => trees = next_tree(0, mirror.seq),
                    // A daemon without trees, or none for this pane yet.
                    Err(client::ClientError::Rpc(_)) => follow_trees = false,
                    Err(e) => break e.to_string(),
                },
            }
        };
        key_task.abort();
        set_ended(Some(lost));
        tokio::select! {
            i = inputs.recv() => if matches!(i, None | Some(Input::Quit)) { return },
            () = tokio::time::sleep(backoff) => {}
        }
    }
}

/// A GPUI keystroke as the model names keys; `None` for keys the desktop
/// keeps (Cmd shortcuts) or that have no name.
fn key_name(k: &gpui::Keystroke) -> Option<String> {
    let m = &k.modifiers;
    if m.platform || m.alt {
        return None;
    }
    let named = match k.key.as_str() {
        "enter" => "Enter",
        "escape" => "Esc",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "space" if !m.control => " ",
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            return Some(f.to_uppercase());
        }
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_owned());
    }
    let mut chars = k.key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if m.control => Some(format!("Ctrl-{}", ch.to_ascii_lowercase())),
        _ if m.control => None,
        _ => k.key_char.clone().filter(|t| t.chars().count() == 1),
    }
}

/// The text Copy takes from a panel's model, if any.
fn copy_text(app: &App) -> Option<String> {
    let v = &app.manifest.view;
    match v.kind {
        ViewKind::Form => app.form.get(app.focus).cloned(),
        ViewKind::Agent => app
            .agent
            .transcript
            .iter()
            .rev()
            .find(|e| e.who == app_model::Said::Agent)
            .map(|e| e.text.clone()),
        ViewKind::Table if !v.columns.is_empty() => {
            let row = app.rows.get(app.selected)?;
            Some(
                v.columns
                    .iter()
                    .map(|c| field(row, c).map(text).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\t"),
            )
        }
        _ => {
            let row = app.rows.get(app.selected)?;
            Some(match &v.field {
                Some(p) => field(row, p).map(text).unwrap_or_default(),
                None => text(row),
            })
        }
    }
    .filter(|t| !t.is_empty())
}

/// Pasted text as the keys the model takes: one per character, line
/// breaks as spaces, other control characters dropped.
fn paste_keys(text: &str) -> Vec<String> {
    text.chars()
        .filter_map(|ch| match ch {
            '\r' | '\n' | '\t' => Some(" ".to_owned()),
            c if c.is_control() => None,
            c => Some(c.to_string()),
        })
        .collect()
}

impl Render for AppPanel {
    fn render(&mut self, window_: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::for_window(window_);
        let latest = self.latest.lock().ok().and_then(|l| l.clone());
        if latest.is_some() {
            self.shown = latest;
        }
        let ended = self.ended();
        let focused = self.focus.is_focused(window_);
        let root = div()
            .id("app-panel")
            .track_focus(&self.focus)
            .on_action(cx.listener(|p, _: &crate::keymap::Copy, _, cx| p.copy(cx)))
            .on_action(cx.listener(|p, _: &crate::keymap::Paste, _, cx| p.paste(cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_click(cx.listener(|p, _, window, cx| {
                window.focus(&p.focus, cx);
            }))
            .h_full()
            .flex()
            .flex_col()
            .bg(c(t.surface))
            .map(|d| match self.pane {
                // An app pane fills its rectangle; the workspace marks focus.
                Some(_) => d.size_full(),
                None => d.w(px(360.0)).border_l_1().border_color(c(if focused {
                    t.focus
                } else {
                    t.divider
                })),
            })
            .text_size(px(12.0))
            .text_color(c(t.text));
        let tree = match self.pane {
            Some(_) => self.tree.lock().ok().and_then(|t| t.clone()),
            None => None,
        };
        let Some(app) = self.shown.as_ref() else {
            return root.child(
                div()
                    .p_2()
                    .text_color(c(t.muted))
                    .child(ended.unwrap_or_else(|| "starting…".into())),
            );
        };
        // The host's tree; else the model's (a panel, or a host that
        // publishes none); an agentic pane has no tree.
        let tree = tree.or_else(|| app.frame());
        let (title, status, content) = match &tree {
            Some(tree) => (
                tree.title.clone(),
                tree.status.clone(),
                node(
                    &tree.root,
                    tree,
                    &t,
                    &mut self.scrolls,
                    &mut self.scrolled_to,
                ),
            ),
            None => {
                let title = if app.version > 1 {
                    format!("{} (reloaded {})", app.manifest.title, app.version - 1)
                } else {
                    app.manifest.title.clone()
                };
                (title, app.status_text(), body(app, &t))
            }
        };
        let status = match (&ended, &self.pane) {
            (Some(why), Some(_)) => format!("stale: {why}"),
            (Some(why), None) => format!("ended: {why}"),
            (None, _) => status,
        };
        root.child(
            div()
                .px_2()
                .py_1()
                .text_color(c(t.muted))
                .child(SharedString::from(title)),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .px_2()
                .child(content),
        )
        .child(
            div()
                .px_2()
                .py_1()
                .text_color(c(t.faint))
                .child(SharedString::from(status)),
        )
    }
}

/// An agentic pane: who it is, what it may do, its steps, its
/// transcript, and the prompt.
fn body(app: &App, t: &theme::Theme) -> gpui::AnyElement {
    {
        use app_model::Said;
        let a = &app.agent;
        let kind = app
            .manifest
            .agent
            .as_ref()
            .map_or("agent", |s| s.kind.name());
        let activity = if a.running {
            "working (Esc cancels)"
        } else {
            "idle"
        };
        let newest = a.transcript.len().saturating_sub(MAX_ROWS);
        div()
            .flex()
            .flex_col()
            .h_full()
            .gap_1()
            .child(
                div()
                    .text_color(c(t.muted))
                    .child(SharedString::from(format!(
                        "{kind} · {} · {activity}",
                        a.mode
                    ))),
            )
            .child(
                div()
                    .text_color(c(t.faint))
                    .child(SharedString::from(format!("grant: {}", a.grant))),
            )
            // An operator's plan and every step it took, with its state.
            .children(a.plan.as_ref().map(|plan| {
                div()
                    .italic()
                    .text_color(c(t.muted))
                    .child(SharedString::from(format!("plan: {plan}")))
            }))
            .children(a.taken.iter().map(|s| {
                let (mark, color) = match (s.ok, s.open) {
                    (false, _) => ("✗", t.danger),
                    (true, true) => ("…", t.warning),
                    (true, false) => ("✓", t.accent),
                };
                let host = s
                    .host
                    .as_deref()
                    .map(|h| format!(" @{h}"))
                    .unwrap_or_default();
                div().text_color(c(color)).child(SharedString::from(format!(
                    "{mark} {} {}{host} → {}",
                    s.tool, s.target, s.state
                )))
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .children(a.transcript[newest..].iter().map(|e| {
                        let (mark, color) = match e.who {
                            Said::You => ("> ", t.text),
                            Said::Agent => ("", t.text),
                            Said::Tool => ("→ ", t.accent),
                            Said::Result { ok: true } => ("← ", t.faint),
                            Said::Result { ok: false } => ("← failed: ", t.danger),
                            Said::Note => ("· ", t.faint),
                            Said::Failed => ("✗ ", t.danger),
                        };
                        div()
                            .text_color(c(color))
                            .child(SharedString::from(format!("{mark}{}", e.text)))
                    })),
            )
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(c(t.selected))
                    .child(SharedString::from(format!("> {}▏", a.input))),
            )
            .into_any_element()
    }
}

/// A row of cells, highlighted when selected.
fn line(t: &theme::Theme, selected: bool) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .px_1()
        .rounded_sm()
        .when(selected, |d| d.bg(c(t.selected)))
}

fn cell(s: String) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(SharedString::from(s))
}

/// The items shown: up to `MAX_ROWS`, around the selected one (a log, its
/// newest), with the index of the first.
fn shown_items(n: &Node) -> (usize, &[Node]) {
    let len = n.children.len();
    let anchor = match n.kind {
        Kind::Log => len,
        _ => n.selected_index().unwrap_or(0) + MAX_ROWS / 2,
    };
    let end = anchor.clamp(MAX_ROWS.min(len), len);
    let start = end.saturating_sub(MAX_ROWS);
    (start, &n.children[start..end])
}

/// A node of the view tree, drawn natively. Scroll handles are kept in
/// `scrolls` by node key, so a new tree keeps each position; a table or
/// list scrolls to its selection when the selection moves.
fn node(
    n: &Node,
    tree: &Tree,
    t: &theme::Theme,
    scrolls: &mut HashMap<String, gpui::ScrollHandle>,
    scrolled_to: &mut HashMap<String, Option<String>>,
) -> gpui::AnyElement {
    let focused = tree.focus.as_deref() == Some(n.key.as_str());
    let id = SharedString::from(format!("app-node-{}", n.key));
    let mut child = |c: &Node| node(c, tree, t, scrolls, scrolled_to);
    let content = match &n.kind {
        Kind::Column | Kind::Scroll => {
            let d = div()
                .flex()
                .flex_col()
                .gap_1()
                .size_full()
                .children(n.children.iter().map(|c| {
                    let fixed = matches!(c.kind, Kind::Text { .. } | Kind::Gauge { .. })
                        || n.kind == Kind::Scroll;
                    div()
                        .min_h_0()
                        .when(fixed, |d| d.flex_none())
                        .when(!fixed, |d| d.flex_1())
                        .child(child(c))
                }));
            if n.kind == Kind::Scroll {
                let handle = scrolls.entry(n.key.clone()).or_default();
                d.id(id)
                    .overflow_y_scroll()
                    .track_scroll(handle)
                    .into_any_element()
            } else {
                d.into_any_element()
            }
        }
        Kind::Row => div()
            .flex()
            .flex_row()
            .gap_2()
            .size_full()
            .children(
                n.children
                    .iter()
                    .map(|c| div().flex_1().min_w_0().h_full().child(child(c))),
            )
            .into_any_element(),
        Kind::Split { direction, sizes } => {
            let total: f32 = if sizes.len() == n.children.len() {
                sizes.iter().map(|s| f32::from(*s)).sum()
            } else {
                0.0
            };
            let count = n.children.len().max(1) as f32;
            let row = *direction == Direction::Row;
            div()
                .flex()
                .when(row, |d| d.flex_row())
                .when(!row, |d| d.flex_col())
                .size_full()
                .children(n.children.iter().enumerate().map(|(i, kid)| {
                    let share = if total > 0.0 {
                        f32::from(sizes[i]) / total
                    } else {
                        1.0 / count
                    };
                    div()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .border_color(c(t.divider))
                        .when(row, |d| d.h_full().w(gpui::relative(share)).px_1())
                        .when(!row, |d| d.w_full().h(gpui::relative(share)).py_1())
                        .when(row && i > 0, |d| d.border_l_1())
                        .when(!row && i > 0, |d| d.border_t_1())
                        .child(child(kid))
                }))
                .into_any_element()
        }
        Kind::Tabs { active } => div()
            .flex()
            .flex_col()
            .gap_1()
            .size_full()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children(n.children.iter().enumerate().map(|(i, page)| {
                        line(t, i == *active)
                            .text_color(c(if i == *active { t.text } else { t.muted }))
                            .child(SharedString::from(
                                page.title.clone().unwrap_or_else(|| page.key.clone()),
                            ))
                    })),
            )
            .children(n.shown().first().map(|page| {
                // The page's own title is its tab's name.
                let mut page = page.clone();
                page.title = None;
                div().flex_1().min_h_0().child(child(&page))
            }))
            .into_any_element(),
        Kind::Table { .. } | Kind::List { .. } | Kind::Log => {
            let (start, items) = shown_items(n);
            let selected = n.selected_index();
            let handle = scrolls.entry(n.key.clone()).or_default().clone();
            // Scroll to the selection only when it moved, so the position
            // a reader scrolled to survives updates.
            let want = selected
                .and_then(|i| n.children.get(i))
                .map(|c| c.key.clone());
            if let Some(i) = selected
                && scrolled_to.get(&n.key) != Some(&want)
            {
                handle.scroll_to_item(i.saturating_sub(start));
            }
            scrolled_to.insert(n.key.clone(), want);
            let log = n.kind == Kind::Log;
            let rows = div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .when(log, |d| d.justify_end().overflow_hidden())
                .when(!log, |d| d.overflow_y_scroll().track_scroll(&handle))
                .children(items.iter().enumerate().map(|(i, r)| {
                    line(t, !log && Some(start + i) == selected)
                        .children(r.cells().iter().map(|s| cell(s.clone())))
                }));
            div()
                .flex()
                .flex_col()
                .size_full()
                .when_some(
                    match &n.kind {
                        Kind::Table { columns, .. } => Some(columns),
                        _ => None,
                    },
                    |d, columns| {
                        d.child(
                            line(t, false)
                                .text_color(c(t.muted))
                                .children(columns.iter().map(|h| cell(h.clone()))),
                        )
                    },
                )
                .child(rows)
                .into_any_element()
        }
        Kind::Item { cells } => line(t, false)
            .children(cells.iter().map(|s| cell(s.clone())))
            .into_any_element(),
        Kind::Text { text } => div()
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Kind::Gauge { ratio, label } => div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .h(px(8.0))
                    .w_full()
                    .rounded_sm()
                    .bg(c(t.selected))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(ratio.clamp(0.0, 1.0) as f32))
                            .bg(c(t.accent)),
                    ),
            )
            .child(SharedString::from(label.clone()))
            .into_any_element(),
        Kind::Chart { values } => {
            let top = values.iter().copied().fold(0.0, f64::max).max(1.0);
            div()
                .h(px(80.0))
                .flex()
                .flex_row()
                .items_end()
                .gap(px(1.0))
                .children(values.iter().rev().take(MAX_ROWS).rev().map(|v| {
                    div()
                        .flex_1()
                        .h(gpui::relative((v.max(0.0) / top) as f32))
                        .bg(c(t.accent))
                }))
                .into_any_element()
        }
        Kind::Form { fields, focus } => div()
            .flex()
            .flex_col()
            .gap_1()
            .children(fields.iter().enumerate().map(|(i, f)| {
                let shown = match (f.toggle, f.value.as_str()) {
                    (true, "true") => "[x]".to_owned(),
                    (true, _) => "[ ]".to_owned(),
                    (false, v) => v.to_owned(),
                };
                let star = if f.required { "*" } else { "" };
                line(t, focused && i == *focus)
                    .child(
                        div()
                            .text_color(c(t.muted))
                            .child(format!("{}{star}", f.label)),
                    )
                    .child(cell(shown))
            }))
            .into_any_element(),
    };
    match &n.title {
        Some(title) => div()
            .flex()
            .flex_col()
            .size_full()
            .border_1()
            .rounded_sm()
            .border_color(c(if focused { t.focus } else { t.divider }))
            .child(
                div()
                    .px_1()
                    .text_color(c(t.muted))
                    .child(SharedString::from(title.clone())),
            )
            .child(div().flex_1().min_h_0().px_1().child(content))
            .into_any_element(),
        None => content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(s: &'static str) -> gpui::Keystroke {
        gpui::Keystroke::parse(s).expect("keystroke")
    }

    #[test]
    fn pasted_text_becomes_one_key_per_character() {
        assert_eq!(
            paste_keys("a b\r\nc\u{7}é"),
            ["a", " ", "b", " ", " ", "c", "é"]
        );
    }

    #[test]
    fn keys_get_the_names_the_model_uses() {
        let name = |s: &'static str| key_name(&keystroke(s));
        assert_eq!(name("enter").as_deref(), Some("Enter"));
        assert_eq!(name("down").as_deref(), Some("Down"));
        assert_eq!(name("ctrl-c").as_deref(), Some("Ctrl-c"));
        assert_eq!(name("f2").as_deref(), Some("F2"));
        assert_eq!(name("space").as_deref(), Some(" "));
        // The desktop's own shortcuts stay with the desktop.
        assert_eq!(name("cmd-shift-p"), None);
    }
}
