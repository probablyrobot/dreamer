//! Native panels (spec 12.2.1): an app package drawn by the desktop from
//! the same model the pane app draws (`app-model`). The model runs on a
//! thread of its own, with its own connection to the daemon; the panel
//! only sends it keys and draws what it published. No package code runs
//! on the UI thread or in the paint loop.
//!
//! The same view draws an app pane: a tmux pane whose program is a
//! registered app host. There the model is the host's own, as it published
//! it to the daemon (`apps.view`), and input goes back to the host
//! (`apps.input`). The desktop runs none of the package.
//!
//! What is drawn is the app's view tree: for an app pane, the tree its
//! host publishes (`apps.tree`, the whole tree and then keyed patches); for
//! a panel, or a host that publishes no tree, the tree of its model.
//! Scroll positions are kept per node key, so they survive updates.
//!
//! Bound nodes (a `transcript` or `composer` bound to an agent session, a
//! `log` bound to a tap) are drawn from streams the panel follows itself
//! ([`app_model::bound`]): for an app pane, on the desktop's connection
//! with `for_pane`, so the daemon checks the app's grant too; for a panel,
//! by its model on the model's narrowed connection. Streamed tokens reach
//! the paint without passing through the app.
//!
//! Input follows the desktop's one policy (`input::app_intent`): special
//! keys and Ctrl combinations are keys by the model's names; text comes
//! through the platform's text input (so IME commits arrive as text) and
//! goes into the focused input as one edit, or as keys in a view with no
//! input; Cmd-C / Ctrl-Shift-C copies the mouse-selected lines, else the
//! focused node's selected row or field; Cmd-V / Ctrl-Shift-V and a middle
//! click paste into the focused input only. Clicks and wheel steps name
//! the tree node they land on, so they reach that node. In an app pane,
//! tmux's prefix works as in a terminal pane: the key after it runs tmux's
//! binding.
//!
//! A `terminal` node is drawn with the terminal renderer from a replica of
//! its pane, followed on a thread of its own, read only: keys never go to
//! the pane. A node the app host marked refused (its pane is outside the
//! app's grant) shows the refusal and follows nothing. An `image` node's
//! file, as the host resolved it in the app's package, is read and decoded
//! off the UI thread and drawn once it is ready.
//!
//! A `web` node of an app pane is the app's page, drawn by Chromium in a
//! web view of its own ([`crate::web`]) placed where the node is. The
//! page's bridge calls go to the daemon as `apps.call` for this pane, so
//! they run as the app, under its grant. The page of an app whose whole
//! view is a page takes the keys while the pane has the focus. A panel
//! shows a `web` node's address only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use app_model::{App, Endpoint, Input};
use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollEvent, SharedString,
    StatefulInteractiveElement, Styled, UTF16Selection, Window, div, prelude::FluentBuilder, px,
};
use replica::feed::Feed;
use terminal_ghostty::core::FrameSnapshot;
use view_tree::rich::{self, Block as RichBlock, Role};
use view_tree::{Direction, Frame as Tree, Kind, Node, Tone};

use crate::input::{self, AppKey};
use crate::theme::{self, c, ca};

mod bound;

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
    /// An app pane's view tree as its host last published it, and its
    /// version (0 before the first).
    tree: Arc<Mutex<Option<Tree>>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
    /// The version of the tree in the frame drawn last (a capture waits
    /// for the version it was asked for).
    pub(crate) painted: u64,
    /// Per node key: scroll handles, and the selection last scrolled to.
    scrolls: HashMap<String, gpui::ScrollHandle>,
    scrolled_to: HashMap<String, Option<String>>,
    /// tmux's prefix was pressed in this app pane: the next key is tmux's.
    pub(crate) prefixed: bool,
    /// Lines selected with the mouse, in one node of the tree; and whether
    /// the button is still down.
    pub(crate) lines: Option<LineSel>,
    dragging: bool,
    /// The part of a wheel step not yet sent.
    wheel: f32,
    /// IME composition in progress: shown, never sent.
    preedit: Option<String>,
    /// The streams bound nodes draw, and calls made for the app (an
    /// approval answered from a card).
    pub(crate) streams: app_model::bound::Streams,
    calls: tokio::sync::mpsc::UnboundedSender<api::Request>,
    /// Each transcript's scroll, following the newest.
    follow: std::cell::RefCell<HashMap<String, bound::Follow>>,
    /// The daemon `terminal` nodes' panes are followed through.
    socket: PathBuf,
    /// The panes `terminal` nodes show, each followed by a feed of its own,
    /// with the newest frame painted.
    embedded: HashMap<u32, Embedded>,
    /// `image` nodes' pictures, by the file the host resolved.
    pictures: HashMap<String, Picture>,
    /// `web` nodes' pages.
    pages: Pages,
}

/// The pages of an app pane's `web` nodes.
#[derive(Default)]
struct Pages {
    /// Each shown `web` node's view, by the node's key.
    views: HashMap<String, Entity<crate::web::WebPane>>,
    /// Where the pages' bridge calls go (an app pane's only).
    bridge: Option<tokio::sync::mpsc::UnboundedSender<web_engine::BridgeCall>>,
    workspace: Option<gpui::WeakEntity<crate::workspace::Workspace>>,
}

/// A pane a `terminal` node shows.
struct Embedded {
    feed: Arc<Feed>,
    frame: Option<Arc<FrameSnapshot>>,
}

/// An image file, read and decoded off the UI thread.
#[derive(Clone)]
enum Picture {
    Loading,
    Ready(Arc<gpui::RenderImage>),
    Failed(String),
}

/// Read and decode the image file `path`: a PNG or JPEG of at most
/// [`view_tree::image::MAX_BYTES`] and [`view_tree::image::MAX_SIDE`]
/// pixels a side. Slow: never on the UI thread.
fn load_picture(path: &str) -> Picture {
    use view_tree::image::{MAX_BYTES, MAX_SIDE, sniff};
    let read = || -> Result<Arc<gpui::RenderImage>, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > MAX_BYTES {
            return Err(format!("larger than {} MiB", MAX_BYTES / (1024 * 1024)));
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let (_, w, h) = sniff(&bytes).ok_or("not a PNG or JPEG")?;
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err(format!("{w}x{h} is too large"));
        }
        terminal_render::decode_image(&bytes)
    };
    match read() {
        Ok(image) => Picture::Ready(image),
        Err(why) => Picture::Failed(why),
    }
}

/// Lines selected with the mouse: the node they are in (by key; `AGENT`
/// for an agentic pane's transcript), where the press was, where the
/// pointer is, and whether a click there picks (a table's or list's row,
/// a form's field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineSel {
    pub(crate) node: String,
    pub(crate) from: usize,
    pub(crate) to: usize,
    pick: bool,
}

impl LineSel {
    fn range(&self) -> std::ops::RangeInclusive<usize> {
        self.from.min(self.to)..=self.from.max(self.to)
    }
}

/// The node key of an agentic pane's transcript, which has no tree. Tree
/// keys are never empty.
const AGENT: &str = "";

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
        let streams = app_model::bound::Streams::default();
        let (l, e, p, st) = (latest.clone(), ended.clone(), path.clone(), streams.clone());
        let daemon = socket.clone();
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
                        rt.block_on(app_model::run_with(p, endpoint, &mut rx, &mut publish, &st))
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
            seq: Arc::default(),
            painted: 0,
            scrolls: HashMap::new(),
            scrolled_to: HashMap::new(),
            prefixed: false,
            lines: None,
            dragging: false,
            wheel: 0.0,
            preedit: None,
            streams,
            // A panel's model answers for itself (a click on the node).
            calls: tokio::sync::mpsc::unbounded_channel().0,
            follow: Default::default(),
            socket: daemon,
            embedded: HashMap::new(),
            pictures: HashMap::new(),
            pages: Pages::default(),
        }
    }

    /// Draw the app pane `pane`, whose host is the app `instance`, from the
    /// views its host publishes through the daemon at `socket`. When the
    /// host stops, its last view stays, marked stale.
    pub(crate) fn follow(
        pane: u32,
        instance: String,
        socket: PathBuf,
        workspace: gpui::WeakEntity<crate::workspace::Workspace>,
        cx: &mut Context<Self>,
    ) -> AppPanel {
        let latest: Arc<Mutex<Option<App>>> = Arc::default();
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let (inputs, rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, wake) = async_channel::bounded::<()>(1);
        let tree: Arc<Mutex<Option<Tree>>> = Arc::default();
        let streams = app_model::bound::Streams::default();
        let (calls, calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let (bridge, bridge_rx) = tokio::sync::mpsc::unbounded_channel();
        let seq: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        let (l, e, tr, st, sq) = (
            latest.clone(),
            ended.clone(),
            tree.clone(),
            streams.clone(),
            seq.clone(),
        );
        let daemon = socket.clone();
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
                let shared = Shared {
                    latest: l,
                    ended: e,
                    tree: tr,
                    streams: st,
                    seq: sq,
                };
                let channels = (rx, calls_rx, bridge_rx);
                rt.block_on(follow_pane(pane, socket, channels, shared, wake));
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
            seq,
            painted: 0,
            scrolls: HashMap::new(),
            scrolled_to: HashMap::new(),
            prefixed: false,
            lines: None,
            dragging: false,
            wheel: 0.0,
            preedit: None,
            streams,
            calls,
            follow: Default::default(),
            socket: daemon,
            embedded: HashMap::new(),
            pictures: HashMap::new(),
            pages: Pages {
                views: HashMap::new(),
                bridge: Some(bridge),
                workspace: Some(workspace),
            },
        }
    }

    /// Give each shown `web` node of `tree` a web view of its own (in an
    /// app pane), and let the others' go, closing their pages.
    fn sync_pages(&mut self, tree: &Tree, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((pane, _)), Some(bridge), Some(ws)) = (
            self.pane.as_ref(),
            self.pages.bridge.as_ref(),
            self.pages.workspace.as_ref(),
        ) else {
            return;
        };
        let mut wanted = Vec::new();
        pages_in(&tree.root, &mut wanted);
        self.pages.views.retain(|key, v| {
            let url = &v.read(cx).url;
            wanted
                .iter()
                .any(|n| n.key == *key && page_url(n) == Some(url))
        });
        for n in wanted {
            let Kind::Web { page: p, .. } = &n.kind else {
                continue;
            };
            if self.pages.views.contains_key(&n.key) {
                continue;
            }
            let tx = bridge.clone();
            let page = web_engine::AppPage {
                app: p.app.clone(),
                origin: p.origin.clone(),
                dir: (!p.dir.is_empty()).then(|| PathBuf::from(&p.dir)),
                allow: p.allow.clone(),
                bridge: Arc::new(move |call: web_engine::BridgeCall| {
                    if let Err(e) = tx.send(call) {
                        let why = "the app pane has closed".to_owned();
                        (e.0.reply)(Err((api::code::UNAVAILABLE, why)));
                    }
                }),
            };
            let (pane, ws, url) = (*pane, ws.clone(), p.url.clone());
            let view = gpui::AppContext::new(cx, |cx| {
                crate::web::WebPane::new(pane, url, Default::default(), Some(page), ws, window, cx)
            });
            self.pages.views.insert(n.key.clone(), view);
        }
    }

    /// The views of this pane's pages.
    pub(crate) fn pages(&self) -> Vec<Entity<crate::web::WebPane>> {
        self.pages.views.values().cloned().collect()
    }

    /// The page that is this app's whole view (a web app), if it is one.
    pub(crate) fn root_page(&self) -> Option<Entity<crate::web::WebPane>> {
        let root = self
            .tree
            .lock()
            .ok()
            .and_then(|t| t.as_ref().map(|t| t.root.key.clone()))?;
        self.pages.views.get(&root).cloned()
    }

    /// Where the keys go when the pane has the focus: the page of an app
    /// whose view is a page; else the app.
    pub(crate) fn keys_to(&self, cx: &gpui::App) -> FocusHandle {
        self.root_page()
            .map_or_else(|| self.focus.clone(), |v| v.read(cx).focus.clone())
    }

    /// Follow the panes `tree`'s `terminal` nodes show (not refused ones,
    /// and only on shown tabs), and stop following the others; start
    /// reading and decoding the images it shows that are new. Each feed
    /// wakes the view when its pane has a new frame.
    fn sync_embeds(&mut self, tree: &Tree, cx: &mut Context<Self>) {
        let wanted = tree.panes();
        self.embedded.retain(|p, _| wanted.contains(p));
        for pane in wanted {
            if self.embedded.contains_key(&pane) {
                continue;
            }
            let Ok(feed) = Feed::start(self.socket.clone(), Some(pane), None) else {
                continue;
            };
            let wake = feed.wake.clone();
            self.embedded.insert(
                pane,
                Embedded {
                    feed: Arc::new(feed),
                    frame: None,
                },
            );
            cx.spawn(async move |this, cx| {
                while wake.recv().await.is_ok() {
                    let alive = this.update(cx, |p, cx| {
                        if let Some(e) = p.embedded.get_mut(&pane)
                            && let Some(f) = e.feed.take()
                        {
                            e.frame = Some(f);
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            })
            .detach();
        }
        let mut wanted_pictures = Vec::new();
        images_in(&tree.root, &mut wanted_pictures);
        self.pictures.retain(|p, _| wanted_pictures.contains(p));
        for path in wanted_pictures {
            if self.pictures.contains_key(&path) {
                continue;
            }
            self.pictures.insert(path.clone(), Picture::Loading);
            let task = cx.background_executor().spawn({
                let path = path.clone();
                async move { load_picture(&path) }
            });
            cx.spawn(async move |this, cx| {
                let picture = task.await;
                let _ = this.update(cx, |p, cx| {
                    if let Some(slot) = p.pictures.get_mut(&path) {
                        *slot = picture;
                        cx.notify();
                    }
                });
            })
            .detach();
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

    /// An approval card's button: in an app pane, answered here for the
    /// app (`for_pane`: the daemon checks the app's grant allows it); in a
    /// panel, a click its model answers on its own connection.
    fn answer(&self, a: bound::Answering) {
        if let Some((pane, _)) = &self.pane {
            let p = api::AgentSessionAnswer {
                session: a.session,
                approval: a.approval,
                option: a.option.map(|(_, o)| o),
                for_pane: Some(*pane),
            };
            let _ = self.calls.send(if a.allow {
                api::Request::AgentSessionsApprove(p)
            } else {
                api::Request::AgentSessionsDeny(p)
            });
            return;
        }
        let index = match (a.allow, a.option) {
            (false, _) => app_model::session::CLICK_DENY,
            (true, o) => app_model::session::CLICK_ALLOW + o.map_or(0, |(i, _)| i),
        };
        let _ = self.inputs.send(Input::Select {
            node: Some(a.node),
            index,
        });
    }

    /// The tree drawn: an app pane's host's, else the model's (a panel,
    /// or a host that publishes none); `None` for an agentic pane.
    fn drawn_tree(&self) -> Option<Tree> {
        let host = match self.pane {
            Some(_) => self.tree.lock().ok().and_then(|t| t.clone()),
            None => None,
        };
        host.or_else(|| self.shown.as_ref().and_then(App::frame))
    }

    /// Whether the view drawn has a focused input that takes text: the
    /// tree's focused form field, unless it is a toggle; or an agent's
    /// prompt.
    fn takes_text(&self) -> bool {
        match self.drawn_tree() {
            Some(tree) => tree_takes_text(&tree),
            None => self.shown.as_ref().is_some_and(App::takes_text),
        }
    }

    /// Text typed (committed by the platform's text input): into the
    /// focused input as one edit, or, in a view with no input, as the keys
    /// it is made of (a table's command keys, `q`).
    pub(crate) fn typed(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.takes_text() {
            let _ = self.inputs.send(Input::Text(text.to_owned()));
        } else {
            for ch in text.chars().filter(|c| !c.is_control()) {
                self.key(&ch.to_string());
            }
        }
    }

    /// Pasted text: into the focused input as one edit. A view with no
    /// input takes none, so a paste never runs a command.
    pub(crate) fn paste_text(&mut self, text: &str) -> bool {
        if text.is_empty() || !self.takes_text() {
            return false;
        }
        let _ = self.inputs.send(Input::Text(text.to_owned()));
        true
    }

    /// What Copy takes: the lines selected with the mouse, else the
    /// focused node's selected row (a table's columns separated by tabs)
    /// or focused form field, or an agent view's latest answer.
    pub(crate) fn copy_text(&self) -> Option<String> {
        let app = self.shown.as_ref()?;
        let tree = self.drawn_tree();
        if let Some(sel) = &self.lines {
            let text = sel
                .range()
                .filter_map(|i| line_text(tree.as_ref(), app, &sel.node, i))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return Some(text);
            }
        }
        copy_text(tree.as_ref(), app)
    }

    /// Copy: to the clipboard, and to tmux's newest buffer when the
    /// settings send Dreamer's copies there.
    pub(crate) fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.copy_text() {
            crate::to_tmux_buffer(&text);
            crate::clip::write(cx, text);
        }
    }

    /// Paste the clipboard's text into the focused input.
    pub(crate) fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = crate::clip::read(cx) {
            self.paste_text(&text);
        }
    }

    /// tmux's newest buffer into the focused input (a middle click, or the
    /// prefix and `]`), as tmux pastes it.
    fn paste_tmux_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(b) = crate::TMUX_BUFFERS.get() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Some(text) = b.newest().await {
                let _ = this.update(cx, |p, _| p.paste_text(&text));
            }
        })
        .detach();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.tmux_key(ev, cx) {
            cx.stop_propagation();
            return;
        }
        // Shift-Enter starts a new line in a composer.
        let k = &ev.keystroke;
        if k.key == "enter"
            && k.modifiers.shift
            && !k.modifiers.platform
            && self.drawn_tree().is_some_and(|t| {
                t.focus
                    .as_deref()
                    .and_then(|f| t.root.find(f))
                    .is_some_and(|n| matches!(n.kind, Kind::Composer { .. }))
            })
        {
            self.key("Shift-Enter");
            cx.stop_propagation();
            return;
        }
        match input::app_intent(&ev.keystroke) {
            AppKey::Copy => self.copy(cx),
            AppKey::Paste => self.paste(cx),
            AppKey::Key(name) => {
                // A key acts on the view; a mouse selection is done with.
                self.lines = None;
                self.key(&name);
            }
            AppKey::Pass => return,
        }
        cx.stop_propagation();
    }

    /// tmux's prefix in an app pane, as in a terminal pane: the key after
    /// it runs tmux's binding for it (prefix and an arrow selects the
    /// next pane), and `]` pastes tmux's newest buffer into the focused
    /// input. True when the key was the layer's.
    fn tmux_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let (Some((pane, _)), Some(b)) = (self.pane.as_ref(), crate::TMUX_BUFFERS.get()) else {
            return false;
        };
        let Some(key) = crate::tmux_keys::from_keystroke(&ev.keystroke) else {
            return false;
        };
        if !self.prefixed {
            if crate::prefix::is_prefix(&key) {
                self.prefixed = true;
                cx.notify();
                return true;
            }
            return false;
        }
        self.prefixed = false;
        crate::prefix::refresh(cx);
        cx.notify();
        if key.text() == Some(']') {
            self.paste_tmux_buffer(cx);
            return true;
        }
        let (pane, name) = (*pane, key.tmux_name());
        cx.spawn(async move |_, _| {
            // A mode tmux opens (copy mode, a chooser) is closed by the
            // daemon: an app pane has no terminal to show it in.
            let _ = b.prefix_key(pane, name).await;
        })
        .detach();
        true
    }

    /// A left press on line `i` of node `node`: the start of a mouse
    /// selection.
    fn line_down(&mut self, node: &str, i: usize, pick: bool, cx: &mut Context<Self>) {
        self.lines = Some(LineSel {
            node: node.to_owned(),
            from: i,
            to: i,
            pick,
        });
        self.dragging = true;
        cx.notify();
    }

    /// The pointer over line `i` of node `node` with the button down
    /// extends the selection to it; a selection stays in its node.
    fn line_moved(&mut self, node: &str, i: usize, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.dragging || ev.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(sel) = self.lines.as_mut()
            && sel.node == node
            && sel.to != i
        {
            sel.to = i;
            cx.notify();
        }
    }

    /// The button comes up. A click (no drag) on a table's or list's row
    /// selects it, and on a form's field focuses it, in the app (naming
    /// the node clicked, which takes the focus); a drag keeps the lines
    /// selected for Copy.
    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.dragging) {
            return;
        }
        if let Some(sel) = self.lines.take_if(|s| s.from == s.to && s.pick) {
            let _ = self.inputs.send(Input::Select {
                node: Some(sel.node),
                index: sel.from,
            });
        }
        cx.notify();
    }

    /// A middle click pastes tmux's newest buffer into the focused input,
    /// as in a terminal pane (when `clipboard.middle_click` is `paste`).
    fn middle_click(&mut self, cx: &mut Context<Self>) {
        if crate::live::with(|l| l.middle_click) == Some(workspace_config::MiddleClick::Paste) {
            self.paste_tmux_buffer(cx);
        }
    }

    /// Wheel steps over a table or list (`node`) move its selection.
    fn wheel(&mut self, node: &str, ev: &ScrollEvent, cx: &mut Context<Self>) {
        let steps = input::app_wheel_steps(&mut self.wheel, ev.delta, px(LINE_HEIGHT));
        if steps != 0 {
            let _ = self.inputs.send(Input::Scroll {
                node: Some(node.to_owned()),
                steps: i64::from(steps),
            });
            cx.notify();
        }
    }
}

/// The height of a drawn line, for turning trackpad pixels into rows.
const LINE_HEIGHT: f32 = 16.0;

/// Text input from the platform, including IME: committed text goes to
/// the view (`AppPanel::typed`), composition is only shown.
impl EntityInputHandler for AppPanel {
    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.preedit.as_ref().map(|p| 0..p.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.preedit = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit = None;
        // The key after tmux's prefix is never typed into the app.
        if !self.prefixed {
            self.lines = None;
            self.typed(text);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        new_text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The IME candidate window opens at the bottom left of the view.
        Some(Bounds::new(
            gpui::point(
                element_bounds.origin.x,
                element_bounds.origin.y + element_bounds.size.height - px(LINE_HEIGHT),
            ),
            gpui::size(px(LINE_HEIGHT), px(LINE_HEIGHT)),
        ))
    }
}

/// Mouse input over what a view draws: each line takes presses and drags
/// (and is highlighted while selected), and each table or list takes
/// wheel steps, by node key.
#[derive(Clone)]
struct Lines {
    entity: Entity<AppPanel>,
    sel: Option<LineSel>,
    mark: u32,
}

impl Lines {
    /// Line `i` of node `node`; `pick` when a click there selects it (a
    /// table's or list's row, a form's field).
    fn wrap(&self, node: &str, i: usize, pick: bool, d: gpui::Div) -> gpui::Div {
        let selected = self
            .sel
            .as_ref()
            .is_some_and(|s| s.node == node && s.range().contains(&i));
        let (down, moved) = (self.entity.clone(), self.entity.clone());
        let (k1, k2) = (node.to_owned(), node.to_owned());
        d.when(selected, |d| d.bg(ca(self.mark)))
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                down.update(cx, |p, cx| p.line_down(&k1, i, pick, cx));
            })
            .on_mouse_move(move |ev, _, cx| {
                moved.update(cx, |p, cx| p.line_moved(&k2, i, ev, cx));
            })
    }

    /// Wheel steps over the table or list `node` move its selection.
    fn wheel<E: InteractiveElement>(&self, node: &str, e: E) -> E {
        let entity = self.entity.clone();
        let key = node.to_owned();
        e.on_scroll(move |ev, _, cx| {
            entity.update(cx, |p, cx| p.wheel(&key, ev, cx));
        })
    }
}

/// Whether the tree's focused node is a form whose focused field takes
/// text (not a toggle).
fn tree_takes_text(tree: &Tree) -> bool {
    let Some(n) = tree.focus.as_deref().and_then(|k| tree.root.find(k)) else {
        return false;
    };
    match &n.kind {
        Kind::Form { fields, focus } => fields.get(*focus).is_some_and(|f| !f.toggle),
        Kind::Input { .. } | Kind::Composer { .. } => true,
        _ => false,
    }
}

/// The text of line `i` of node `key` as the view draws it: a table's row
/// (columns separated by tabs), a list's or log's row, a form field's
/// value, a text node's text; or, in an agentic pane (no tree), a
/// transcript entry.
fn line_text(tree: Option<&Tree>, app: &App, key: &str, i: usize) -> Option<String> {
    let Some(tree) = tree else {
        return (key == AGENT)
            .then(|| app.agent.transcript.get(i).map(|e| e.text.clone()))
            .flatten();
    };
    let n = tree.root.find(key)?;
    match &n.kind {
        Kind::Table { .. } | Kind::List { .. } | Kind::Log => {
            n.children.get(i).map(|r| r.cells().join("\t"))
        }
        Kind::Form { fields, .. } => fields.get(i).map(|f| f.value.clone()),
        Kind::Text { text } => (i == 0).then(|| text.clone()),
        Kind::Markdown { .. } | Kind::Code { .. } | Kind::Diff { .. } => {
            rich_of(&n.kind).get(i).map(rich::Line::text)
        }
        Kind::Input { value, .. } => (i == 0).then(|| value.clone()),
        Kind::Select { options, .. } => options.get(i).cloned(),
        _ => None,
    }
}

/// A text component's lines, as drawn.
fn rich_of(kind: &Kind) -> Vec<rich::Line> {
    match kind {
        Kind::Markdown { text } => rich::markdown(text),
        Kind::Code { text, language } => rich::code(text, language),
        Kind::Diff { text } => rich::diff(text),
        _ => Vec::new(),
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
/// What a followed app pane shares with its view: the model, why it is
/// stale, its tree, and its bound streams.
struct Shared {
    latest: Arc<Mutex<Option<App>>>,
    ended: Arc<Mutex<Option<String>>>,
    tree: Arc<Mutex<Option<Tree>>>,
    streams: app_model::bound::Streams,
    /// The view tree's version in `tree`, stored after the tree.
    seq: Arc<std::sync::atomic::AtomicU64>,
}

/// What a pane's follower receives: input for the app, calls made for
/// it, and its pages' bridge calls.
type Channels = (
    tokio::sync::mpsc::UnboundedReceiver<Input>,
    tokio::sync::mpsc::UnboundedReceiver<api::Request>,
    tokio::sync::mpsc::UnboundedReceiver<web_engine::BridgeCall>,
);

async fn follow_pane(
    pane: u32,
    socket: PathBuf,
    (mut inputs, mut calls, mut bridge): Channels,
    shared: Shared,
    wake: impl Fn(),
) {
    use std::time::Duration;
    let Shared {
        latest,
        ended,
        tree,
        streams,
        seq,
    } = shared;
    // The streams the tree binds, followed for the app in this pane.
    let mut follower = app_model::bound::Follower::new(Some(pane));
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
        let (c, mut pushed) = match client::Client::connect(&socket, "app-pane").await {
            Ok(mut c) => {
                let pushed = c.pushed();
                (Arc::new(c), pushed)
            }
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
        // Input goes on its own task, one call after another, so it keeps
        // its order and never waits for a view.
        let (keys, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<api::AppInput>();
        let sender = c.clone();
        let key_task = tokio::spawn(async move {
            while let Some(input) = key_rx.recv().await {
                let _ = sender.call(api::Request::AppsInput(input)).await;
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
        follower.reset();
        let lost = loop {
            let next_pushed = async {
                match pushed.as_mut() {
                    Some(p) => p.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                p = next_pushed => match p {
                    Some(client::Pushed::Event { topic, payload, .. }) => {
                        if let Ok(n) = api::Notification::decode(&topic, &payload)
                            && streams.take(&n)
                        {
                            // Coalesced: one wake stands for every batch
                            // before the next paint.
                            wake();
                        }
                    }
                    Some(client::Pushed::Closed { error }) => {
                        break error.unwrap_or_else(|| "the daemon closed the connection".into());
                    }
                    Some(_) => {}
                    None => break "the daemon closed the connection".into(),
                },
                Some(r) = calls.recv() => {
                    // An approval answered from a card; a refusal shows in
                    // the transcript as the request still waiting.
                    let c = c.clone();
                    tokio::spawn(async move {
                        let _ = c.call(r).await;
                    });
                }
                Some(call) = bridge.recv() => {
                    // A page's call, made as the app in this pane.
                    let c = c.clone();
                    tokio::spawn(async move {
                        let asked = api::Request::AppsCall(api::AppCall {
                            pane,
                            method: call.method,
                            params: call.params,
                        });
                        (call.reply)(match c.call(asked).await {
                            Ok(api::Response::AppsCall(r)) => Ok(r.result),
                            Ok(_) => Err((api::code::INTERNAL, "an unexpected reply".into())),
                            Err(client::ClientError::Rpc(e)) => Err((e.code, e.message)),
                            Err(e) => Err((api::code::UNAVAILABLE, e.to_string())),
                        });
                    });
                }
                i = inputs.recv() => match i {
                    None | Some(Input::Quit) => {
                        key_task.abort();
                        return;
                    }
                    Some(Input::Redraw) => {}
                    Some(i) => {
                        if let Some(input) = to_remote(pane, i) {
                            let _ = keys.send(input);
                        }
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
                                // After the tree: a reader that sees this
                                // version has the tree too.
                                seq.store(mirror.seq, std::sync::atomic::Ordering::Release);
                                if changed
                                    && let Some(f) = mirror.frame.as_ref()
                                    && let Err(e) = follower.sync(&c, f, &streams).await
                                {
                                    break e;
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

/// An input for an app pane's host, as `apps.input` carries it.
fn to_remote(pane: u32, i: Input) -> Option<api::AppInput> {
    let mut out = api::AppInput {
        pane,
        ..api::AppInput::default()
    };
    match i {
        Input::Key(key) => out.key = key,
        Input::Text(text) => out.text = Some(text),
        Input::Select { node, index } => {
            out.select = Some(u32::try_from(index).ok()?);
            out.node = node;
        }
        Input::Scroll { node, steps } => {
            out.scroll = Some(i32::try_from(steps).ok()?);
            out.node = node;
        }
        Input::Redraw | Input::Quit => return None,
    }
    Some(out)
}

/// The text Copy takes when no lines are selected: from the tree drawn,
/// the focused table's or list's selected row (a table's columns
/// separated by tabs) or the focused form field, else a text view's text;
/// from an agentic pane (no tree), its latest answer.
fn copy_text(tree: Option<&Tree>, app: &App) -> Option<String> {
    let Some(tree) = tree else {
        return app
            .agent
            .transcript
            .iter()
            .rev()
            .find(|e| e.who == app_model::Said::Agent)
            .map(|e| e.text.clone())
            .filter(|t| !t.is_empty());
    };
    let text = match tree.focus.as_deref().and_then(|k| tree.root.find(k)) {
        Some(n) => match &n.kind {
            Kind::Table { .. } | Kind::List { .. } => n
                .selected_index()
                .and_then(|i| n.children.get(i))
                .map(|r| r.cells().join("\t")),
            Kind::Log => n.children.last().map(|r| r.cells().join("\t")),
            Kind::Form { fields, focus } => fields.get(*focus).map(|f| f.value.clone()),
            Kind::Input { value, .. } | Kind::Composer { value, .. } => Some(value.clone()),
            Kind::Select {
                options, selected, ..
            } => options.get(*selected).cloned(),
            _ => None,
        },
        None => tree.root.kind.text().map(str::to_owned),
    };
    text.filter(|t| !t.is_empty())
}

/// The GUI benchmark's record of panel renders, while it records them
/// (`crate::bench_gui`): when each began, how long building its elements
/// took (ns), and the streams' revision it drew.
pub(crate) static RENDER_LOG: Mutex<Option<Vec<(std::time::Instant, u64, u64)>>> = Mutex::new(None);

impl Render for AppPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t0 = std::time::Instant::now();
        let revision = self.streams.revision();
        let e = self.render_panel(window, cx);
        if let Ok(mut log) = RENDER_LOG.lock()
            && let Some(log) = log.as_mut()
        {
            log.push((t0, t0.elapsed().as_nanos() as u64, revision));
        }
        e
    }
}

impl AppPanel {
    fn render_panel(&mut self, window_: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = theme::for_window(window_);
        // Before the tree is read, so the version is never ahead of it.
        self.painted = self.seq.load(std::sync::atomic::Ordering::Acquire);
        let latest = self.latest.lock().ok().and_then(|l| l.clone());
        if latest.is_some() {
            self.shown = latest;
        }
        let ended = self.ended();
        let focused = self.focus.is_focused(window_);
        let entity = cx.entity();
        let focus = self.focus.clone();
        let lines = Lines {
            entity: entity.clone(),
            sel: self.lines.clone(),
            mark: t.mark,
        };
        let preedit = self.preedit.clone().map(|text| {
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .px_2()
                .bg(c(t.selected))
                .underline()
                .child(text)
        });
        let prefix = self.prefixed.then(|| {
            div()
                .absolute()
                .bottom_0()
                .right_0()
                .px_1()
                .bg(c(t.accent))
                .text_color(c(t.background))
                .child("prefix")
        });
        let root = div()
            .id("app-panel")
            .relative()
            .track_focus(&self.focus)
            .on_action(cx.listener(|p, _: &crate::keymap::Copy, _, cx| p.copy(cx)))
            .on_action(cx.listener(|p, _: &crate::keymap::Paste, _, cx| p.paste(cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_click(cx.listener(|p, _, window, cx| {
                window.focus(&p.focus, cx);
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|p, _: &MouseDownEvent, _, cx| p.middle_click(cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|p, _: &MouseUpEvent, _, cx| p.mouse_up(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|p, _: &MouseUpEvent, _, cx| p.mouse_up(cx)),
            )
            .children(preedit)
            .children(prefix)
            // Registers the platform text input (IME) each frame.
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
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
        let tree = self.tree.lock().ok().and_then(|t| t.clone());
        // The host's tree; else the model's (a panel, or a host that
        // publishes none); an agentic pane has no tree.
        let tree = tree.or_else(|| self.shown.as_ref().and_then(App::frame));
        if let Some(tree) = &tree {
            self.sync_embeds(tree, cx);
            self.sync_pages(tree, window_, cx);
        }
        let Some(app) = self.shown.as_ref() else {
            return root
                .child(
                    div()
                        .p_2()
                        .text_color(c(t.muted))
                        .child(ended.unwrap_or_else(|| "starting…".into())),
                )
                .into_any_element();
        };
        let answer: bound::Answer = {
            let entity = cx.entity();
            std::rc::Rc::new(move |cx: &mut gpui::App, a: bound::Answering| {
                entity.update(cx, |p, _| p.answer(a));
            })
        };
        let streams = self.streams.clone();
        let term = crate::term_font();
        let (title, status, content) = match &tree {
            Some(tree) => (
                tree.title.clone(),
                tree.status.clone(),
                streams.with(|data| {
                    node(
                        &tree.root,
                        &Draw {
                            tree,
                            t: &t,
                            lines: &lines,
                            data,
                            answer: &answer,
                            follow: &self.follow,
                            embedded: &self.embedded,
                            pictures: &self.pictures,
                            pages: &self.pages.views,
                            term: &term,
                        },
                        &mut self.scrolls,
                        &mut self.scrolled_to,
                    )
                }),
            ),
            None => {
                let title = if app.version > 1 {
                    format!("{} (reloaded {})", app.manifest.title, app.version - 1)
                } else {
                    app.manifest.title.clone()
                };
                (title, app.status_text(), body(app, &t, &lines))
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
        .into_any_element()
    }

    /// A panel drawing `tree` with `streams`, and no app behind it: the GUI
    /// benchmark's streaming transcript. Clicks and keys go nowhere.
    pub(crate) fn fixed(
        tree: Tree,
        streams: app_model::bound::Streams,
        cx: &mut Context<Self>,
    ) -> AppPanel {
        let manifest = "schema_version = 1\nid = \"bench.transcript\"\nversion = \"0.1.0\"\ntitle = \"Transcript\"\n\n[data.none]\nmethod = \"tmux.topology\"\n\n[view]\ntype = \"text\"\nsource = \"none\"\n";
        let app = app_model::manifest::Manifest::parse(manifest)
            .ok()
            .map(App::new);
        AppPanel {
            path: PathBuf::new(),
            pane: None,
            latest: Arc::new(Mutex::new(app)),
            ended: Arc::default(),
            inputs: tokio::sync::mpsc::unbounded_channel().0,
            focus: cx.focus_handle(),
            shown: None,
            tree: Arc::new(Mutex::new(Some(tree))),
            seq: Arc::default(),
            painted: 0,
            scrolls: HashMap::new(),
            scrolled_to: HashMap::new(),
            prefixed: false,
            lines: None,
            dragging: false,
            wheel: 0.0,
            preedit: None,
            streams,
            calls: tokio::sync::mpsc::unbounded_channel().0,
            follow: Default::default(),
            socket: PathBuf::new(),
            embedded: HashMap::new(),
            pictures: HashMap::new(),
            pages: Pages::default(),
        }
    }
}

/// An agentic pane: who it is, what it may do, its steps, its
/// transcript, and the prompt. Transcript entries take mouse selection
/// as lines of the node `AGENT`.
fn body(app: &App, t: &theme::Theme, lines: &Lines) -> gpui::AnyElement {
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
                    .children(a.transcript[newest..].iter().enumerate().map(|(i, e)| {
                        let (mark, color) = match e.who {
                            Said::You => ("> ", t.text),
                            Said::Agent => ("", t.text),
                            Said::Tool => ("→ ", t.accent),
                            Said::Result { ok: true } => ("← ", t.faint),
                            Said::Result { ok: false } => ("← failed: ", t.danger),
                            Said::Note => ("· ", t.faint),
                            Said::Failed => ("✗ ", t.danger),
                        };
                        lines.wrap(
                            AGENT,
                            newest + i,
                            false,
                            div()
                                .text_color(c(color))
                                .child(SharedString::from(format!("{mark}{}", e.text))),
                        )
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

/// What every node is drawn with: the tree, the theme, and the mouse
/// input over lines.
struct Draw<'a> {
    tree: &'a Tree,
    t: &'a theme::Theme,
    lines: &'a Lines,
    /// The bound streams, their transcripts' scrolls, and what an
    /// approval card's button does.
    data: &'a app_model::bound::Data,
    answer: &'a bound::Answer,
    follow: &'a std::cell::RefCell<HashMap<String, bound::Follow>>,
    embedded: &'a HashMap<u32, Embedded>,
    pictures: &'a HashMap<String, Picture>,
    term: &'a crate::TermFont,
    pages: &'a HashMap<String, Entity<crate::web::WebPane>>,
}

/// The `web` nodes of `n`'s subtree whose pages are shown (not refused
/// ones, and only on shown tabs).
fn pages_in(n: &Node, out: &mut Vec<Node>) {
    if page_url(n).is_some() {
        out.push(n.clone());
    }
    for c in n.shown() {
        pages_in(c, out);
    }
}

/// The address a `web` node's page shows, if it shows one.
fn page_url(n: &Node) -> Option<&String> {
    match &n.kind {
        Kind::Web { page, refused, .. } if refused.is_empty() && !page.url.is_empty() => {
            Some(&page.url)
        }
        _ => None,
    }
}

/// A `web` node: its page's view; its address in a panel; or why it is
/// refused.
fn web_node(n: &Node, src: &str, refused: &str, dr: &Draw<'_>) -> gpui::AnyElement {
    let t = dr.t;
    let note = |text: String, color: u32| {
        div()
            .text_color(c(color))
            .child(SharedString::from(text))
            .into_any_element()
    };
    if !refused.is_empty() {
        return note(format!("web {src}: {refused}"), t.danger);
    }
    match dr.pages.get(&n.key) {
        Some(view) => div().size_full().child(view.clone()).into_any_element(),
        None => note(
            format!("web: {}", page_url(n).map_or(src, String::as_str)),
            t.muted,
        ),
    }
}

/// The files of the images `n`'s subtree shows (not refused ones).
fn images_in(n: &Node, out: &mut Vec<String>) {
    if let Kind::Image { path, refused, .. } = &n.kind
        && refused.is_empty()
        && !path.is_empty()
        && !out.contains(path)
    {
        out.push(path.clone());
    }
    for c in n.shown() {
        images_in(c, out);
    }
}

/// A `terminal` node: its pane's newest frame, clipped to the node, read
/// only; or why it is not shown.
fn terminal_node(pane: u32, refused: &str, dr: &Draw<'_>) -> gpui::AnyElement {
    let t = dr.t;
    let note = |text: String, color: u32| {
        div()
            .text_color(c(color))
            .child(SharedString::from(text))
            .into_any_element()
    };
    if !refused.is_empty() {
        return note(format!("refused: {refused}"), t.danger);
    }
    match dr.embedded.get(&pane).and_then(|e| e.frame.clone()) {
        Some(frame) => div()
            .size_full()
            .overflow_hidden()
            .child(terminal_render::TerminalElement {
                frame,
                font: gpui::font(dr.term.family.clone()),
                font_size: px(dr.term.size),
                spacing: dr.term.spacing(),
            })
            .into_any_element(),
        None => note(format!("pane %{pane}…"), t.muted),
    }
}

/// An `image` node: the picture at its own size, shrunk to fit; or its
/// name while it loads, or why it is not shown.
fn image_node(
    src: &str,
    path: &str,
    size: (u32, u32),
    refused: &str,
    dr: &Draw<'_>,
) -> gpui::AnyElement {
    let t = dr.t;
    let note = |text: String, color: u32| {
        div()
            .text_color(c(color))
            .child(SharedString::from(text))
            .into_any_element()
    };
    if !refused.is_empty() {
        return note(format!("image {src}: {refused}"), t.danger);
    }
    match dr.pictures.get(path) {
        Some(Picture::Ready(image)) => gpui::img(image.clone())
            .w(px(size.0 as f32))
            .h(px(size.1 as f32))
            .max_w_full()
            .max_h_full()
            .into_any_element(),
        Some(Picture::Failed(why)) => note(format!("image {src}: {why}"), t.danger),
        _ => note(format!("image {src}…"), t.muted),
    }
}

/// A node of the view tree, drawn natively. Scroll handles are kept in
/// `scrolls` by node key, so a new tree keeps each position; a table or
/// list scrolls to its selection when the selection moves. Every line of
/// a table, list, log, form, or text takes mouse presses and drags by its
/// node's key and the index `line_text` reads it at; wheel steps over a
/// table or list move its selection.
fn node(
    n: &Node,
    dr: &Draw<'_>,
    scrolls: &mut HashMap<String, gpui::ScrollHandle>,
    scrolled_to: &mut HashMap<String, Option<String>>,
) -> gpui::AnyElement {
    let (tree, t, lines) = (dr.tree, dr.t, dr.lines);
    let focused = tree.focus.as_deref() == Some(n.key.as_str());
    let id = SharedString::from(format!("app-node-{}", n.key));
    let mut child = |c: &Node| node(c, dr, scrolls, scrolled_to);
    let content = match &n.kind {
        Kind::Column | Kind::Scroll => {
            let d = div()
                .flex()
                .flex_col()
                .gap_1()
                .size_full()
                .children(n.children.iter().map(|c| {
                    let fixed = fixed_height(c) || n.kind == Kind::Scroll;
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
            .children(n.children.iter().map(|c| {
                // A badge or a button keeps its own width.
                let own =
                    c.title.is_none() && matches!(c.kind, Kind::Badge { .. } | Kind::Button { .. });
                div()
                    .min_w_0()
                    .when(own, |d| d.flex_none())
                    .when(!own, |d| d.flex_1().h_full())
                    .child(child(c))
            }))
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
        Kind::Log if matches!(n.bind, Some(view_tree::Bind::Tap(_))) => match &n.bind {
            Some(view_tree::Bind::Tap(pane)) => bound::tap(n, *pane, t, dr.data),
            _ => div().into_any_element(),
        },
        Kind::Transcript { .. } => {
            bound::transcript(n, t, lines, dr.data, &mut dr.follow.borrow_mut(), dr.answer)
        }
        Kind::Composer { .. } => bound::composer(n, focused, t, lines, dr.data),
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
                    lines.wrap(
                        &n.key,
                        start + i,
                        !log,
                        line(t, !log && Some(start + i) == selected)
                            .children(r.cells().iter().map(|s| cell(s.clone()))),
                    )
                }));
            let rows = if log { rows } else { lines.wheel(&n.key, rows) };
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
        Kind::Text { text } => lines
            .wrap(
                &n.key,
                0,
                false,
                div().child(SharedString::from(text.clone())),
            )
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
                lines.wrap(
                    &n.key,
                    i,
                    true,
                    line(t, focused && i == *focus)
                        .child(
                            div()
                                .text_color(c(t.muted))
                                .child(format!("{}{star}", f.label)),
                        )
                        .child(cell(shown)),
                )
            }))
            .into_any_element(),
        Kind::Markdown { .. }
        | Kind::Code { .. }
        | Kind::Diff { .. }
        | Kind::Button { .. }
        | Kind::Input { .. }
        | Kind::Select { .. }
        | Kind::Toggle { .. }
        | Kind::Badge { .. }
        | Kind::Progress { .. }
        | Kind::Status { .. } => component(n, focused, t, lines, scrolls),
        Kind::Terminal { pane, refused } => terminal_node(*pane, refused, dr),
        Kind::Web { src, refused, .. } => web_node(n, src, refused, dr),
        Kind::Image {
            src,
            path,
            width,
            height,
            refused,
        } => image_node(src, path, (*width, *height), refused, dr),
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

/// Takes the height it needs in a column, not a share of what is left:
/// text, gauges, controls, badges, progress and status lines, and rows or
/// columns of only those.
fn fixed_height(n: &Node) -> bool {
    match &n.kind {
        Kind::Text { .. }
        | Kind::Gauge { .. }
        | Kind::Button { .. }
        | Kind::Input { .. }
        | Kind::Select { .. }
        | Kind::Toggle { .. }
        | Kind::Badge { .. }
        | Kind::Progress { .. }
        | Kind::Status { .. }
        | Kind::Composer { .. }
        | Kind::Image { .. } => true,
        Kind::Row | Kind::Column => !n.children.is_empty() && n.children.iter().all(fixed_height),
        _ => false,
    }
}

/// A tone's colours from the theme: the text, and the wash behind it.
fn tone_colors(t: &theme::Theme, tone: Tone) -> (u32, gpui::Rgba) {
    // A wash of a theme colour: the colour at a fifth of its strength.
    let wash = |hex: u32| ca((hex << 8) | 0x33);
    match tone {
        Tone::Neutral => (t.muted, c(t.selected)),
        Tone::Accent => (t.accent, wash(t.accent)),
        Tone::Success => (t.success, wash(t.success)),
        Tone::Warning => (t.warning_text, c(t.warning_surface)),
        Tone::Danger => (t.danger_text, c(t.danger_surface)),
    }
}

/// How a span of markdown, code, or a diff is drawn, from the theme.
fn role_style(t: &theme::Theme, r: Role) -> gpui::HighlightStyle {
    let color = |hex: u32| Some(gpui::Hsla::from(c(hex)));
    let mut s = gpui::HighlightStyle::default();
    match r {
        Role::Plain => {}
        Role::Strong => s.font_weight = Some(gpui::FontWeight::BOLD),
        Role::Emphasis => s.font_style = Some(gpui::FontStyle::Italic),
        Role::Code => {
            s.color = color(t.kind);
            s.background_color = color(t.selected);
        }
        Role::Link => {
            s.color = color(t.accent);
            s.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.0),
                color: color(t.accent),
                wavy: false,
            });
        }
        Role::Marker | Role::Hunk => s.color = color(t.accent),
        Role::Keyword => s.color = color(t.keyword),
        Role::String => s.color = color(t.string),
        Role::Number => s.color = color(t.number),
        Role::Comment => {
            s.color = color(t.faint);
            s.font_style = Some(gpui::FontStyle::Italic);
        }
        Role::Type => s.color = color(t.kind),
        Role::Added => s.color = color(t.success),
        Role::Removed => s.color = color(t.danger),
        Role::Meta => {
            s.color = color(t.muted);
            s.font_weight = Some(gpui::FontWeight::BOLD);
        }
    }
    s
}

/// One line of rich text as styled text.
fn rich_text(t: &theme::Theme, l: &rich::Line) -> gpui::StyledText {
    let mut text = String::new();
    let mut runs = Vec::new();
    for s in &l.spans {
        let start = text.len();
        text.push_str(&s.text);
        if s.role != Role::Plain {
            runs.push((start..text.len(), role_style(t, s.role)));
        }
    }
    gpui::StyledText::new(text).with_highlights(runs)
}

/// A text, input, or status component, drawn natively. Each line of
/// markdown, code, or a diff takes mouse selection (for Copy); a click on
/// a control goes to the app as a `select` of its node (an option's index
/// for a select), which presses, flips, picks, or focuses it there.
fn component(
    n: &Node,
    focused: bool,
    t: &theme::Theme,
    lines: &Lines,
    scrolls: &mut HashMap<String, gpui::ScrollHandle>,
) -> gpui::AnyElement {
    let label = |s: &str| {
        (!s.is_empty()).then(|| {
            div()
                .flex_none()
                .text_color(c(t.muted))
                .child(SharedString::from(s.to_owned()))
        })
    };
    match &n.kind {
        Kind::Markdown { .. } | Kind::Code { .. } | Kind::Diff { .. } => {
            let markdown = matches!(n.kind, Kind::Markdown { .. });
            let handle = scrolls.entry(n.key.clone()).or_default().clone();
            div()
                .id(SharedString::from(format!("app-node-{}", n.key)))
                .flex()
                .flex_col()
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&handle)
                .when(!markdown, |d| d.p_1().rounded_sm().bg(c(t.raised)))
                .children(rich_of(&n.kind).iter().enumerate().map(|(i, l)| {
                    let d = div().min_h(px(LINE_HEIGHT));
                    let d = match l.block {
                        RichBlock::Heading(level) => d
                            .when(level == 1, |d| d.text_size(px(16.0)).mt_1())
                            .when(level == 2, |d| d.text_size(px(14.0)))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(rich_text(t, l)),
                        RichBlock::Quote => d
                            .border_l_2()
                            .border_color(c(t.accent))
                            .pl_2()
                            .italic()
                            .text_color(c(t.muted))
                            .child(rich_text(t, l)),
                        // A code block in markdown sits on the raised colour;
                        // a code or diff node already does.
                        RichBlock::Code => d
                            .whitespace_nowrap()
                            .when(markdown, |d| d.px_2().bg(c(t.raised)))
                            .child(rich_text(t, l)),
                        RichBlock::Rule => d
                            .flex()
                            .items_center()
                            .child(div().h(px(1.0)).w_full().bg(c(t.divider))),
                        RichBlock::Text => d.child(rich_text(t, l)),
                    };
                    lines.wrap(&n.key, i, false, d)
                }))
                .into_any_element()
        }
        Kind::Button { label: text } => div()
            .flex()
            .child(
                lines.wrap(
                    &n.key,
                    0,
                    true,
                    div()
                        .flex_none()
                        .cursor_pointer()
                        .px_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(c(if focused { t.focus } else { t.divider }))
                        .bg(c(if focused { t.accent } else { t.raised }))
                        .text_color(c(if focused { t.background } else { t.text }))
                        .child(SharedString::from(text.clone())),
                ),
            )
            .into_any_element(),
        Kind::Input {
            label: name,
            value,
            placeholder,
        } => {
            let empty = value.is_empty();
            lines
                .wrap(
                    &n.key,
                    0,
                    true,
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .children(label(name))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(c(if focused { t.focus } else { t.divider }))
                                .bg(c(t.background))
                                .when(!empty, |d| d.child(SharedString::from(value.clone())))
                                .when(focused, |d| {
                                    d.child(div().text_color(c(t.accent)).child("▏"))
                                })
                                .when(empty, |d| {
                                    d.child(
                                        div()
                                            .text_color(c(t.faint))
                                            .child(SharedString::from(placeholder.clone())),
                                    )
                                }),
                        ),
                )
                .into_any_element()
        }
        Kind::Select {
            label: name,
            options,
            selected,
        } => div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .children(label(name))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .p(px(1.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(c(if focused { t.focus } else { t.divider }))
                    .children(options.iter().enumerate().map(|(i, o)| {
                        let on = i == *selected;
                        lines.wrap(
                            &n.key,
                            i,
                            true,
                            div()
                                .cursor_pointer()
                                .px_1()
                                .rounded_sm()
                                .when(on, |d| d.bg(c(t.selected)).text_color(c(t.text)))
                                .when(!on, |d| d.text_color(c(t.muted)))
                                .child(SharedString::from(o.clone())),
                        )
                    })),
            )
            .into_any_element(),
        Kind::Toggle { label: name, on } => lines
            .wrap(
                &n.key,
                0,
                true,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .when(*on, |d| d.justify_end())
                            .w(px(24.0))
                            .h(px(14.0))
                            .p(px(2.0))
                            .rounded_full()
                            .border_1()
                            .border_color(c(if focused { t.focus } else { t.divider }))
                            .bg(c(if *on { t.accent } else { t.selected }))
                            .child(div().size(px(8.0)).rounded_full().bg(c(if *on {
                                t.background
                            } else {
                                t.faint
                            }))),
                    )
                    .child(
                        div()
                            .text_color(c(if focused { t.text } else { t.muted }))
                            .child(SharedString::from(name.clone())),
                    ),
            )
            .into_any_element(),
        Kind::Badge { text, tone } => {
            let (fg, bg) = tone_colors(t, *tone);
            div()
                .flex()
                .child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(bg)
                        .text_color(c(fg))
                        .text_size(px(11.0))
                        .child(SharedString::from(text.clone())),
                )
                .into_any_element()
        }
        Kind::Progress { ratio, label: text } => div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .h(px(4.0))
                    .rounded_full()
                    .bg(c(t.selected))
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(gpui::relative(ratio.clamp(0.0, 1.0) as f32))
                            .bg(c(t.accent)),
                    ),
            )
            .children(label(text))
            .into_any_element(),
        Kind::Status { items } => div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .text_size(px(11.0))
            .children(items.iter().enumerate().flat_map(|(i, item)| {
                let fg = match item.tone {
                    Tone::Neutral => t.muted,
                    Tone::Accent => t.accent,
                    Tone::Success => t.success,
                    Tone::Warning => t.warning,
                    Tone::Danger => t.danger,
                };
                let sep = (i > 0).then(|| div().text_color(c(t.faint)).child("·"));
                sep.into_iter().chain(std::iter::once(
                    div()
                        .text_color(c(fg))
                        .child(SharedString::from(item.text.clone())),
                ))
            }))
            .into_any_element(),
        _ => div().into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> App {
        let m = app_model::manifest::Manifest::parse(
            r#"
schema_version = 1
id = "t.app"
version = "1"
title = "T"
[data.s]
method = "tmux.topology"
path = "panes"
[view]
type = "table"
source = "s"
columns = ["id", "name"]
"#,
        )
        .expect("manifest");
        let mut a = App::new(m);
        a.set_rows(
            vec![
                serde_json::json!({"id": 1, "name": "one"}),
                serde_json::json!({"id": 2, "name": "two"}),
            ],
            0,
        );
        a
    }

    #[test]
    fn copy_takes_a_lines_text_as_drawn() {
        let a = table();
        let tree = a.frame();
        let tree = tree.as_ref();
        assert_eq!(line_text(tree, &a, "view", 1).as_deref(), Some("2\ttwo"));
        assert_eq!(line_text(tree, &a, "view", 2), None);
        assert_eq!(copy_text(tree, &a).as_deref(), Some("1\tone"));
    }

    #[test]
    fn copy_from_a_tree_takes_the_focused_nodes_selected_row() {
        let m = app_model::manifest::Manifest::parse(
            r#"
schema_version = 2
id = "t.tree"
version = "1"
title = "Tree"
root = "main"
[data.panes]
method = "tmux.topology"
path = "panes"
[data.windows]
method = "tmux.topology"
path = "windows"
[nodes.main]
type = "split"
children = ["left", "names"]
[nodes.left]
type = "table"
source = "panes"
row_key = "id"
columns = ["id", "cmd"]
[nodes.names]
type = "list"
source = "windows"
row_key = "id"
field = "name"
"#,
        )
        .expect("tree manifest");
        let mut a = App::new(m);
        a.set_source(
            "panes",
            vec![
                serde_json::json!({"id": 1, "cmd": "sh"}),
                serde_json::json!({"id": 2, "cmd": "vim"}),
            ],
            0,
        );
        a.set_source(
            "windows",
            vec![
                serde_json::json!({"id": 7, "name": "build"}),
                serde_json::json!({"id": 8, "name": "logs"}),
            ],
            0,
        );
        // A click on the list's second row, as the host takes it.
        a.select_at(Some("names"), 1);
        let tree = a.frame().expect("a tree");
        assert_eq!(copy_text(Some(&tree), &a).as_deref(), Some("logs"));
        // Wheel steps over the table: its selected row is copied.
        a.scroll(Some("left"), 1);
        let tree = a.frame().expect("a tree");
        assert_eq!(copy_text(Some(&tree), &a).as_deref(), Some("2\tvim"));
        // A mouse selection reads lines of the node it is in.
        assert_eq!(
            line_text(Some(&tree), &a, "names", 0).as_deref(),
            Some("build")
        );
        assert!(!tree_takes_text(&tree));
    }

    #[test]
    fn components_take_text_copy_and_lines_as_drawn() {
        let m = app_model::manifest::Manifest::parse(include_str!(
            "../../../examples/apps/components/app.toml"
        ))
        .expect("the reference app");
        let mut a = App::new(m);
        // The focused input takes text, and Copy takes its value.
        a.insert_text("Ada");
        let tree = a.frame().expect("a tree");
        assert!(tree_takes_text(&tree));
        assert_eq!(copy_text(Some(&tree), &a).as_deref(), Some("Ada"));
        // A click on an option, as the host takes it: Copy takes it.
        a.select_at(Some("skin"), 1);
        let tree = a.frame().expect("a tree");
        assert!(!tree_takes_text(&tree));
        assert_eq!(copy_text(Some(&tree), &a).as_deref(), Some("light"));
        // A mouse selection reads the lines of markdown as drawn.
        assert_eq!(
            line_text(Some(&tree), &a, "readme", 2).as_deref(),
            Some("Text with strong and emphasis, inline code, and a link.")
        );
        assert_eq!(
            line_text(Some(&tree), &a, "change", 5).as_deref(),
            Some("-    println!(\"hello\");")
        );
    }

    #[test]
    fn every_input_reaches_the_host_as_one_apps_input() {
        let text = to_remote(7, Input::Text("a b".into())).expect("text");
        assert_eq!((text.pane, text.text.as_deref()), (7, Some("a b")));
        assert!(text.key.is_empty() && text.select.is_none() && text.scroll.is_none());
        let click = to_remote(
            7,
            Input::Select {
                node: Some("names".into()),
                index: 3,
            },
        )
        .expect("click");
        assert_eq!(
            (click.select, click.node.as_deref()),
            (Some(3), Some("names"))
        );
        assert_eq!(
            to_remote(
                7,
                Input::Scroll {
                    node: None,
                    steps: -2
                }
            )
            .and_then(|i| i.scroll),
            Some(-2)
        );
        assert_eq!(
            to_remote(7, Input::Key("Enter".into())).map(|i| i.key),
            Some("Enter".into())
        );
        assert!(to_remote(7, Input::Redraw).is_none());
    }

    #[test]
    fn only_shown_images_load_and_a_file_that_is_no_image_fails() {
        let image = |key: &str, path: &str, refused: &str| {
            Node::new(
                key,
                Kind::Image {
                    src: "x.png".into(),
                    path: path.into(),
                    width: 1,
                    height: 1,
                    refused: refused.into(),
                },
            )
        };
        let mut tabs = Node::new("tabs", Kind::Tabs { active: 0 });
        tabs.children = vec![image("shown", "/a.png", ""), image("hidden", "/b.png", "")];
        let mut root = Node::new("main", Kind::Column);
        root.children = vec![
            tabs,
            image("refused", "/c.png", "outside"),
            image("same", "/a.png", ""),
        ];
        let mut out = Vec::new();
        images_in(&root, &mut out);
        assert_eq!(out, ["/a.png"]);

        let dir = std::env::temp_dir().join(format!("panel-picture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = dir.join("fake.png");
        std::fs::write(&fake, b"not an image").expect("write");
        let Picture::Failed(why) = load_picture(&fake.display().to_string()) else {
            panic!("decoded a text file");
        };
        assert_eq!(why, "not a PNG or JPEG");
        assert!(matches!(
            load_picture(&dir.join("none.png").display().to_string()),
            Picture::Failed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
