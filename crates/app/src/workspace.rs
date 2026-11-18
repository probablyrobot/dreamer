//! The workspace window (spec 4.2, 14.1, 15.1): one tmux session, a narrow
//! tab strip of its windows, and the active window's panes, each drawn at
//! its tmux cell rectangle. tmux is authoritative for geometry; the view
//! follows `tmux.watch` and never assumes an action's result.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gpui::{
    Context, Entity, FocusHandle, MouseButton, MouseDownEvent, Render, Window, div, font,
    prelude::*, px,
};
use replica::feed::Feed;
use replica::topology::TopologyFeed;
use terminal_render::cell_metrics;
use workspace_config::desktop_state::{DesktopState, DesktopStore};

use crate::theme::c;
use crate::{Shell, Smoke, load_frame, term_font};

/// Height of the tab strip.
/// The tab strip's height. On macOS it is the title bar too: the window's
/// buttons sit at its left, vertically centred.
const TAB_STRIP: f32 = if cfg!(target_os = "macos") {
    34.0
} else {
    30.0
};
/// Room for the macOS window buttons at the strip's left.
const TITLE_BUTTONS: f32 = if cfg!(target_os = "macos") { 78.0 } else { 0.0 };
/// How far in from a window's edge a press resizes it, when the window has
/// no system frame (Linux, `window.decorations: none`).
const RESIZE_EDGE: f32 = 5.0;
/// Space between the window's edges and the panes, so text never touches
/// the window frame.
const PANE_INSET: f32 = 6.0;
/// Command palette rows shown at once; the list scrolls with the selection.
const PALETTE_ROWS: usize = 12;
/// The command palette's width, centred under the tab strip.
const PALETTE_WIDTH: f32 = 460.0;

/// A visible pane: a terminal view with the feed that keeps it current;
/// for a pane whose program is a registered app host, the app drawn
/// natively from the model its host publishes; or, for a web pane, the
/// page Chromium draws over it.
enum PaneView {
    Terminal(Entity<Shell>),
    App {
        view: Entity<crate::panel::AppPanel>,
        instance: String,
    },
    Web {
        view: Entity<crate::web::WebPane>,
        url: String,
    },
}

impl PaneView {
    fn shell(&self) -> Option<&Entity<Shell>> {
        match self {
            PaneView::Terminal(s) => Some(s),
            PaneView::App { .. } | PaneView::Web { .. } => None,
        }
    }

    fn focus(&self, cx: &gpui::App) -> FocusHandle {
        match self {
            PaneView::Terminal(s) => s.read(cx).focus.clone(),
            PaneView::App { view, .. } => view.read(cx).focus.clone(),
            PaneView::Web { view, .. } => view.read(cx).focus.clone(),
        }
    }

    fn element(&self) -> gpui::AnyView {
        match self {
            PaneView::Terminal(s) => s.clone().into(),
            PaneView::App { view, .. } => view.clone().into(),
            PaneView::Web { view, .. } => view.clone().into(),
        }
    }

    /// The app instance drawn; `None` for a terminal.
    fn app(&self) -> Option<&str> {
        match self {
            PaneView::Terminal(_) | PaneView::Web { .. } => None,
            PaneView::App { instance, .. } => Some(instance),
        }
    }

    /// The web page drawn; `None` for a terminal or an app.
    fn web(&self) -> Option<&str> {
        match self {
            PaneView::Web { url, .. } => Some(url),
            PaneView::Terminal(_) | PaneView::App { .. } => None,
        }
    }

    fn web_pane(&self) -> Option<&Entity<crate::web::WebPane>> {
        match self {
            PaneView::Web { view, .. } => Some(view),
            PaneView::Terminal(_) | PaneView::App { .. } => None,
        }
    }
}

/// The app a pane's view draws instead of its terminal: the registered app
/// whose host is the pane's program, for a pane of the local daemon (whose
/// `apps.view` serves it). `None` draws the terminal.
fn app_drawn<'a>(p: &'a api::Pane, via: &replica::Via) -> Option<&'a str> {
    if *via != replica::Via::Local {
        return None;
    }
    p.app.as_ref().map(|a| a.instance.as_str())
}

/// The web page a pane's view draws instead of its terminal: its
/// placeholder's page, for a pane of the local tmux (pages run on this
/// machine, so a host's `localhost` would be the wrong one). `None` draws
/// the terminal, which shows the placeholder's text.
fn web_drawn<'a>(p: &'a api::Pane, via: &replica::Via) -> Option<&'a str> {
    if *via != replica::Via::Local {
        return None;
    }
    p.web.as_deref()
}

pub(crate) struct Workspace {
    /// The daemon's tmux options, for the fallback attach command.
    daemon_args: Vec<String>,
    /// The local daemon's tmux, or a host through its coordinator.
    pub(crate) via: replica::Via,
    /// The local daemon's remote hosts, while the sidebar shows them.
    hosts: Option<Arc<replica::hosts::HostsFeed>>,
    topology: Arc<TopologyFeed>,
    socket: PathBuf,
    /// The latest topology, and the session this window shows.
    current: Option<api::Topology>,
    session: Option<u32>,
    panes: HashMap<u32, PaneView>,
    /// Views of panes shown lately and hidden now (another tab, or zoomed
    /// away), still following their feeds: the Warm client tier of spec
    /// 16.6. Showing one again paints its last frame at once instead of
    /// waiting for a new checkpoint. At most `HIDDEN_VIEWS`, the oldest
    /// hidden going first.
    hidden: HashMap<u32, (PaneView, std::time::Instant)>,
    /// A tab clicked here and not yet confirmed by tmux: shown at once (its
    /// panes' last frames, when hidden views have them), until the
    /// topology reports it active or `SELECT_GRACE` passes. tmux stays
    /// authoritative: whatever it reports then is what the window shows.
    selecting: Option<(u32, std::time::Instant)>,
    /// The pane keyboard input goes to (local; tmux is told on a click).
    focused: Option<u32>,
    /// tmux's active pane of the shown window as last seen: when tmux
    /// selects another (`select-pane` from any client, or the prefix and
    /// an arrow), focus follows it here.
    tmux_active: Option<u32>,
    pub(crate) focus: FocusHandle,
    title: String,
    /// The pane area in cells as last painted, the size last requested for
    /// a tmux window, and whether a (debounced) request is pending.
    area: Option<(u32, u32)>,
    requested: Option<(u32, u32, u32)>,
    size_pending: bool,
    /// Where the pane area was painted and its cell size, for turning
    /// pointer positions into cells; and a divider drag in progress.
    origin: Option<gpui::Point<gpui::Pixels>>,
    cell: Option<(gpui::Pixels, gpui::Pixels)>,
    drag: Option<Drag>,
    /// Resize actions sent for divider drags (smoke evidence).
    pub(crate) resizes_sent: u32,
    /// The command palette while open, and where its keys go.
    pub(crate) palette: Option<crate::palette::Palette>,
    palette_focus: FocusHandle,
    /// The theme being previewed from the palette, and its pending write.
    theme_preview: Option<(&'static str, gpui::Task<()>)>,
    /// A press on the title bar's empty space: the next move drags the
    /// window (macOS, where the tab strip is the title bar).
    title_drag: bool,
    /// The sidebar (sessions) is open.
    pub(crate) sidebar: bool,
    /// Where the arrangements of all windows are kept, this window's key
    /// there, what it was opened from, and whether the first topology has
    /// been matched against that yet.
    store: Option<Arc<DesktopStore>>,
    state_key: u64,
    saved: Option<DesktopState>,
    restored: bool,
    /// A window opened on a new session waits for it here.
    opening: Opening,
    /// The pane context menu, open where it was asked for.
    menu: Option<gpui::Point<gpui::Pixels>>,
    bounds: Option<[f32; 4]>,
    _bounds: Option<gpui::Subscription>,
    /// Activation reads tmux's key options again (the prefix layer).
    _activation: Option<gpui::Subscription>,
    /// A save is running, and the arrangement changed since it began.
    saving: bool,
    save_dirty: bool,
    /// Filters' marks for the panes shown (local panes only), and the
    /// panes whose marks are switched off in this view.
    decorations: Option<Arc<replica::decorations::DecorationsFeed>>,
    pub(crate) undecorated: std::collections::HashSet<u32>,
    /// An app package drawn as a native panel beside the panes.
    pub(crate) panel: Option<Entity<crate::panel::AppPanel>>,
    /// Installed packages' contributions (local windows only), and the
    /// latest of them drawn.
    contrib: Option<Arc<crate::contrib::Contributions>>,
    pub(crate) contributed: crate::contrib::Snapshot,
    /// The palette's contributed entries, as (package index, command index).
    palette_contributed: Vec<(usize, usize)>,
    /// The harness drawer was asked for: focus it once it shows here (not
    /// the pane it replaces), until the grace runs out.
    drawer_focus: Option<(std::time::Instant, Option<u32>)>,
    /// The pane focused before the drawer took focus, focused again when it
    /// hides.
    drawer_back: Option<u32>,
}

/// A window that shows a new session: what it waits for before choosing
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// The session is chosen as usual: saved, else the first.
    Ready,
    /// The daemon is making the new session.
    Asked,
    /// The new session's first pane, until the topology shows it (or the
    /// grace runs out and the window falls back to choosing as usual).
    Pane(u32, std::time::Instant),
}

/// How long a new session's first pane is waited for in the topology.
const OPENING_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a requested drawer is waited for, to focus it when it shows.
const DRAWER_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// A divider being dragged: the size the pointer asks for, the last size
/// sent to tmux, and whether a throttled send is pending.
#[derive(Debug, Clone, Copy)]
struct Drag {
    divider: Divider,
    size: u32,
    sent: Option<u32>,
    throttling: bool,
}

/// Smallest pane a drag may leave, in cells.
const MIN_PANE: u32 = 2;
/// At most one resize per this interval while dragging.
const DRAG_THROTTLE: std::time::Duration = std::time::Duration::from_millis(60);

impl Workspace {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        topology: Arc<TopologyFeed>,
        socket: PathBuf,
        daemon_args: Vec<String>,
    ) -> Workspace {
        Workspace {
            daemon_args,
            via: replica::Via::Local,
            hosts: None,
            topology,
            socket,
            current: None,
            session: None,
            panes: HashMap::new(),
            hidden: HashMap::new(),
            selecting: None,
            focused: None,
            tmux_active: None,
            focus: cx.focus_handle(),
            title: String::new(),
            area: None,
            requested: None,
            size_pending: false,
            origin: None,
            cell: None,
            drag: None,
            resizes_sent: 0,
            palette: None,
            palette_focus: cx.focus_handle(),
            theme_preview: None,
            title_drag: false,
            sidebar: false,
            store: None,
            state_key: 0,
            saved: None,
            restored: false,
            opening: Opening::Ready,
            menu: None,
            bounds: None,
            _bounds: None,
            _activation: None,
            saving: false,
            save_dirty: false,
            decorations: None,
            undecorated: std::collections::HashSet::new(),
            panel: None,
            contrib: None,
            contributed: crate::contrib::Snapshot::default(),
            palette_contributed: Vec::new(),
            drawer_focus: None,
            drawer_back: None,
        }
    }

    /// Follow installed packages' contributions from `<config dir>/apps`.
    pub(crate) fn follow_contributions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.via != replica::Via::Local || self.contrib.is_some() {
            return;
        }
        let apps = workspace_config::config_dir().map(|d| app_model::installed::apps_dir(&d));
        let taken = crate::keymap::SHORTCUTS
            .iter()
            .filter_map(crate::keymap::Shortcut::keys)
            .collect();
        let feed = Arc::new(crate::contrib::Contributions::start(
            apps,
            self.socket.clone(),
            taken,
        ));
        self.contrib = Some(feed.clone());
        let any = window.window_handle();
        cx.spawn(async move |this, cx| {
            while feed.wake.recv().await.is_ok() {
                let snapshot = feed.snapshot();
                let alive = cx.update_window(any, |_, _, cx| {
                    this.update(cx, |ws, cx| {
                        ws.contributed = snapshot;
                        cx.notify();
                    })
                    .is_ok()
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Run the contributed command at (package, command): its package's
    /// command through the daemon, or open the package as a panel.
    fn run_contributed(&mut self, at: (usize, usize), window: &mut Window, cx: &mut Context<Self>) {
        let Some(pkg) = self.contributed.packages.get(at.0) else {
            return;
        };
        let Some(c) = pkg.manifest.contributes.commands.get(at.1) else {
            return;
        };
        match (&c.run, &self.contrib) {
            (Some(run), Some(feed)) => feed.run(&pkg.manifest.id, run, &c.title),
            _ => {
                let path = pkg.manifest_path.display().to_string();
                self.open_panel(&path, window, cx);
            }
        }
    }

    /// A key press that is a contributed command's binding runs it, before
    /// the focused pane sees it.
    fn contributed_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.contributed.packages.is_empty() {
            return;
        }
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let mut parts: Vec<&str> = Vec::new();
        if m.platform {
            parts.push("cmd");
        }
        if m.control {
            parts.push("ctrl");
        }
        if m.alt {
            parts.push("alt");
        }
        if m.shift {
            parts.push("shift");
        }
        parts.push(k.key.as_str());
        let pressed = app_model::installed::normal_binding(&parts.join("-"));
        let found = self.contributed.commands().into_iter().find(|&(p, c)| {
            self.contributed.packages[p].manifest.contributes.commands[c]
                .key
                .as_deref()
                .is_some_and(|b| app_model::installed::normal_binding(b) == pressed)
        });
        if let Some(at) = found {
            self.run_contributed(at, window, cx);
            cx.stop_propagation();
        }
    }

    /// Follow the marks of the panes shown, starting the feed the first
    /// time. A host's panes have none here: its events are not relayed.
    fn follow_decorations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.via != replica::Via::Local {
            return;
        }
        if self.decorations.is_none() {
            let Ok(feed) = replica::decorations::DecorationsFeed::start(self.socket.clone()) else {
                return;
            };
            let feed = Arc::new(feed);
            self.decorations = Some(feed.clone());
            let any = window.window_handle();
            cx.spawn(async move |this, cx| {
                while feed.wake.recv().await.is_ok() {
                    let alive = cx.update_window(any, |_, _, cx| {
                        this.update(cx, |ws, cx| ws.apply_decorations(cx)).is_ok()
                    });
                    if !matches!(alive, Ok(true)) {
                        return;
                    }
                }
            })
            .detach();
        }
        if let Some(feed) = &self.decorations {
            feed.watch(self.panes.keys().copied());
        }
        self.apply_decorations(cx);
    }

    /// Hand each pane its current marks.
    fn apply_decorations(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.decorations.clone() else {
            return;
        };
        for (id, view) in &self.panes {
            let Some(shell) = view.shell() else { continue };
            let shown = !self.undecorated.contains(id);
            let marks = feed.marks(*id);
            shell.update(cx, |s, cx| s.set_marks(marks, shown, cx));
        }
    }

    /// Show the app package whose manifest is at `path` as a panel, in
    /// place of any panel shown before, and give it the keyboard.
    pub(crate) fn open_panel(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let path = match path.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(rest),
            None => PathBuf::from(path),
        };
        let socket = self.socket.clone();
        let panel = cx.new(|cx| crate::panel::AppPanel::start(path, socket, cx));
        let focus = panel.read(cx).focus.clone();
        self.panel = Some(panel);
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The pane area was painted at `cols` x `rows` cells. When that, or
    /// the window shown, differs from the last request, ask tmux for it
    /// after the size has been still for a moment. Never in reaction to a
    /// topology change: only this view's own size decides (spec 11.8).
    fn area_painted(&mut self, cols: u32, rows: u32, cx: &mut Context<Self>) {
        self.area = Some((cols, rows));
        if self.size_pending {
            return;
        }
        let Some(window) = self.active_window().map(|w| w.id) else {
            return;
        };
        if self.requested == Some((window, cols, rows)) {
            return;
        }
        self.size_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |ws, _| {
                ws.size_pending = false;
                let (Some((cols, rows)), Some(window)) =
                    (ws.area, ws.active_window().map(|w| w.id))
                else {
                    return;
                };
                if ws.requested != Some((window, cols, rows)) {
                    ws.requested = Some((window, cols, rows));
                    ws.topology.size_window(window, cols, rows);
                }
            });
        })
        .detach();
    }

    /// The dividers of the window shown (none while zoomed).
    pub(crate) fn dividers(&self) -> Vec<Divider> {
        if self.active_window().is_some_and(|w| w.zoomed) {
            return Vec::new();
        }
        dividers(&self.visible())
    }

    pub(crate) fn start_drag(&mut self, divider: Divider, cx: &mut Context<Self>) {
        let size = self
            .visible()
            .iter()
            .find(|p| p.id == divider.pane)
            .map_or(MIN_PANE, |p| match divider.axis {
                Axis::Cols => p.width,
                Axis::Rows => p.height,
            });
        self.drag = Some(Drag {
            divider,
            size,
            sent: None,
            throttling: false,
        });
        cx.notify();
    }

    /// The pointer moved during a drag: preview now, resize tmux at most
    /// once per `DRAG_THROTTLE` with the latest size.
    pub(crate) fn drag_to(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let (Some(origin), Some((cw, ch)), Some(drag)) =
            (self.origin, self.cell, self.drag.as_mut())
        else {
            return;
        };
        let d = drag.divider;
        let (pos, cell) = match d.axis {
            Axis::Cols => (at.x - origin.x, cw),
            Axis::Rows => (at.y - origin.y, ch),
        };
        let cells = (f32::from(pos) / f32::from(cell).max(1.0)).floor().max(0.0) as u32;
        let size = cells.saturating_sub(d.base).max(MIN_PANE);
        if size == drag.size {
            return;
        }
        drag.size = size;
        cx.notify();
        if drag.throttling {
            return;
        }
        drag.throttling = true;
        self.send_resize();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAG_THROTTLE).await;
            let _ = this.update(cx, |ws, cx| {
                if let Some(drag) = ws.drag.as_mut() {
                    drag.throttling = false;
                }
                ws.send_resize();
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.send_resize();
        self.drag = None;
        cx.notify();
    }

    /// Send the drag's size if it is not the one last sent. The view keeps
    /// showing tmux's actual layout; only the preview line moves at once.
    fn send_resize(&mut self) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.sent == Some(drag.size) {
            return;
        }
        drag.sent = Some(drag.size);
        let (width, height) = match drag.divider.axis {
            Axis::Cols => (Some(drag.size), None),
            Axis::Rows => (None, Some(drag.size)),
        };
        self.resizes_sent += 1;
        self.topology
            .act(api::action::Kind::ResizePane(api::ResizePane {
                pane: drag.divider.pane,
                width,
                height,
            }));
    }

    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_contributed = self.contributed.commands();
        let labels = self
            .palette_contributed
            .iter()
            .map(|&(p, c)| {
                let m = &self.contributed.packages[p].manifest;
                let cmd = &m.contributes.commands[c];
                match &cmd.key {
                    Some(k) => format!("{}: {} ({k})", m.title, cmd.title),
                    None => format!("{}: {}", m.title, cmd.title),
                }
            })
            .collect();
        let (themes, current) =
            crate::live::with(|l| (l.themes.clone(), Some(l.scheme))).unwrap_or_default();
        self.palette = Some(
            crate::palette::Palette::new()
                .with_contributed(labels)
                .with_themes(themes, current),
        );
        window.focus(&self.palette_focus, cx);
        cx.notify();
    }

    /// Open the palette on a form (editing a profile, or showing why a
    /// submit was refused).
    fn open_form(
        &mut self,
        form: crate::palette::Form,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut p = crate::palette::Palette::new();
        p.stage = crate::palette::Stage::Form(form);
        self.palette = Some(p);
        window.focus(&self.palette_focus, cx);
        cx.notify();
    }

    /// Save a submitted host profile form to hosts.yaml, off the UI
    /// thread. A refused profile reopens the form with the reason; nothing
    /// is written then.
    fn submit_host_form(
        &mut self,
        form: crate::palette::Form,
        here: gpui::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        use workspace_config::hosts::{HostProfile, hosts_path, load_hosts, put_host};
        let value = |i: usize| form.fields[i].1.trim().to_owned();
        let optional = |i: usize| Some(value(i)).filter(|v| !v.is_empty());
        let (name, destination, dream, socket) = (value(0), value(1), optional(2), optional(3));
        let tags: Vec<String> = value(4)
            .split(',')
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        let replacing = form.replacing.clone();
        let any = cx.entity();
        let task = cx.background_executor().spawn(async move {
            let path = hosts_path().ok_or("no configuration directory (HOME unset)")?;
            // An edit keeps what the form does not show.
            let mut profile = replacing
                .as_deref()
                .and_then(|r| load_hosts().ok()?.into_iter().find(|p| p.name == r))
                .unwrap_or(HostProfile {
                    name: String::new(),
                    destination: String::new(),
                    dream: None,
                    tmux: None,
                    tmux_socket: None,
                    tmux_socket_path: None,
                    workspace: None,
                    tags: Vec::new(),
                    reconnect: Default::default(),
                });
            profile.name = name;
            profile.destination = destination;
            profile.dream = dream;
            profile.tmux_socket = socket;
            if profile.tmux_socket.is_some() {
                profile.tmux_socket_path = None;
            }
            profile.tags = tags;
            put_host(&path, profile, replacing.as_deref()).map_err(|e| e.to_string())
        });
        cx.spawn(async move |_, cx| {
            let result = task.await;
            cx.update(|cx| {
                any.update(cx, |ws, cx| {
                    if let Err(why) = result {
                        let mut form = form;
                        form.error = Some(why);
                        let _ = here.update(cx, |_, window, cx| {
                            any.update(cx, |ws, cx| ws.open_form(form, window, cx))
                        });
                    }
                    ws.hosts_changed(cx);
                })
            });
        })
        .detach();
    }

    /// Open the profile form for `name`, filled from hosts.yaml (read off
    /// the UI thread).
    fn edit_host(&mut self, name: String, here: gpui::AnyWindowHandle, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async move {
            workspace_config::hosts::load_hosts()
                .ok()
                .and_then(|ps| ps.into_iter().find(|p| p.name == name))
        });
        let any = cx.entity();
        cx.spawn(async move |_, cx| {
            let Some(p) = task.await else { return };
            let form = crate::palette::Form::host(
                crate::palette::Command::EditHost,
                [
                    p.name.clone(),
                    p.destination.clone(),
                    p.dream.clone().unwrap_or_default(),
                    p.tmux_socket.clone().unwrap_or_default(),
                    p.tags.join(", "),
                ],
                Some(p.name),
            );
            cx.update(|cx| {
                let _ = here.update(cx, |_, window, cx| {
                    any.update(cx, |ws, cx| ws.open_form(form, window, cx))
                });
            });
        })
        .detach();
    }

    /// The sidebar's host list is read again on its next tick; nothing to
    /// do beyond a redraw now.
    fn hosts_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving the theme list without choosing puts everything back.
        if self.theme_preview.take().is_some() {
            crate::theme_pick::cancel(cx);
        }
        self.palette = None;
        if let Some(view) = self.focused.and_then(|f| self.panes.get(&f)) {
            let handle = view.focus(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Enter in the palette: run the command, or move on to its confirm or
    /// name step.
    pub(crate) fn palette_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::palette::Decision;
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        match p.enter() {
            Decision::Run(cmd, name) => {
                self.close_palette(window, cx);
                self.run_command(cmd, name, window, cx);
            }
            Decision::Submit(form) => {
                self.close_palette(window, cx);
                self.submit_host_form(form, window.window_handle(), cx);
            }
            Decision::Theme(scheme) => {
                self.theme_preview = None;
                self.close_palette(window, cx);
                crate::theme_pick::keep(cx, scheme);
            }
            Decision::Stay => {}
        }
        cx.notify();
    }

    fn palette_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let k = &ev.keystroke;
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        match k.key.as_str() {
            "escape" => return self.close_palette(window, cx),
            "enter" => return self.palette_enter(window, cx),
            "backspace" => p.backspace(),
            "up" => p.move_selection(false),
            "down" => p.move_selection(true),
            _ => {
                let m = &k.modifiers;
                if !(m.control || m.platform || m.alt)
                    && let Some(text) = &k.key_char
                {
                    text.chars().for_each(|c| p.type_char(c));
                }
            }
        }
        self.preview_theme(cx);
        cx.stop_propagation();
        cx.notify();
    }

    /// Preview the theme highlighted in the palette, if it changed. The
    /// one in force when the list opened is not a change.
    fn preview_theme(&mut self, cx: &mut Context<Self>) {
        let Some(p) = &self.palette else {
            return;
        };
        let Some(scheme) = p.previewed() else {
            return;
        };
        let before = match &self.theme_preview {
            Some((name, _)) => Some(*name),
            None => p.current.map(|s| s.name),
        };
        if before != Some(scheme.name) {
            let task = crate::theme_pick::preview(cx, scheme);
            self.theme_preview = Some((scheme.name, task));
        }
    }

    /// Run a palette command. Every effect is seen in a newer topology;
    /// "Close view" closes only this window.
    pub(crate) fn run_command(
        &mut self,
        cmd: crate::palette::Command,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::palette::Command as C;
        use api::action::Kind;
        if let Some(action) = cmd.app_action() {
            window.dispatch_action(action, cx);
            cx.notify();
            return;
        }
        let pane = self.focused;
        let win = self.active_window().map(|w| w.id);
        let mut windows: Vec<(u32, u32)> = match (&self.current, self.session) {
            (Some(t), Some(s)) => t
                .windows
                .iter()
                .filter(|w| w.sessions.contains(&s))
                .map(|w| (w.index, w.id))
                .collect(),
            _ => Vec::new(),
        };
        windows.sort_unstable();
        let step = |by: isize| -> Option<u32> {
            let i = windows.iter().position(|(_, id)| Some(*id) == win)?;
            let n = windows.len() as isize;
            Some(windows[((i as isize + by).rem_euclid(n)) as usize].1)
        };
        let split = |direction: api::SplitDirection| {
            pane.map(|pane| {
                Kind::SplitPane(api::SplitPane {
                    pane,
                    direction: direction as i32,
                    ..api::SplitPane::default()
                })
            })
        };
        let action = match cmd {
            C::SplitRight => split(api::SplitDirection::Horizontal),
            C::SplitDown => split(api::SplitDirection::Vertical),
            C::NewWindow => self.session.map(|session| {
                Kind::NewWindow(api::NewWindow {
                    session,
                    ..api::NewWindow::default()
                })
            }),
            C::RenameWindow => win
                .zip(name)
                .map(|(window, name)| Kind::RenameWindow(api::RenameWindow { window, name })),
            C::NextWindow => step(1).map(|window| Kind::SelectWindow(api::SelectWindow { window })),
            C::PreviousWindow => {
                step(-1).map(|window| Kind::SelectWindow(api::SelectWindow { window }))
            }
            C::NextPane => {
                let ids = self.pane_ids();
                let next = pane
                    .and_then(|p| ids.iter().position(|i| *i == p))
                    .map(|i| ids[(i + 1) % ids.len()])
                    .or(ids.first().copied());
                if let Some(next) = next {
                    self.click_pane(next, window, cx);
                }
                None
            }
            C::Zoom => {
                self.toggle_zoom(cx);
                None
            }
            C::CloseView => {
                self.forget_state(cx);
                window.remove_window();
                None
            }
            C::KillPane => pane.map(|pane| Kind::KillPane(api::KillPane { pane })),
            C::KillWindow => win.map(|window| Kind::KillWindow(api::KillWindow { window })),
            C::ToggleSidebar => {
                self.toggle_sidebar(cx);
                None
            }
            C::ToggleDrawer => {
                self.toggle_drawer(false, window, cx);
                None
            }
            C::NewDrawer => {
                self.toggle_drawer(true, window, cx);
                None
            }
            C::OpenHost => {
                if let Some(name) = name {
                    self.open_host(&name, cx);
                }
                None
            }
            C::ReconnectHost => {
                if let (Some(name), Some(h)) = (name, self.hosts_feed()) {
                    h.reconnect(&name);
                }
                None
            }
            C::AddHost => None,
            C::EditHost => {
                if let Some(name) = name {
                    self.edit_host(name, window.window_handle(), cx);
                }
                None
            }
            C::RemoveHost => {
                if let Some(name) = name {
                    cx.background_executor()
                        .spawn(async move {
                            if let Some(path) = workspace_config::hosts::hosts_path()
                                && let Err(e) = workspace_config::hosts::remove_host(&path, &name)
                            {
                                eprintln!("dreamer: {e}");
                            }
                        })
                        .detach();
                }
                None
            }
            C::InstallOnHost => {
                if let Some(name) = name {
                    self.install_on_host(&name, cx);
                }
                None
            }
            C::VerifyHost => {
                if let Some(name) = name {
                    self.verify_host(&name, cx);
                }
                None
            }
            C::DisconnectHost => {
                if let (Some(name), Some(h)) = (name, self.hosts_feed()) {
                    h.disconnect(&name);
                }
                None
            }
            C::ToggleDecorations => {
                if let Some(pane) = pane
                    && !self.undecorated.remove(&pane)
                {
                    self.undecorated.insert(pane);
                }
                self.apply_decorations(cx);
                None
            }
            C::OpenPanel => {
                if let Some(path) = name {
                    self.open_panel(&path, window, cx);
                }
                None
            }
            C::ClosePanel => {
                self.close_panel(window, cx);
                None
            }
            C::OpenWebPage => {
                if let Some(url) = name.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty()) {
                    self.topology.web_open(api::WebOpen {
                        url,
                        pane,
                        session: if pane.is_none() { self.session } else { None },
                        ..api::WebOpen::default()
                    });
                }
                None
            }
            // Chosen in the palette's own list.
            C::Theme => None,
            // The app's own commands, dispatched above.
            C::NewOsWindow
            | C::Settings
            | C::About
            | C::Copy
            | C::Paste
            | C::Find
            | C::FontBigger
            | C::FontSmaller
            | C::FontReset
            | C::FullScreen
            | C::Minimize
            | C::ZoomOsWindow
            | C::BringAllToFront
            | C::Quit => None,
            C::Contributed(i) => {
                if let Some(&at) = self.palette_contributed.get(usize::from(i)) {
                    self.run_contributed(at, window, cx);
                }
                None
            }
        };
        if let Some(kind) = action {
            self.topology.act(kind);
        }
        cx.notify();
    }

    /// Zoom the focused pane, or unzoom; the view follows the topology.
    pub(crate) fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.focused {
            self.topology
                .act(api::action::Kind::ZoomPane(api::ZoomPane { pane }));
            cx.notify();
        }
    }

    /// The session's harness drawer: show and focus it, focus it if shown,
    /// hide it if focused (focus goes back where it was), or with `new` a
    /// fresh harness. The daemon moves the pane; this view follows the
    /// topology.
    pub(crate) fn toggle_drawer(&mut self, new: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(here)) = (self.session, self.active_window().map(|w| w.id)) else {
            return;
        };
        let drawer = self.current.as_ref().and_then(|t| {
            t.panes
                .iter()
                .find(|p| p.drawer_of == Some(session))
                .map(|p| (p.id, p.window))
        });
        let want = match drawer {
            _ if new => api::DrawerWant::New,
            Some((id, w)) if w == here && self.focused == Some(id) => api::DrawerWant::Hide,
            Some((id, w)) if w == here => {
                if self.panes.contains_key(&id) {
                    self.drawer_back = self.focused;
                    self.click_pane(id, window, cx);
                    return;
                }
                api::DrawerWant::Show
            }
            _ => api::DrawerWant::Show,
        };
        let id = drawer.map(|(id, _)| id);
        if want == api::DrawerWant::Hide {
            self.drawer_focus = None;
            let back = self
                .drawer_back
                .take()
                .filter(|b| self.panes.contains_key(b));
            if back.is_some() {
                self.set_focus(back, window, cx);
            }
        } else {
            if self.focused.is_some() && self.focused != id {
                self.drawer_back = self.focused;
            }
            // A new drawer replaces the old pane: wait for the other one.
            let skip = if new { id } else { None };
            self.drawer_focus = Some((std::time::Instant::now(), skip));
        }
        self.topology.drawer(api::DrawerRequest {
            session: Some(session),
            window: Some(here),
            want: want as i32,
            ..api::DrawerRequest::default()
        });
        cx.notify();
    }

    /// Focus the drawer asked for once it is shown here.
    fn focus_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((at, skip)) = self.drawer_focus else {
            return;
        };
        let session = self.session;
        let shown = self.current.as_ref().and_then(|t| {
            t.panes
                .iter()
                .find(|p| session.is_some() && p.drawer_of == session && Some(p.id) != skip)
                .map(|p| p.id)
        });
        if let Some(id) = shown.filter(|id| self.panes.contains_key(id)) {
            self.drawer_focus = None;
            self.set_focus(Some(id), window, cx);
        } else if at.elapsed() >= DRAWER_GRACE {
            self.drawer_focus = None;
        }
    }

    pub(crate) fn zoomed(&self) -> bool {
        self.active_window().is_some_and(|w| w.zoomed)
    }

    /// Where pane `pane` is drawn in this window now (window coordinates),
    /// and for an app pane the version of its view tree in the frame drawn
    /// last; `None` when this window does not show it.
    pub(crate) fn pane_shown(
        &self,
        pane: u32,
        cx: &gpui::App,
    ) -> Option<(gpui::Bounds<gpui::Pixels>, Option<u64>)> {
        let p = self.visible().into_iter().find(|p| p.id == pane)?;
        let (origin, (cw, ch)) = (self.origin?, self.cell?);
        let w = self.active_window()?;
        let (left, top, cols, rows) = if w.zoomed {
            (0, 0, w.width, w.height)
        } else {
            (p.left, p.top, p.width, p.height)
        };
        let bounds = gpui::Bounds::new(
            gpui::point(origin.x + cw * left as f32, origin.y + ch * top as f32),
            gpui::size(cw * cols as f32, ch * rows as f32),
        );
        let painted = match self.panes.get(&pane)? {
            PaneView::App { view, .. } => Some(view.read(cx).painted),
            PaneView::Terminal(_) | PaneView::Web { .. } => None,
        };
        Some((bounds, painted))
    }

    pub(crate) fn pane_ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.panes.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub(crate) fn pane_shell(&self, pane: u32) -> Option<Entity<Shell>> {
        self.panes.get(&pane).and_then(|p| p.shell().cloned())
    }

    pub(crate) fn topology(&self) -> Option<&api::Topology> {
        self.current.as_ref()
    }

    /// Remember where the arrangements are kept, this window's key there,
    /// and the entry it was opened from (none for a new window). The entry
    /// stays in the store until this window saves its own.
    pub(crate) fn with_state(
        &mut self,
        store: Option<Arc<DesktopStore>>,
        key: u64,
        saved: Option<DesktopState>,
    ) {
        self.sidebar = saved.as_ref().is_some_and(|s| s.sidebar);
        self.bounds = saved.as_ref().and_then(|s| s.bounds);
        if let (Some(store), Some(s)) = (&store, &saved) {
            store.set(key, s.clone());
        }
        self.saved = saved;
        self.store = store;
        self.state_key = key;
    }

    /// The arrangement as it is now; none while a new session is awaited.
    fn snapshot(&self) -> Option<DesktopState> {
        let t = self.current.as_ref()?;
        if self.opening != Opening::Ready {
            return None;
        }
        let session = t.sessions.iter().find(|s| Some(s.id) == self.session);
        Some(DesktopState {
            endpoint: t.endpoint.clone(),
            incarnation: t.incarnation,
            session_id: session.map(|s| s.id),
            session_name: session.map(|s| s.name.clone()),
            sidebar: self.sidebar,
            bounds: self.bounds,
        })
    }

    /// This window was closed: it is not opened again at the next launch
    /// (unless it was the last one). Written off the UI thread.
    pub(crate) fn forget_state(&self, cx: &mut gpui::App) {
        forget(self.store.clone(), self.state_key, cx);
    }

    /// Save the arrangement off the UI thread, one write at a time: the
    /// writer takes the latest state when it writes, and writes again if
    /// it changed meanwhile, so an older state never lands last.
    fn save_state(&mut self, cx: &mut Context<Self>) {
        self.save_dirty = true;
        if self.saving || self.store.is_none() {
            return;
        }
        self.saving = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let Ok(next) = this.update(cx, |ws, _| {
                    ws.save_dirty = false;
                    let store = ws.store.clone()?;
                    store.set(ws.state_key, ws.snapshot()?);
                    Some(store)
                }) else {
                    return;
                };
                // The store writes every window's entry, one write at a
                // time, so windows saving at once keep each other's.
                if let Some(store) = next {
                    cx.background_spawn(async move {
                        let _ = store.save();
                    })
                    .await;
                }
                let again = this.update(cx, |ws, _| {
                    if ws.save_dirty {
                        true
                    } else {
                        ws.saving = false;
                        false
                    }
                });
                if !matches!(again, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    pub(crate) fn window_moved(&mut self, bounds: [f32; 4], cx: &mut Context<Self>) {
        if self.bounds != Some(bounds) {
            self.bounds = Some(bounds);
            self.save_state(cx);
        }
    }

    /// Show another session in this window.
    pub(crate) fn switch_session(
        &mut self,
        session: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session == Some(session) {
            return;
        }
        self.session = Some(session);
        self.focused = None;
        if let Some(t) = self.current.clone() {
            self.update_topology(t, window, cx);
        }
        self.save_state(cx);
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar = !self.sidebar;
        self.follow_hosts(cx);
        self.save_state(cx);
        cx.notify();
    }

    /// Keep a hosts feed while the sidebar is open, and redraw on news.
    fn follow_hosts(&mut self, cx: &mut Context<Self>) {
        if !self.sidebar {
            self.hosts = None;
            return;
        }
        if self.hosts.is_some() {
            return;
        }
        let Ok(feed) = replica::hosts::HostsFeed::start(self.socket.clone()) else {
            return;
        };
        let feed = Arc::new(feed);
        self.hosts = Some(feed.clone());
        cx.spawn(async move |ws, cx| {
            while feed.wake.recv().await.is_ok() {
                let alive = ws.update(cx, |ws, cx| {
                    // A newer feed replaced this one, or the sidebar closed.
                    let current = ws.hosts.as_ref().is_some_and(|h| Arc::ptr_eq(h, &feed));
                    cx.notify();
                    current
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// The hosts feed, starting one for a single command if the sidebar
    /// has none.
    fn hosts_feed(&mut self) -> Option<Arc<replica::hosts::HostsFeed>> {
        if let Some(h) = &self.hosts {
            return Some(h.clone());
        }
        replica::hosts::HostsFeed::start(self.socket.clone())
            .ok()
            .map(Arc::new)
    }

    /// A visible first connection to a host: a local tmux window running
    /// `dream hosts verify NAME`, so SSH's questions (an unknown host key,
    /// a passphrase) are asked in an ordinary pane and answered by the
    /// user. Dreamer never answers them. Only from a local window: a host's
    /// window would open it on the host.
    pub(crate) fn verify_host(&mut self, name: &str, cx: &mut Context<Self>) {
        self.host_terminal(name, &["verify"], cx);
    }

    /// Install this version's server on a host, in a local tmux window
    /// that shows the plan and asks before installing.
    pub(crate) fn install_on_host(&mut self, name: &str, cx: &mut Context<Self>) {
        // `upgrade` installs where nothing is, and restarts an older daemon.
        self.host_terminal(name, &["upgrade"], cx);
    }

    /// A local tmux window running `dream hosts <what> NAME --wait`.
    fn host_terminal(&mut self, name: &str, what: &[&str], cx: &mut Context<Self>) {
        if self.via != replica::Via::Local {
            return;
        }
        let Some(session) = self.session else { return };
        let dream = std::env::current_exe()
            .ok()
            .map(|e| e.with_file_name("dream"))
            .filter(|p| p.exists())
            .map_or_else(|| "dream".to_owned(), |p| p.display().to_string());
        self.topology
            .act(api::action::Kind::NewWindow(api::NewWindow {
                session,
                name: Some(format!("{} {name}", what.first().copied().unwrap_or("ssh"))),
                argv: [dream, "hosts".into()]
                    .into_iter()
                    .chain(what.iter().map(|w| (*w).to_owned()))
                    .chain([name.to_owned(), "--wait".into()])
                    .collect(),
                cwd: None,
            }));
        cx.notify();
    }

    /// Open a window on a host's tmux, connecting the host first unless it
    /// is live.
    pub(crate) fn open_host(&mut self, name: &str, cx: &mut Context<Self>) {
        let launch = client::Launch::new(self.daemon_args.clone());
        replica::hosts::connect_once(launch.clone(), name.to_owned());
        if let Err(e) = open_via(
            cx,
            launch,
            replica::Via::Host(name.to_owned()),
            None,
            Vec::new(),
            Start::Saved(None),
        )
        .map(drop)
        {
            eprintln!("dreamer: {e}");
        }
    }

    /// The sidebar's hosts: each with its state; a click opens a window on
    /// it (connecting first). Nothing when the daemon knows no hosts.
    /// Installed packages' sidebar entries, and what was refused.
    fn apps_section(
        &self,
        t: crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let snap = &self.contributed;
        let entries: Vec<(String, String, String)> = snap
            .packages
            .iter()
            .flat_map(|p| {
                p.manifest.contributes.sidebar.iter().map(|e| {
                    (
                        format!("{}.{}", p.manifest.id, e.id),
                        e.title.clone(),
                        p.manifest_path.display().to_string(),
                    )
                })
            })
            .collect();
        if entries.is_empty() && snap.problems.is_empty() {
            return Vec::new();
        }
        let mut out = vec![
            div()
                .pt_2()
                .text_color(c(t.faint))
                .child("Apps")
                .into_any_element(),
        ];
        for (i, (_, title, path)) in entries.into_iter().enumerate() {
            out.push(
                div()
                    .id(("app-entry", i))
                    .px_2()
                    .rounded_sm()
                    .text_color(c(t.muted))
                    .child(title)
                    .on_click(
                        cx.listener(move |ws, _, window, cx| ws.open_panel(&path, window, cx)),
                    )
                    .into_any_element(),
            );
        }
        for p in &snap.problems {
            out.push(
                div()
                    .px_2()
                    .text_color(c(t.danger))
                    .child(p.clone())
                    .into_any_element(),
            );
        }
        out
    }

    fn hosts_section(
        &self,
        t: crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(feed) = &self.hosts else {
            return Vec::new();
        };
        let list = feed.list().unwrap_or_default();
        let error = feed.error();
        if list.hosts.is_empty() && error.is_none() {
            return Vec::new();
        }
        let mut out: Vec<gpui::AnyElement> = vec![
            div()
                .mt_2()
                .text_color(c(t.faint))
                .child("Hosts")
                .into_any_element(),
        ];
        for (i, h) in list.hosts.into_iter().enumerate() {
            let state = api::LinkState::try_from(h.state).unwrap_or(api::LinkState::Unspecified);
            let (mark, color, word) = match state {
                api::LinkState::Live => ("●", t.accent, "live".to_owned()),
                api::LinkState::Connecting => ("◐", t.warning, "connecting".to_owned()),
                api::LinkState::Failed => (
                    "○",
                    t.danger,
                    match h.next_retry_unix_ms {
                        Some(_) => format!("retrying ({})", h.attempts),
                        None => api::HostFailure::try_from(h.failure)
                            .map_or("failed".into(), |f| f.name().replace('_', " ")),
                    },
                ),
                api::LinkState::Closed => ("○", t.faint, "disconnected".to_owned()),
                api::LinkState::Unspecified => ("○", t.faint, "not connected".to_owned()),
            };
            let here = self.via.host() == Some(h.name.as_str());
            let name = h.name.clone();
            // A host that failed on authentication or its host key needs
            // the user's answer: a click opens the visible connection.
            // A host whose server is missing or does not match: a click
            // installs this version's server there, after review.
            let failure = (h.state == api::LinkState::Failed as i32
                && self.via == replica::Via::Local)
                .then(|| api::HostFailure::try_from(h.failure).ok())
                .flatten();
            let needs_user = matches!(
                failure,
                Some(api::HostFailure::Auth | api::HostFailure::HostKey)
            );
            let needs_install = matches!(
                failure,
                Some(api::HostFailure::NotInstalled | api::HostFailure::Incompatible)
            );
            let word = if needs_user {
                format!("{word}: click to verify")
            } else if needs_install {
                format!("{word}: click to install")
            } else {
                word
            };
            out.push(
                div()
                    .id(("host", i))
                    .px_2()
                    .rounded_sm()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .when(here, |d| d.bg(c(t.selected)))
                    .child(div().text_color(c(color)).child(mark))
                    .child(div().text_color(c(t.text)).child(h.name.clone()))
                    .child(div().text_color(c(t.faint)).child(word))
                    .on_click(cx.listener(move |ws, _, _, cx| {
                        if needs_user {
                            ws.verify_host(&name, cx);
                        } else if needs_install {
                            ws.install_on_host(&name, cx);
                        } else {
                            ws.open_host(&name, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }
        if let Some(e) = error {
            out.push(div().text_color(c(t.danger)).child(e).into_any_element());
        }
        out
    }

    /// The tmux mode a pane is in (copy-mode, ...), which Dreamer does not
    /// draw.
    pub(crate) fn pane_mode(&self, pane: u32) -> Option<String> {
        self.current
            .as_ref()?
            .panes
            .iter()
            .find(|p| p.id == pane)?
            .mode
            .clone()
    }

    /// The attach command for the shown session and a window of it.
    pub(crate) fn attach_for(&self, window: Option<u32>) -> Option<crate::fallback::Attach> {
        // A host's tmux is not reachable with a local tmux command.
        if self.via != replica::Via::Local {
            return None;
        }
        Some(crate::fallback::attach_command(
            &self.daemon_args,
            self.session?,
            window,
        ))
    }

    /// The command, a Copy button, and (when configured) Open in terminal.
    fn fallback_ui(
        &self,
        t: crate::theme::Theme,
        id: usize,
        attach: crate::fallback::Attach,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let copy = attach.display.clone();
        let launch = crate::live::with(|l| l.fallback.clone())
            .and_then(|t| crate::fallback::launch_argv(&t, &attach));
        // The command on a line of its own, wrapped rather than cut at the
        // pane's edge, with the buttons below it.
        let copy_button = div()
            .id(("fallback-copy", id))
            .px_1()
            .rounded_sm()
            .bg(c(t.selected))
            .child("Copy")
            .on_click(cx.listener(move |_, _, _, cx| crate::clip::write(cx, copy.clone())));
        let open_button = launch.map(|argv| {
            div()
                .id(("fallback-open", id))
                .px_1()
                .rounded_sm()
                .bg(c(t.selected))
                .child("Open in terminal")
                .on_click(cx.listener(move |_, _, _, cx| {
                    let argv = argv.clone();
                    // Exact argv, never a shell string; off the UI thread.
                    cx.background_spawn(async move {
                        if let Some((program, args)) = argv.split_first() {
                            let _ = std::process::Command::new(program).args(args).spawn();
                        }
                    })
                    .detach();
                }))
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .px_1()
                    .bg(c(t.surface))
                    .font_family(term_font().family)
                    .child(attach.display.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(copy_button)
                    .children(open_button),
            )
    }

    /// Windows in the session shown: the tabs.
    pub(crate) fn tab_count(&self) -> usize {
        match (&self.current, self.session) {
            (Some(t), Some(s)) => t.windows.iter().filter(|w| w.sessions.contains(&s)).count(),
            _ => 0,
        }
    }

    pub(crate) fn session_name(&self) -> Option<String> {
        let t = self.current.as_ref()?;
        t.sessions
            .iter()
            .find(|s| Some(s.id) == self.session)
            .map(|s| s.name.clone())
    }

    /// The window the session shows now, if the topology has one.
    fn active_window(&self) -> Option<&api::Window> {
        let t = self.current.as_ref()?;
        let s = t.sessions.iter().find(|s| Some(s.id) == self.session)?;
        if let Some((w, at)) = self.selecting
            && at.elapsed() < SELECT_GRACE
            && let Some(w) = t
                .windows
                .iter()
                .find(|x| x.id == w && x.sessions.contains(&s.id))
        {
            return Some(w);
        }
        t.windows.iter().find(|w| Some(w.id) == s.active_window)
    }

    /// The panes drawn now: all of the active window's panes, or only its
    /// active pane when zoomed. Empty for an unsupported layout.
    fn visible(&self) -> Vec<api::Pane> {
        let (Some(t), Some(w)) = (self.current.as_ref(), self.active_window()) else {
            return Vec::new();
        };
        if !w.layout_supported {
            return Vec::new();
        }
        t.panes
            .iter()
            .filter(|p| p.window == w.id && (!w.zoomed || Some(p.id) == w.active_pane))
            .cloned()
            .collect()
    }

    /// Take a newer topology and follow it: views for newly visible panes,
    /// none for panes no longer shown (their feeds stop with them).
    pub(crate) fn update_topology(
        &mut self,
        t: api::Topology,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A sidebar restored open shows hosts too.
        if self.sidebar && self.hosts.is_none() {
            self.follow_hosts(cx);
        }
        if let Opening::Pane(pane, since) = self.opening {
            // A new window: the new session, once the topology has it.
            if let Some(s) = session_of_pane(&t, pane) {
                self.session = Some(s);
                self.opening = Opening::Ready;
                self.restored = true;
                self.save_state(cx);
            } else if since.elapsed() >= OPENING_GRACE {
                self.opening = Opening::Ready;
            }
        }
        if self.opening != Opening::Ready {
            // Waiting for the new session: show none rather than another.
        } else if !self.restored {
            // The first topology: the saved session, by id only for the same
            // server incarnation, else by name (spec 15.2: never reopen a
            // stale id from another server epoch).
            // The holding session of hidden drawers is never shown.
            let sessions: Vec<(u32, &str)> = t
                .sessions
                .iter()
                .filter(|s| !s.holding)
                .map(|s| (s.id, s.name.as_str()))
                .collect();
            self.session = workspace_config::desktop_state::choose_session(
                self.saved.as_ref(),
                &t.endpoint,
                t.incarnation,
                &sessions,
            );
            self.restored = true;
        } else if self
            .session
            .is_none_or(|s| !t.sessions.iter().any(|x| x.id == s && !x.holding))
        {
            self.session = t.sessions.iter().find(|s| !s.holding).map(|s| s.id);
        }
        let first_save = self.current.is_none();
        // tmux confirmed the clicked tab, or the grace ran out: from here
        // on its own active window is shown.
        if let Some((w, at)) = self.selecting {
            let confirmed = t
                .sessions
                .iter()
                .find(|s| Some(s.id) == self.session)
                .is_some_and(|s| s.active_window == Some(w));
            if confirmed || at.elapsed() >= SELECT_GRACE {
                self.selecting = None;
            }
        }
        self.current = Some(t);
        self.sync_panes(window, cx);
        self.follow_active(window, cx);
        self.focus_drawer(window, cx);
        self.follow_decorations(window, cx);
        self.follow_contributions(window, cx);
        if first_save {
            // Record the server incarnation the session id now belongs to.
            self.save_state(cx);
        }
        cx.notify();
    }

    /// A view for pane `p`: its app, drawn natively, when its program is a
    /// registered app host; otherwise its terminal.
    fn open_pane(
        &self,
        p: &api::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneView> {
        let pane = p.id;
        if let Some(url) = web_drawn(p, &self.via) {
            let (url, ws) = (url.to_owned(), cx.entity().downgrade());
            let kept = (p.title.clone(), p.web_scroll_x, p.web_scroll_y);
            let view = cx.new(|cx| crate::web::WebPane::new(pane, url.clone(), kept, ws, cx));
            return Some(PaneView::Web { view, url });
        }
        if let Some(instance) = app_drawn(p, &self.via) {
            let (instance, socket) = (instance.to_owned(), self.socket.clone());
            let view =
                cx.new(|cx| crate::panel::AppPanel::follow(pane, instance.clone(), socket, cx));
            return Some(PaneView::App { view, instance });
        }
        let feed = Feed::start_via(self.socket.clone(), self.via.clone(), Some(pane), None)
            .ok()
            .map(Arc::new);
        let frame = load_frame(b"").ok()?;
        let shell = cx.new(|cx| Shell::new(cx, Smoke::Off, frame, feed.clone(), true));
        if let Some(feed) = feed {
            let any = window.window_handle();
            let weak = shell.downgrade();
            cx.spawn(async move |_, cx| {
                while feed.wake.recv().await.is_ok() {
                    let alive = cx.update_window(any, |_, window, cx| {
                        weak.update(cx, |s, cx| s.take_feed(window, cx)).is_ok()
                    });
                    if !matches!(alive, Ok(true)) {
                        return;
                    }
                }
            })
            .detach();
        }
        Some(PaneView::Terminal(shell))
    }

    /// Move keyboard focus to `pane`, and tell the panes' programs (focus
    /// reports, and which window the daemon addresses clipboard writes to).
    fn set_focus(&mut self, pane: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        if pane == self.focused {
            return;
        }
        let report = |id: Option<u32>, gained: bool, cx: &mut Context<Self>| {
            if let Some(shell) = id.and_then(|i| self.panes.get(&i)?.shell())
                && let Some(feed) = shell.read(cx).feed.clone()
            {
                feed.send(api::terminal_input::Event::Focus(api::FocusInput {
                    gained,
                }));
            }
        };
        report(self.focused, false, cx);
        if let Some(web) = self.focused.and_then(|f| self.panes.get(&f)?.web_pane()) {
            web.read(cx).release_focus();
        }
        self.focused = pane;
        report(pane, true, cx);
        if let Some(web) = pane.and_then(|p| self.panes.get(&p)?.web_pane()) {
            web.read(cx).take_focus();
        }
        if let Some(view) = pane.and_then(|p| self.panes.get(&p)) {
            let handle = view.focus(cx);
            window.focus(&handle, cx);
        }
    }

    /// tmux selected another pane of the shown window (`select-pane` from
    /// any client, or the prefix and an arrow typed here, in a terminal or
    /// an app pane): focus follows it, as it would in tmux.
    fn follow_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active_window().and_then(|w| w.active_pane);
        if active == self.tmux_active {
            return;
        }
        self.tmux_active = active;
        if self.drawer_focus.is_some() {
            return;
        }
        if let Some(a) = active.filter(|a| self.panes.contains_key(a)) {
            self.set_focus(Some(a), window, cx);
        }
    }

    /// A web pane's page took focus (a click in it): it becomes the
    /// focused pane, here and in tmux.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn page_focused(&mut self, pane: u32, window: &mut Window, cx: &mut Context<Self>) {
        let clicked = self
            .panes
            .get(&pane)
            .and_then(PaneView::web_pane)
            .is_some_and(|w| w.read(cx).page_has_focus());
        if self.focused != Some(pane) && clicked {
            self.click_pane(pane, window, cx);
        }
    }

    /// A click on a pane: focus it here and make it tmux's active pane.
    pub(crate) fn click_pane(&mut self, pane: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.set_focus(Some(pane), window, cx);
        self.topology
            .act(api::action::Kind::SelectPane(api::SelectPane { pane }));
        cx.notify();
    }

    /// The session's windows (tabs) by index, and the active one.
    pub(crate) fn tabs(&self) -> (Vec<u32>, Option<u32>) {
        let mut tabs: Vec<&api::Window> = match (&self.current, self.session) {
            (Some(t), Some(s)) => t
                .windows
                .iter()
                .filter(|w| w.sessions.contains(&s))
                .collect(),
            _ => Vec::new(),
        };
        tabs.sort_by_key(|w| w.index);
        (
            tabs.iter().map(|w| w.id).collect(),
            self.active_window().map(|w| w.id),
        )
    }

    /// Views for the panes shown now; hidden ones keep theirs for a while.
    fn sync_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible();
        // Panes no longer shown keep their views for a while (hidden);
        // panes that no longer exist lose them.
        let exists = |id: &u32, t: &Option<api::Topology>| {
            t.as_ref()
                .is_some_and(|t| t.panes.iter().any(|p| p.id == *id))
        };
        let gone: Vec<u32> = self
            .panes
            .keys()
            .copied()
            .filter(|id| !visible.iter().any(|p| p.id == *id))
            .collect();
        let now = std::time::Instant::now();
        for id in gone {
            if let Some(view) = self.panes.remove(&id)
                && exists(&id, &self.current)
            {
                self.hidden.insert(id, (view, now));
            }
        }
        let current = self.current.clone();
        // A view that no longer fits its pane goes: the pane's program
        // became an app host, or its app went away.
        let fits = |id: &u32, view: &PaneView| {
            current
                .as_ref()
                .and_then(|t| t.panes.iter().find(|p| p.id == *id))
                .is_some_and(|p| {
                    app_drawn(p, &self.via) == view.app() && web_drawn(p, &self.via) == view.web()
                })
        };
        self.hidden
            .retain(|id, (view, _)| exists(id, &current) && fits(id, view));
        let stale: Vec<u32> = self
            .panes
            .iter()
            .filter(|(id, view)| !fits(id, view))
            .map(|(id, _)| *id)
            .collect();
        let mut refocus = false;
        for id in stale {
            self.panes.remove(&id);
            refocus |= self.focused == Some(id);
        }
        while self.hidden.len() > HIDDEN_VIEWS {
            let Some(oldest) = self
                .hidden
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.hidden.remove(&oldest);
        }
        for p in &visible {
            if self.panes.contains_key(&p.id) {
                continue;
            }
            if let Some((view, _)) = self.hidden.remove(&p.id) {
                self.panes.insert(p.id, view);
            } else if let Some(view) = self.open_pane(p, window, cx) {
                self.panes.insert(p.id, view);
            }
        }
        // The focused pane's view was replaced: the new one takes the keys.
        if refocus && let Some(view) = self.focused.and_then(|f| self.panes.get(&f)) {
            let handle = view.focus(cx);
            window.focus(&handle, cx);
        }
        // Keep focus on a visible pane: tmux's active one if ours is gone.
        if self.focused.is_none_or(|f| !self.panes.contains_key(&f)) {
            let active = self.active_window().and_then(|w| w.active_pane);
            let target = active.filter(|a| self.panes.contains_key(a));
            self.set_focus(target.or_else(|| visible.first().map(|p| p.id)), window, cx);
        }
    }

    /// Keep a web pane's page title and scroll position in its
    /// placeholder.
    #[cfg(feature = "web")]
    pub(crate) fn keep_web_state(&self, state: api::WebState) {
        self.topology.web_state(state);
    }

    pub(crate) fn click_tab_for_smoke(
        &mut self,
        window_id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.click_tab(window_id, window, cx);
    }

    /// Show the tab at once, and ask tmux to select it.
    fn click_tab(&mut self, window_id: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.selecting = Some((window_id, std::time::Instant::now()));
        self.sync_panes(window, cx);
        self.topology
            .act(api::action::Kind::SelectWindow(api::SelectWindow {
                window: window_id,
            }));
        cx.notify();
    }

    fn sync_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .focused
            .and_then(|f| self.panes.get(&f))
            .map(|p| match p {
                PaneView::Terminal(s) => s.read(cx).wanted_title(),
                PaneView::App { view, instance } => view
                    .read(cx)
                    .shown
                    .as_ref()
                    .map_or_else(|| instance.clone(), |a| a.manifest.title.clone()),
                PaneView::Web { url, .. } => url.chars().take(80).collect(),
            })
            .unwrap_or_else(|| "Dreamer".into());
        let title = match self.via.host() {
            Some(host) => format!("{host}: {title}"),
            None => title,
        };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }
}

/// How many hidden panes keep their views (and their feeds): enough for a
/// few full tabs, a bounded cost however many panes exist.
const HIDDEN_VIEWS: usize = 48;

/// How long a clicked tab is shown before tmux confirms it.
const SELECT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Each window frame's start and the terminal paint time so far, while
/// the GUI benchmark records them (`crate::bench_gui`).
pub(crate) static FRAME_LOG: std::sync::Mutex<Option<Vec<(std::time::Instant, u64)>>> =
    std::sync::Mutex::new(None);

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Ok(mut log) = FRAME_LOG.lock()
            && let Some(log) = log.as_mut()
        {
            log.push((std::time::Instant::now(), terminal_render::paint_nanos()));
        }
        self.sync_title(window, cx);
        // Chromium's views sit above everything drawn here: a page shows
        // only while its pane is on screen, and its snapshot while an
        // overlay is open. The overlay waits (drawn, but not seen) until
        // no live page is left to cover it.
        let place = if self.palette.is_some() || self.menu.is_some() {
            crate::web::Place::Covered
        } else {
            crate::web::Place::Shown
        };
        let mut overlay_clear = true;
        for view in self.panes.values().filter_map(PaneView::web_pane) {
            overlay_clear &= view.update(cx, |w, cx| w.set_place(place, window, cx));
        }
        for view in self.hidden.values().filter_map(|(v, _)| v.web_pane()) {
            view.update(cx, |w, cx| {
                w.set_place(crate::web::Place::Hidden, window, cx)
            });
        }
        let t = crate::theme::for_window(window);
        let tf = term_font();
        let m = cell_metrics(window, &font(tf.family.clone()), px(tf.size), tf.spacing());
        let entity = cx.entity();

        // Tabs: the session's windows by index.
        let mut tabs: Vec<api::Window> = match (&self.current, self.session) {
            (Some(t), Some(s)) => t
                .windows
                .iter()
                .filter(|w| w.sessions.contains(&s))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        tabs.sort_by_key(|w| w.index);
        let active = self.active_window().map(|w| w.id);
        // Compact status: only when something is not normal.
        let status = {
            use replica::feed::FeedStatus;
            use replica::topology::TopologyStatus;
            match self.topology.status() {
                TopologyStatus::Reconnecting(why) => Some(format!("reconnecting: {why}")),
                TopologyStatus::CannotStart(why) => Some(why),
                TopologyStatus::Connecting => Some("connecting…".to_owned()),
                TopologyStatus::Following => {
                    let behind = self
                        .panes
                        .values()
                        .filter_map(PaneView::shell)
                        .filter(|s| {
                            s.read(cx)
                                .feed
                                .as_ref()
                                .is_some_and(|f| f.status() != FeedStatus::Following)
                        })
                        .count();
                    (behind > 0).then(|| match behind {
                        1 => "1 pane catching up".to_owned(),
                        n => format!("{n} panes catching up"),
                    })
                }
            }
        };
        // The tab strip, in the terminal's own background so it reads as
        // part of the same surface; on macOS, or with no decorations, it is
        // the title bar.
        let bare = crate::live::with(|l| l.decorations).unwrap_or_default()
            == workspace_config::Decorations::None;
        let strip = div()
            .id("title-bar")
            .h(px(TAB_STRIP))
            .flex_none()
            .flex()
            .flex_row()
            .items_stretch()
            .gap_1()
            .pl(px(if bare { 0.0 } else { TITLE_BUTTONS } + 8.0))
            .pr_2()
            .bg(c(t.background))
            .border_b_1()
            .border_color(c(t.divider))
            .text_size(px(12.0))
            .when(cfg!(target_os = "macos") || bare, |d| {
                d.window_control_area(gpui::WindowControlArea::Drag)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|ws, _: &MouseDownEvent, _, _| ws.title_drag = true),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|ws, _: &gpui::MouseUpEvent, _, _| ws.title_drag = false),
                    )
                    .on_mouse_move(cx.listener(|ws, _: &gpui::MouseMoveEvent, window, _| {
                        if std::mem::take(&mut ws.title_drag) {
                            window.start_window_move();
                        }
                    }))
                    .on_click(|ev, window, _| {
                        if ev.click_count() == 2 {
                            if cfg!(target_os = "macos") {
                                window.titlebar_double_click();
                            } else {
                                window.zoom_window();
                            }
                        }
                    })
            })
            .children(tabs.into_iter().map(|w| {
                let id = w.id;
                let is_active = Some(id) == active;
                // A plain label; the active one in full text color with
                // an accent line under it, like the focused pane's edge.
                div()
                    .id(("tab", id as usize))
                    .flex()
                    .items_center()
                    .px_2()
                    .border_b_2()
                    .border_color(if is_active {
                        c(t.accent)
                    } else {
                        gpui::transparent_black().into()
                    })
                    .text_color(if is_active { c(t.text) } else { c(t.muted) })
                    .hover(|s| s.text_color(c(t.text)))
                    .child(format!("{}:{}", w.index, w.name))
                    // A tab press is not a window drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |ws, _, window, cx| ws.click_tab(id, window, cx)))
            }))
            .child(div().flex_1())
            .children(self.contributed.outcome.clone().map(|o| {
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .text_color(c(t.faint))
                    .child(o)
            }))
            .children(self.contributed.status.values().map(|v| {
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .text_color(c(t.muted))
                    .child(v.clone())
            }))
            .children(status.map(|s| {
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .text_color(c(t.warning))
                    .child(s)
            }))
            // A configuration change that was not applied, and why: the
            // settings in force stay until the file reads again.
            .children(
                crate::live::with(|l| l.refused.clone())
                    .flatten()
                    .map(|why| {
                        let short = why
                            .rsplit_once("dreamer.yaml: ")
                            .map_or(why.as_str(), |(_, rest)| rest)
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        div()
                            .flex()
                            .items_center()
                            .px_2()
                            .text_color(c(t.warning))
                            .child(format!("dreamer.yaml not applied: {short}"))
                    }),
            );

        // The sidebar, on demand: sessions, the shown one highlighted.
        let sidebar = self.sidebar.then(|| {
            let sessions: Vec<api::Session> = self
                .current
                .as_ref()
                .map(|t| t.sessions.iter().filter(|s| !s.holding).cloned().collect())
                .unwrap_or_default();
            let shown = self.session;
            div()
                .w(px(180.0))
                .h_full()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .bg(c(t.surface))
                .text_size(px(12.0))
                .child(div().text_color(c(t.faint)).child("Sessions"))
                .children(sessions.into_iter().map(|s| {
                    let id = s.id;
                    let here = Some(id) == shown;
                    div()
                        .id(("session", id as usize))
                        .px_2()
                        .rounded_sm()
                        .text_color(if here { c(t.text) } else { c(t.muted) })
                        .when(here, |d| d.bg(c(t.selected)))
                        .child(s.name)
                        .on_click(
                            cx.listener(move |ws, _, window, cx| ws.switch_session(id, window, cx)),
                        )
                }))
                .children(self.hosts_section(t, cx))
                .children(self.apps_section(t, cx))
        });

        // Panes at their tmux cell rectangles, in canonical cells scaled to
        // this view; a zoomed pane fills the window's area.
        let w = self.active_window().cloned();
        let zoomed = w.as_ref().is_some_and(|w| w.zoomed);
        let unsupported = w.as_ref().is_some_and(|w| !w.layout_supported);
        let focused = self.focused;
        // Zooming means something only with more than one pane.
        let zoomable = zoomed || self.visible().len() > 1;
        // Panes in a tmux-drawn mode: marked, with the attach command.
        let mut mode_banners: HashMap<u32, gpui::Div> = self
            .visible()
            .iter()
            .filter_map(|p| Some((p.id, self.pane_mode(p.id)?)))
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|(id, mode)| {
                let attach = self.attach_for(w.as_ref().map(|w| w.id))?;
                Some((
                    id,
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .p_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .bg(c(t.warning_surface))
                        .text_size(px(12.0))
                        .text_color(c(t.warning_text))
                        .child(format!(
                            "tmux {mode} is open in this pane; Dreamer does not draw tmux's own screens. To use it:"
                        ))
                        .child(self.fallback_ui(t, id as usize + 1, attach, cx)),
                ))
            })
            .collect();
        let panes: Vec<_> = self
            .visible()
            .into_iter()
            .filter_map(|p| {
                let view = self.panes.get(&p.id)?.element();
                let (left, top, cols, rows) = match (&w, zoomed) {
                    (Some(w), true) => (0, 0, w.width, w.height),
                    _ => (p.left, p.top, p.width, p.height),
                };
                let id = p.id;
                let entity = entity.clone();
                let right = entity.clone();
                Some(
                    div()
                        .absolute()
                        .left(m.width * left as f32)
                        .top(m.height * top as f32)
                        .w(m.width * cols as f32)
                        .h(m.height * rows as f32)
                        // Which pane has focus matters only among several.
                        .when(Some(id) == focused && !zoomed && zoomable, |d| {
                            d.border_1().border_color(c(t.focus))
                        })
                        .group("pane")
                        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                            entity.update(cx, |ws, cx| {
                                if ws.focused != Some(id) {
                                    ws.click_pane(id, window, cx);
                                }
                            });
                        })
                        .on_mouse_down(
                            MouseButton::Right,
                            move |ev: &MouseDownEvent, window, cx| {
                                right.update(cx, |ws, cx| ws.right_click(id, ev, window, cx));
                            },
                        )
                        .child(view)
                        .children(mode_banners.remove(&id))
                        .when(Some(id) == focused && zoomable, |d| {
                            // Zoom control on the focused pane: shown only
                            // while the pointer is over the pane, on its
                            // own chip, so it never sits on terminal text.
                            d.child(
                                div()
                                    .id(("zoom", id as usize))
                                    .absolute()
                                    .bottom_1()
                                    .right_1()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(c(t.raised))
                                    .text_size(px(11.0))
                                    .text_color(c(t.muted))
                                    .opacity(0.0)
                                    .group_hover("pane", |s| s.opacity(1.0))
                                    .hover(|s| s.text_color(c(t.text)))
                                    .child(if zoomed { "unzoom" } else { "zoom" })
                                    .on_click(cx.listener(|ws, _, _, cx| ws.toggle_zoom(cx))),
                            )
                        }),
                )
            })
            .collect();
        // Divider hit zones in tmux's one-cell gaps, and a preview line
        // while one is dragged.
        let handles: Vec<_> = self
            .dividers()
            .into_iter()
            .enumerate()
            .map(|(i, d)| {
                let (left, top, w, h) = match d.axis {
                    Axis::Cols => (d.at, d.from, 1, d.to - d.from),
                    Axis::Rows => (d.from, d.at, d.to - d.from, 1),
                };
                div()
                    .id(("divider", i))
                    .absolute()
                    .left(m.width * left as f32)
                    .top(m.height * top as f32)
                    .w(m.width * w as f32)
                    .h(m.height * h as f32)
                    .flex()
                    .justify_center()
                    .items_center()
                    // tmux's one-cell gap, drawn as a hairline down its
                    // middle rather than a solid bar.
                    .child(match d.axis {
                        Axis::Cols => div().w(px(1.0)).h_full().bg(c(t.divider)),
                        Axis::Rows => div().h(px(1.0)).w_full().bg(c(t.divider)),
                    })
                    .when(d.axis == Axis::Cols, |x| x.cursor_col_resize())
                    .when(d.axis == Axis::Rows, |x| x.cursor_row_resize())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _: &MouseDownEvent, _, cx| ws.start_drag(d, cx)),
                    )
            })
            .collect();
        let preview = self.drag.map(|drag| {
            let d = drag.divider;
            let at = d.base + drag.size;
            let line = div().absolute().bg(c(t.accent));
            match d.axis {
                Axis::Cols => line
                    .left(m.width * at as f32)
                    .top(m.height * d.from as f32)
                    .w(px(2.0))
                    .h(m.height * (d.to - d.from) as f32),
                Axis::Rows => line
                    .left(m.width * d.from as f32)
                    .top(m.height * at as f32)
                    .w(m.width * (d.to - d.from) as f32)
                    .h(px(2.0)),
            }
        });
        let palette = self.palette.clone().map(|p| {
            use crate::palette::Stage;
            let choosing = matches!(p.stage, Stage::Choose | Stage::Theme(_));
            type Row = (
                String,
                bool,
                Option<&'static terminal_ghostty::core::schemes::Scheme>,
            );
            let (prompt, rows): (String, Vec<Row>) = match &p.stage {
                Stage::Choose => (
                    p.filter.clone(),
                    p.shown()
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (p.label_of(*c), i == p.selected, None))
                        .collect(),
                ),
                Stage::Theme(pick) => (
                    pick.filter.clone(),
                    pick.shown()
                        .into_iter()
                        .enumerate()
                        .map(|(i, s)| (s.title.to_owned(), i == pick.selected, Some(s)))
                        .collect(),
                ),
                Stage::Confirm(c) => (
                    format!(
                        "{} Press Enter to confirm, Backspace to go back.",
                        c.label()
                    ),
                    Vec::new(),
                ),
                Stage::Name(c, name) => (format!("{} {name}", c.label()), Vec::new()),
                Stage::Form(f) => (
                    match &f.error {
                        Some(e) => format!("{} {e}", f.command.label()),
                        None => format!(
                            "{} Enter: next field; Up/Down: move; Escape: cancel.",
                            f.command.label()
                        ),
                    },
                    f.fields
                        .iter()
                        .enumerate()
                        .map(|(i, (label, value, _))| {
                            (format!("{label}: {value}"), i == f.at, None)
                        })
                        .collect(),
                ),
            };
            div()
                .absolute()
                .top(px(TAB_STRIP + 8.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .track_focus(&self.palette_focus)
                        .on_key_down(cx.listener(Self::palette_key))
                        .w(px(PALETTE_WIDTH))
                        .p_2()
                        .rounded_md()
                        .shadow_lg()
                        .bg(c(t.raised))
                        .border_1()
                        .border_color(c(t.focus))
                        .text_size(px(13.0))
                        .text_color(c(t.text))
                        .child(if choosing {
                            // The query line: what was typed, or a hint.
                            div()
                                .px_2()
                                .pb_2()
                                .mb_1()
                                .border_b_1()
                                .border_color(c(t.divider))
                                .flex()
                                .gap_2()
                                .child(div().text_color(c(t.accent)).child("›"))
                                .child(if prompt.is_empty() {
                                    div().text_color(c(t.faint)).child(
                                        if matches!(p.stage, Stage::Theme(_)) {
                                            "Type a theme · Enter keeps it · Escape puts it back"
                                        } else {
                                            "Type a command"
                                        },
                                    )
                                } else {
                                    div().child(prompt)
                                })
                        } else {
                            div().px_2().pb_1().child(prompt)
                        })
                        .children({
                            // A window of rows that keeps the selected one in
                            // view, so a long list never runs off the window.
                            let total = rows.len();
                            let at = rows.iter().position(|(_, s, _)| *s).unwrap_or(0);
                            let start = (at + 1).saturating_sub(PALETTE_ROWS);
                            let below = total.saturating_sub(start + PALETTE_ROWS);
                            rows.into_iter()
                                .skip(start)
                                .take(PALETTE_ROWS)
                                .map(|(label, selected, scheme)| {
                                    div()
                                        .px_2()
                                        .py(px(2.0))
                                        .rounded_sm()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .when(selected, |d| d.bg(c(t.selected)))
                                        .when(label.starts_with("Kill"), |d| {
                                            d.text_color(c(t.danger))
                                        })
                                        .child(label)
                                        .children(scheme.map(swatch))
                                })
                                .chain((below > 0).then(|| {
                                    div()
                                        .px_2()
                                        .pt_1()
                                        .text_size(px(11.0))
                                        .text_color(c(t.faint))
                                        .child(format!("{below} more"))
                                }))
                                .collect::<Vec<_>>()
                        }),
                )
        });
        let menu = self.menu.map(|at| self.context_menu(at, t, window, cx));
        let notice = if unsupported {
            let attach = self.attach_for(w.as_ref().map(|w| w.id));
            Some(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_color(c(t.muted))
                    .child("This window uses a layout Dreamer cannot draw yet (floating panes). Open it in an ordinary tmux client:")
                    .children(attach.map(|a| self.fallback_ui(t, 0, a, cx))),
            )
        } else {
            None
        };
        let area = self.pane_area(
            entity.clone(),
            m,
            panes
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            handles
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            preview.map(IntoElement::into_any_element),
            notice.map(IntoElement::into_any_element),
            t,
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(t.background))
            // One type family for the whole window: the chrome uses the
            // terminal's font at smaller sizes.
            .font_family(term_font().family)
            .text_color(c(t.text))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::contributed_key))
            .on_action(cx.listener(|ws, _: &crate::keymap::ZoomPane, _, cx| ws.toggle_zoom(cx)))
            .on_action(
                cx.listener(|ws, _: &crate::keymap::CommandPalette, window, cx| {
                    ws.open_palette(window, cx)
                }),
            )
            .on_action(
                cx.listener(|ws, _: &crate::keymap::ToggleSidebar, _, cx| ws.toggle_sidebar(cx)),
            )
            .on_action(
                cx.listener(|ws, _: &crate::keymap::ToggleDrawer, window, cx| {
                    ws.toggle_drawer(false, window, cx)
                }),
            )
            .on_action(cx.listener(|ws, _: &crate::keymap::NewDrawer, window, cx| {
                ws.toggle_drawer(true, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &crate::keymap::NewTab, window, cx| {
                ws.run_command(crate::palette::Command::NewWindow, None, window, cx)
            }))
            .on_action(
                cx.listener(|ws, _: &crate::keymap::SplitRight, window, cx| {
                    ws.run_command(crate::palette::Command::SplitRight, None, window, cx)
                }),
            )
            .on_action(cx.listener(|ws, _: &crate::keymap::SplitDown, window, cx| {
                ws.run_command(crate::palette::Command::SplitDown, None, window, cx)
            }))
            .on_action(cx.listener(|ws, _: &crate::keymap::NextTab, window, cx| {
                ws.run_command(crate::palette::Command::NextWindow, None, window, cx)
            }))
            .on_action(
                cx.listener(|ws, _: &crate::keymap::PreviousTab, window, cx| {
                    ws.run_command(crate::palette::Command::PreviousWindow, None, window, cx)
                }),
            )
            // Any key closes the context menu; Escape goes no further.
            .capture_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, _, cx| {
                if ws.menu.take().is_some() {
                    if ev.keystroke.key == "escape" {
                        cx.stop_propagation();
                    }
                    cx.notify();
                }
            }))
            .child(strip)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .children(sidebar)
                    .child(
                        // Inset from the window's edges, in the terminal's
                        // background, so panes and chrome are one surface.
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .p(px(PANE_INSET))
                            .bg(c(t.background))
                            .child(area),
                    )
                    .children(self.panel.clone()),
            )
            // Last, so it floats over the panes rather than under them.
            .children(palette.map(|p| p.when(!overlay_clear, |d| d.opacity(0.0))))
            .children(menu.map(|m| m.when(!overlay_clear, |d| d.opacity(0.0))))
            .when(bare && !cfg!(target_os = "macos"), |d| {
                d.children(resize_edges())
            })
    }
}

/// Show or hide the system's title bar and window buttons. macOS hides the
/// buttons (the window keeps its resizable frame); Linux asks for
/// client-side decorations and draws none.
pub(crate) fn decorate(window: &mut Window, decorations: workspace_config::Decorations) {
    let bare = decorations == workspace_config::Decorations::None;
    #[cfg(target_os = "macos")]
    window.set_native_titlebar_visibility(!bare);
    #[cfg(not(target_os = "macos"))]
    window.request_decorations(if bare {
        gpui::WindowDecorations::Client
    } else {
        gpui::WindowDecorations::Server
    });
}

/// Thin bands along a frameless window's edges and corners that resize it.
fn resize_edges() -> impl Iterator<Item = gpui::AnyElement> {
    use gpui::ResizeEdge as E;
    let w = px(RESIZE_EDGE);
    [
        E::Top,
        E::TopRight,
        E::Right,
        E::BottomRight,
        E::Bottom,
        E::BottomLeft,
        E::Left,
        E::TopLeft,
    ]
    .into_iter()
    .enumerate()
    .map(move |(i, edge)| {
        let d = div().id(("resize-edge", i)).absolute();
        let d = match edge {
            E::Top => d.top_0().left(w).right(w).h(w).cursor_ns_resize(),
            E::Bottom => d.bottom_0().left(w).right(w).h(w).cursor_ns_resize(),
            E::Left => d.left_0().top(w).bottom(w).w(w).cursor_ew_resize(),
            E::Right => d.right_0().top(w).bottom(w).w(w).cursor_ew_resize(),
            E::TopLeft => d.top_0().left_0().size(w).cursor_nwse_resize(),
            E::BottomRight => d.bottom_0().right_0().size(w).cursor_nwse_resize(),
            E::TopRight => d.top_0().right_0().size(w).cursor_nesw_resize(),
            E::BottomLeft => d.bottom_0().left_0().size(w).cursor_nesw_resize(),
        };
        d.on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        })
        .into_any_element()
    })
}

/// The pane context menu's entries; `None` is a separator.
fn menu_items() -> Vec<Option<(&'static str, Box<dyn gpui::Action>)>> {
    use crate::keymap as k;
    vec![
        Some(("Copy", Box::new(k::Copy))),
        Some(("Paste", Box::new(k::Paste))),
        None,
        Some(("New Window", Box::new(k::NewWindow))),
        Some(("New Tab", Box::new(k::NewTab))),
        Some(("Split Right", Box::new(k::SplitRight))),
        Some(("Split Down", Box::new(k::SplitDown))),
        None,
        Some(("Command Palette", Box::new(k::CommandPalette))),
    ]
}

/// The context menu's width.
const MENU_WIDTH: f32 = 220.0;

impl Workspace {
    /// A right click in a pane: the context menu, unless the program there
    /// takes the mouse (Shift+right-click opens it even then). App panes
    /// draw their own.
    fn right_click(
        &mut self,
        pane: u32,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(shell) = self.panes.get(&pane).and_then(PaneView::shell) else {
            return;
        };
        if shell.read(cx).frame.view.mouse_tracking && !ev.modifiers.shift {
            return;
        }
        if self.focused != Some(pane) {
            self.click_pane(pane, window, cx);
        }
        self.menu = Some(ev.position);
        cx.notify();
    }

    /// The context menu at `at`, kept inside the window, over a backdrop
    /// that closes it on a click elsewhere.
    fn context_menu(
        &self,
        at: gpui::Point<gpui::Pixels>,
        t: crate::theme::Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let items = menu_items();
        let viewport = window.viewport_size();
        let height = items
            .iter()
            .map(|i| if i.is_some() { 24.0 } else { 9.0 })
            .sum::<f32>()
            + 10.0;
        let x = f32::from(at.x).min(f32::from(viewport.width) - MENU_WIDTH - 4.0);
        let y = f32::from(at.y).min(f32::from(viewport.height) - height - 4.0);
        let close =
            |ws: &mut Workspace, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
                ws.menu = None;
                cx.notify();
            };
        div()
            .id("menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(close))
            .on_mouse_down(MouseButton::Right, cx.listener(close))
            .child(
                div()
                    .absolute()
                    .left(px(x.max(4.0)))
                    .top(px(y.max(4.0)))
                    .w(px(MENU_WIDTH))
                    .p_1()
                    .rounded_md()
                    .shadow_lg()
                    .bg(c(t.raised))
                    .border_1()
                    .border_color(c(t.divider))
                    .text_size(px(12.0))
                    .text_color(c(t.text))
                    .children(items.into_iter().enumerate().map(|(i, item)| {
                        let Some((label, action)) = item else {
                            return div()
                                .my(px(4.0))
                                .h(px(1.0))
                                .bg(c(t.divider))
                                .into_any_element();
                        };
                        let keys = crate::keymap::shortcut_label(&*action);
                        div()
                            .id(("menu-item", i))
                            .h(px(24.0))
                            .px_2()
                            .rounded_sm()
                            .flex()
                            .items_center()
                            .justify_between()
                            .hover(|s| s.bg(c(t.selected)))
                            .child(label)
                            .children(keys.map(|k| div().text_color(c(t.faint)).child(k)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |ws, _: &MouseDownEvent, window, cx| {
                                    ws.menu = None;
                                    window.dispatch_action(action.boxed_clone(), cx);
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .into_any_element()
                    })),
            )
    }
}

impl Workspace {
    /// The pane area: panes, divider handles, the drag preview, and the
    /// canvas that measures it.
    #[allow(clippy::too_many_arguments)]
    fn pane_area(
        &self,
        entity: Entity<Workspace>,
        m: terminal_render::CellMetrics,
        panes: Vec<gpui::AnyElement>,
        handles: Vec<gpui::AnyElement>,
        preview: Option<gpui::AnyElement>,
        notice: Option<gpui::AnyElement>,
        t: crate::theme::Theme,
    ) -> gpui::Div {
        div()
            .relative()
            .flex_1()
            .overflow_hidden()
            // tmux leaves one cell between panes; the divider handles draw
            // a hairline in it.
            .bg(c(t.background))
            .children(panes)
            .children(handles)
            .children(preview)
            .children(notice)
            // Measures the pane area for window sizing.
            .child({
                let entity = entity.clone();
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        entity.update(cx, |ws, _| {
                            ws.origin = Some(bounds.origin);
                            ws.cell = Some((m.width, m.height));
                        });
                        // A drag follows the pointer anywhere until
                        // the button comes up.
                        let e = entity.clone();
                        window.on_mouse_event(move |ev: &gpui::MouseMoveEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                e.update(cx, |ws, cx| {
                                    if ws.drag.is_some() {
                                        ws.drag_to(ev.position, cx);
                                    }
                                });
                            }
                        });
                        let e = entity.clone();
                        window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                e.update(cx, |ws, cx| {
                                    if ws.drag.is_some() {
                                        ws.end_drag(cx);
                                    }
                                });
                            }
                        });
                        let cols = (f32::from(bounds.size.width) / f32::from(m.width)).floor();
                        let rows = (f32::from(bounds.size.height) / f32::from(m.height)).floor();
                        if cols >= 2.0 && rows >= 2.0 {
                            entity
                                .update(cx, |ws, cx| ws.area_painted(cols as u32, rows as u32, cx));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full()
            })
    }
}

/// Keep `workspace` current with the daemon's topology; never polls.
pub(crate) fn follow_topology(
    any: gpui::AnyWindowHandle,
    workspace: Entity<Workspace>,
    topology: Arc<TopologyFeed>,
    cx: &mut gpui::App,
) {
    let weak = workspace.downgrade();
    cx.spawn(async move |cx| {
        while topology.wake.recv().await.is_ok() {
            let t = topology.take().and_then(|u| u.topology);
            let alive = cx.update_window(any, |_, window, cx| {
                weak.update(cx, |ws, cx| match t {
                    Some(t) => ws.update_topology(t, window, cx),
                    // A status change: redraw the status.
                    None => cx.notify(),
                })
                .is_ok()
            });
            if !matches!(alive, Ok(true)) {
                return;
            }
        }
    })
    .detach();
}

/// Which smoke a workspace window runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SmokeRun {
    /// `--smoke-workspace`.
    Workspace,
    /// `--smoke-workspace-restore`: the second launch.
    Restore,
    /// `--smoke-clipboard`: copy and paste in every kind of pane.
    Clipboard,
    /// `--smoke-web DIR`: a page in a web pane, captured before and after
    /// a scroll into DIR.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    Web(PathBuf),
}

/// What a window shows first.
#[derive(Debug, Clone)]
pub(crate) enum Start {
    /// A saved window's session (by id on the same tmux server, else by
    /// name), or with none the first session.
    Saved(Option<DesktopState>),
    /// A new session of its own, as a terminal's new window is a new shell.
    NewSession,
}

/// A smoke run: windows save their arrangement only in a private
/// directory the run was given (`DREAMER_DESKTOP_STATE_DIR`), never the
/// user's, including windows a smoke opens itself.
pub(crate) static SMOKE_RUN: AtomicBool = AtomicBool::new(false);

/// The one store of every window's arrangement for this process, if this
/// run saves one.
fn store() -> Option<Arc<DesktopStore>> {
    static STORE: std::sync::OnceLock<Option<Arc<DesktopStore>>> = std::sync::OnceLock::new();
    if SMOKE_RUN.load(Ordering::Relaxed) && std::env::var_os("DREAMER_DESKTOP_STATE_DIR").is_none()
    {
        return None;
    }
    STORE
        .get_or_init(|| {
            workspace_config::desktop_state::desktop_state_path()
                .map(|p| Arc::new(DesktopStore::new(p)))
        })
        .clone()
}

/// Windows' keys in the store, in opening order.
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// What a new window asks the daemon for: a new session running the
/// default shell in the home directory (tmux names it).
pub(crate) fn new_session_action(home: Option<PathBuf>) -> api::action::Kind {
    api::action::Kind::NewSession(api::NewSession {
        cwd: home
            .filter(|h| h.is_absolute())
            .map(|h| h.display().to_string()),
        ..api::NewSession::default()
    })
}

/// The session pane `pane` belongs to in `t`.
fn session_of_pane(t: &api::Topology, pane: u32) -> Option<u32> {
    let window = t.panes.iter().find(|p| p.id == pane)?.window;
    t.windows
        .iter()
        .find(|w| w.id == window)?
        .sessions
        .first()
        .copied()
}

/// Window `key` was closed: drop its entry from `store` and write the
/// rest, off the UI thread.
fn forget(store: Option<Arc<DesktopStore>>, key: u64, cx: &mut gpui::App) {
    if let Some(store) = store {
        store.forget(key);
        cx.background_spawn(async move {
            let _ = store.save();
        })
        .detach();
    }
}

/// Close `window` (Close Window): it is not opened again at the next
/// launch. Its tmux session keeps running.
pub(crate) fn close_window(window: &mut Window, cx: &mut gpui::App) {
    if let Some(Some(ws)) = window.root::<Workspace>() {
        let (store, key) = {
            let w = ws.read(cx);
            (w.store.clone(), w.state_key)
        };
        forget(store, key, cx);
    }
    window.remove_window();
}

/// Open a workspace window following the daemon `launch` names. With
/// `smoke` (the daemon's arguments, to find the isolated tmux), run that
/// smoke.
pub(crate) fn open(
    cx: &mut gpui::App,
    launch: client::Launch,
    smoke: Option<(Vec<String>, SmokeRun)>,
    open_at_start: Vec<String>,
    start: Start,
) -> Result<(), String> {
    open_via(cx, launch, replica::Via::Local, smoke, open_at_start, start).map(drop)
}

/// At launch: every window saved last time, each on its own session, or
/// one window when none was saved. The first runs `smoke`, if any.
pub(crate) fn open_saved(
    cx: &mut gpui::App,
    launch: client::Launch,
    smoke: Option<(Vec<String>, SmokeRun)>,
    open_at_start: Vec<String>,
) -> Result<(), String> {
    let saved = store()
        .and_then(|s| workspace_config::desktop_state::load_desktop(s.path()))
        .unwrap_or_default();
    let mut windows = saved.into_iter().map(Some).collect::<Vec<_>>();
    if windows.is_empty() {
        windows.push(None);
    }
    let mut first = Some((smoke, open_at_start));
    for w in windows {
        let (smoke, open_at_start) = first.take().unwrap_or_default();
        open(cx, launch.clone(), smoke, open_at_start, Start::Saved(w))?;
    }
    Ok(())
}

/// The local topology feed, started when the process starts so the
/// daemon connection and the first topology overlap GPUI's own startup.
/// The first local window takes it; later windows start their own.
pub(crate) static EARLY_TOPOLOGY: std::sync::Mutex<Option<Arc<TopologyFeed>>> =
    std::sync::Mutex::new(None);

/// Run the remote smoke in a window opened with [`open_via`].
pub(crate) fn smoke_remote(
    cx: &mut gpui::App,
    window: (gpui::AnyWindowHandle, gpui::WeakEntity<Workspace>),
) {
    let (any, ws) = window;
    cx.spawn(async move |cx| smoke::remote(any, ws, cx).await)
        .detach();
}

/// Like [`open`], showing the tmux reached `via` (a host, through the
/// local daemon's coordinator).
pub(crate) fn open_via(
    cx: &mut gpui::App,
    launch: client::Launch,
    via: replica::Via,
    smoke: Option<(Vec<String>, SmokeRun)>,
    open_at_start: Vec<String>,
    start: Start,
) -> Result<(gpui::AnyWindowHandle, gpui::WeakEntity<Workspace>), String> {
    use gpui::{Bounds, WindowBounds, WindowOptions, size};
    let socket = launch.socket.clone();
    let daemon_args = launch.args.clone();
    let bench_daemon = daemon_args.clone();
    let socket_for_buffers = launch.socket.clone();
    let early = (via == replica::Via::Local)
        .then(|| EARLY_TOPOLOGY.lock().ok().and_then(|mut t| t.take()))
        .flatten();
    let topology = match early {
        Some(t) => t,
        None => Arc::new(
            TopologyFeed::start_via(socket.clone(), via.clone(), Some(launch))
                .map_err(|e| format!("cannot follow the tmux topology: {e}"))?,
        ),
    };
    // The saved arrangement. A smoke run uses one only in a private
    // directory it was given, never the user's. A host's window does not
    // save one: the arrangement is the local tmux's.
    let store = if via == replica::Via::Local {
        store()
    } else {
        None
    };
    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    let (saved, new_session) = match start {
        Start::Saved(s) => (s, false),
        Start::NewSession => (None, true),
    };
    // A new window sits a little below and right of the active one, as
    // new windows cascade in a terminal.
    let cascade = new_session
        .then(|| cx.active_window())
        .flatten()
        .and_then(|w| w.update(cx, |_, window, _| window.bounds()).ok())
        .map(|b| {
            let step = px(28.0);
            Bounds::new(gpui::point(b.origin.x + step, b.origin.y + step), b.size)
        });
    let bounds = match saved.as_ref().and_then(|s| s.bounds) {
        Some([x, y, w, h]) if w >= 200.0 && h >= 120.0 => {
            Bounds::new(gpui::point(px(x), px(y)), size(px(w), px(h)))
        }
        _ => cascade.unwrap_or_else(|| Bounds::centered(None, size(px(1000.), px(560.)), cx)),
    };
    let asked = new_session.then(|| {
        topology.act_then(new_session_action(
            std::env::var_os("HOME").map(PathBuf::from),
        ))
    });
    let closing = store.clone();
    let t = topology.clone();
    let decorations = crate::live::with(|l| l.decorations).unwrap_or_default();
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some(crate::APP_ID.into()),
                // macOS: no separate system title bar; the tab strip, in
                // the theme's colors, is the title bar.
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Dreamer".into()),
                    appears_transparent: cfg!(target_os = "macos"),
                    traffic_light_position: cfg!(target_os = "macos")
                        .then(|| gpui::point(px(12.0), px((TAB_STRIP - 14.0) / 2.0))),
                }),
                window_decorations: (decorations == workspace_config::Decorations::None)
                    .then_some(gpui::WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| {
                // Linux took its decorations from the options above.
                if cfg!(target_os = "macos") {
                    decorate(window, decorations);
                }
                let ws = cx.new(|cx| {
                    let mut ws = Workspace::new(cx, t, socket, daemon_args);
                    ws.via = via.clone();
                    ws.with_state(store, key, saved);
                    if new_session {
                        ws.opening = Opening::Asked;
                    }
                    ws
                });
                // Closed from its own button: not opened again at the
                // next launch.
                window.on_window_should_close(cx, move |_, cx| {
                    forget(closing.clone(), key, cx);
                    true
                });
                let focus = ws.read(cx).focus.clone();
                window.focus(&focus, cx);
                // Remember where the window is.
                ws.update(cx, |ws, cx| {
                    ws._bounds = Some(cx.observe_window_bounds(window, |ws, window, cx| {
                        let b = window.bounds();
                        ws.window_moved(
                            [
                                f32::from(b.origin.x),
                                f32::from(b.origin.y),
                                f32::from(b.size.width),
                                f32::from(b.size.height),
                            ],
                            cx,
                        );
                    }));
                    ws._activation = Some(cx.observe_window_activation(window, |_, window, cx| {
                        if window.is_window_active() {
                            crate::prefix::refresh(cx);
                        }
                    }));
                });
                ws
            },
        )
        .map_err(|e| format!("could not open a window: {e:#}"))?;
    let any: gpui::AnyWindowHandle = window.into();
    let ws = window
        .update(cx, |_, _, cx| cx.entity())
        .map_err(|e| format!("{e:#}"))?;
    follow_topology(any, ws.clone(), topology, cx);
    if let Some(asked) = asked {
        // The new session's first pane, or (if the daemon could not make
        // one) the usual choice.
        let weak = ws.downgrade();
        cx.spawn(async move |cx| {
            let pane = asked.recv().await.ok().flatten().and_then(|r| {
                (r.state == api::ActionState::Completed as i32)
                    .then_some(r.new_pane)
                    .flatten()
            });
            if pane.is_none() {
                eprintln!("dreamer: the new window's session could not be made; showing another");
            }
            let _ = cx.update_window(any, |_, window, cx| {
                weak.update(cx, |ws, cx| {
                    ws.opening = match pane {
                        Some(p) => Opening::Pane(p, std::time::Instant::now()),
                        None => Opening::Ready,
                    };
                    if let Some(t) = ws.current.clone() {
                        ws.update_topology(t, window, cx);
                    }
                })
            });
        })
        .detach();
    }
    if !open_at_start.is_empty() {
        let _ = window.update(cx, |ws, window, cx| {
            if open_at_start.iter().any(|o| o == "sidebar") && !ws.sidebar {
                ws.toggle_sidebar(cx);
            }
            if open_at_start.iter().any(|o| o == "palette") {
                ws.open_palette(window, cx);
            }
        });
    }
    // tmux's paste buffers: one follower per process, for the local
    // daemon. A copy in tmux reaches the clipboard when the settings say
    // so, under the same rule as a program's clipboard write.
    if via == replica::Via::Local
        && crate::TMUX_BUFFERS.get().is_none()
        && let Ok(b) = replica::clipboard::TmuxBuffers::start(socket_for_buffers.clone())
    {
        let copied = b.copied.clone();
        let _ = crate::TMUX_BUFFERS.set(b);
        crate::prefix::refresh(cx);
        cx.spawn(async move |cx| {
            while let Ok(text) = copied.recv().await {
                let _ = cx.update_window(any, |_, window, cx| {
                    use workspace_config::ClipboardWrite;
                    let (from, write) =
                        crate::live::with(|l| (l.clipboard_tmux.from_tmux(), l.clipboard))
                            .unwrap_or((false, ClipboardWrite::Deny));
                    let allowed = from
                        && match write {
                            ClipboardWrite::Deny => false,
                            ClipboardWrite::Allow => true,
                            ClipboardWrite::Focused => {
                                window.is_window_active() || crate::clip::in_memory()
                            }
                        };
                    if allowed {
                        crate::clip::write(cx, text);
                    }
                });
            }
        })
        .detach();
    }
    // Images of panes for `dream app preview --png`: one server per
    // process, for the local daemon.
    if via == replica::Via::Local {
        crate::capture::serve(socket_for_buffers.clone(), cx);
    }
    // The GUI benchmark (`cargo xtask bench gui`): measures this window.
    if via == replica::Via::Local
        && let Some(out) = std::env::var_os("DREAMER_BENCH_GUI")
    {
        let weak = ws.downgrade();
        let daemon = bench_daemon;
        cx.spawn(async move |cx| crate::bench_gui::run(any, weak, daemon, out.into(), cx).await)
            .detach();
    }
    if let Some((daemon, run)) = smoke {
        let weak = ws.downgrade();
        match run {
            SmokeRun::Restore => cx
                .spawn(async move |cx| smoke::restore(any, weak, cx).await)
                .detach(),
            SmokeRun::Workspace => cx
                .spawn(async move |cx| smoke::run(any, weak, daemon, cx).await)
                .detach(),
            SmokeRun::Clipboard => cx
                .spawn(async move |cx| smoke::clipboard::run(any, weak, daemon, cx).await)
                .detach(),
            #[cfg(feature = "web")]
            SmokeRun::Web(out) => cx
                .spawn(async move |cx| smoke::web::run(any, weak, out, cx).await)
                .detach(),
            #[cfg(not(feature = "web"))]
            SmokeRun::Web(_) => {
                let _ = daemon;
                eprintln!("smoke: this build has no web engine (the `web` feature)");
                std::process::exit(6);
            }
        }
    }
    Ok((any, ws.downgrade()))
}

mod smoke {
    //! `dreamer --smoke-workspace`: against an isolated tmux whose session
    //! `main` has two windows, the first split into three panes, each pane
    //! having printed `READY-%<pane id>`.

    pub(super) mod clipboard;
    #[cfg(feature = "web")]
    pub(super) mod web;

    use std::time::{Duration, Instant};

    use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, WeakEntity};

    use super::Workspace;

    fn fail(why: &str) -> ! {
        eprintln!("smoke: {why}");
        std::process::exit(3)
    }

    /// Read from the workspace (and its panes) on the UI thread.
    fn read<T>(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
        f: impl FnOnce(&Workspace, &gpui::App) -> T,
    ) -> Option<T> {
        cx.update_window(any, |_, _, cx| ws.read_with(cx, |w, cx| f(w, cx)).ok())
            .ok()
            .flatten()
    }

    /// Poll (smoke only) until `check` holds or 10 s pass.
    async fn wait_for(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
        what: &str,
        check: impl Fn(&Workspace, &gpui::App) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while read(any, ws, cx, &check) != Some(true) {
            if Instant::now() > deadline {
                crate::stalled(any, cx, &format!("never saw: {what}")).await;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    /// Every visible pane shows its own `READY-%<id>` line.
    fn panes_ready(ws: &Workspace, cx: &gpui::App, n: usize) -> bool {
        let ids = ws.pane_ids();
        ids.len() == n
            && ids.iter().all(|id| {
                ws.pane_shell(*id).is_some_and(|s| {
                    s.read(cx)
                        .frame
                        .text_dump()
                        .contains(&format!("READY-%{id}"))
                })
            })
    }

    fn tmux(daemon: &[String], args: &[&str]) {
        let opt = |name: &str| {
            daemon
                .iter()
                .position(|a| a == name)
                .and_then(|i| daemon.get(i + 1))
                .cloned()
        };
        let program = opt("--tmux").unwrap_or_else(|| "tmux".into());
        let Some(socket) = opt("--tmux-socket") else {
            fail("the workspace smoke needs an isolated tmux (-L)")
        };
        let ok = std::process::Command::new(program)
            .args(["-L", &socket])
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            fail(&format!("tmux {args:?} failed"));
        }
    }

    fn tmux_output(daemon: &[String], args: &[&str]) -> String {
        let opt = |name: &str| {
            daemon
                .iter()
                .position(|a| a == name)
                .and_then(|i| daemon.get(i + 1))
                .cloned()
        };
        let program = opt("--tmux").unwrap_or_else(|| "tmux".into());
        let socket = opt("--tmux-socket").unwrap_or_default();
        std::process::Command::new(program)
            .args(["-L", &socket])
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Text into the first visible pane, through its feed (and so through
    /// the coordinator for a host).
    fn type_into_first(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
        line: &str,
    ) {
        let text = format!("{line}\r");
        let _ = cx.update_window(any, move |_, _, cx| {
            let _ = ws.read_with(cx, |w, cx| {
                if let Some(s) = w.pane_ids().first().and_then(|id| w.pane_shell(*id)) {
                    s.read(cx)
                        .send_all([api::terminal_input::Event::Text(api::TextInput { text })]);
                }
            });
        });
    }

    fn first_pane_has_line(w: &Workspace, cx: &gpui::App, line: &str) -> bool {
        w.pane_ids()
            .first()
            .and_then(|id| w.pane_shell(*id))
            .is_some_and(|s| {
                s.read(cx)
                    .frame
                    .text_dump()
                    .lines()
                    .any(|l| l.trim_end() == line)
            })
    }

    fn panes_following(w: &Workspace, cx: &gpui::App) -> bool {
        let ids = w.pane_ids();
        !ids.is_empty()
            && ids.iter().all(|id| {
                w.pane_shell(*id).is_some_and(|s| {
                    s.read(cx)
                        .feed
                        .as_ref()
                        .is_some_and(|f| matches!(f.status(), replica::feed::FeedStatus::Following))
                })
            })
    }

    /// `dreamer --smoke-remote --host NAME`: a host's session through the
    /// local daemon's coordinator. Its pane shows what the host's tmux has
    /// (`READY-REMOTE`); typed text reaches it; disconnecting the host marks
    /// the window stale and keeps the pane's content; reconnecting resumes.
    pub(super) async fn remote(any: AnyWindowHandle, ws: WeakEntity<Workspace>, cx: &mut AsyncApp) {
        let host = read(any, &ws, cx, |w, _| w.via.host().map(str::to_owned))
            .flatten()
            .unwrap_or_else(|| fail("the remote smoke needs --host"));
        let deadline = Duration::from_secs(20);
        wait_long(any, &ws, cx, "the host's pane", deadline, |w, cx| {
            panes_following(w, cx) && first_pane_has_line(w, cx, "READY-REMOTE")
        })
        .await;
        println!("smoke: host `{host}`: its session's pane is shown through the coordinator");
        type_into_first(any, &ws, cx, "echo REMOTE-$((6*7))");
        wait_long(any, &ws, cx, "REMOTE-42", deadline, |w, cx| {
            first_pane_has_line(w, cx, "REMOTE-42")
        })
        .await;
        println!("smoke: text typed in the window ran on the host: REMOTE-42");

        let socket = read(any, &ws, cx, |w, _| w.socket.clone())
            .unwrap_or_else(|| fail("the window went away"));
        let hosts = replica::hosts::HostsFeed::start(socket)
            .unwrap_or_else(|e| fail(&format!("hosts feed: {e}")));
        let stale = |w: &Workspace| {
            matches!(
                w.topology.status(),
                replica::topology::TopologyStatus::Reconnecting(_)
            )
        };
        hosts.disconnect(&host);
        wait_long(any, &ws, cx, "the window marked stale", deadline, |w, _| {
            stale(w)
        })
        .await;
        if read(any, &ws, cx, |w, cx| {
            first_pane_has_line(w, cx, "REMOTE-42")
        }) != Some(true)
        {
            fail("the pane lost its content when the host went away");
        }
        println!(
            "smoke: disconnecting the host marked the window stale and the pane kept its content"
        );
        hosts.reconnect(&host);
        wait_long(
            any,
            &ws,
            cx,
            "the window current again",
            deadline,
            |w, cx| !stale(w) && panes_following(w, cx),
        )
        .await;
        type_into_first(any, &ws, cx, "echo BACK-$((2+3))");
        wait_long(any, &ws, cx, "BACK-5", deadline, |w, cx| {
            first_pane_has_line(w, cx, "BACK-5")
        })
        .await;
        println!(
            "smoke: reconnecting resumed the window, and typing reached the host again: BACK-5"
        );
        profiles(any, &ws, cx).await;
        themes(any, &ws, cx).await;
        std::process::exit(0);
    }

    /// Host profiles edited through the palette form: add one, change its
    /// tags, remove it. Each change lands in hosts.yaml (the smoke's private
    /// configuration directory), and the other profile is kept intact.
    async fn profiles(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
        let Some(path) = workspace_config::hosts::hosts_path() else {
            fail("no configuration directory");
        };
        if std::env::var_os("DREAMER_CONFIG_DIR").is_none() {
            fail("the profile smoke needs a private DREAMER_CONFIG_DIR");
        }
        let original = workspace_config::hosts::load_hosts().unwrap_or_default();
        let file_has = |want: &dyn Fn(&[workspace_config::hosts::HostProfile]) -> bool| {
            workspace_config::hosts::load_hosts().is_ok_and(|p| want(&p))
        };
        async fn until(cx: &mut AsyncApp, what: &str, ok: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ok() {
                if Instant::now() > deadline {
                    fail(&format!("never saw: {what}"));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }
        palette(any, ws, cx, "add a host profile");
        palette_more(any, ws, cx, "smoke-added");
        palette_more(any, ws, cx, "me@smoke-dest");
        for _ in 0..3 {
            palette_more(any, ws, cx, "");
        }
        until(cx, "the added profile in hosts.yaml", || {
            file_has(&|p| {
                p.iter()
                    .any(|h| h.name == "smoke-added" && h.destination == "me@smoke-dest")
            })
        })
        .await;
        // Edit: the form opens filled in; go to Tags and submit.
        palette(any, ws, cx, "edit a host profile");
        palette_more(any, ws, cx, "smoke-added");
        wait_for(any, ws, cx, "the edit form", |w, _| {
            matches!(
                w.palette.as_ref().map(|p| &p.stage),
                Some(crate::palette::Stage::Form(f)) if f.fields[1].1 == "me@smoke-dest"
            )
        })
        .await;
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| {
                    if let Some(p) = w.palette.as_mut() {
                        p.move_selection(false);
                        "lab".chars().for_each(|c| p.type_char(c));
                    }
                    w.palette_enter(window, cx);
                });
            }
        });
        until(cx, "the edited tags", || {
            file_has(&|p| {
                p.iter()
                    .any(|h| h.name == "smoke-added" && h.tags == ["lab"])
            })
        })
        .await;
        palette(any, ws, cx, "remove a host profile");
        palette_more(any, ws, cx, "smoke-added");
        until(cx, "the profile removed", || {
            file_has(&|p| !p.iter().any(|h| h.name == "smoke-added"))
        })
        .await;
        let after = workspace_config::hosts::load_hosts().unwrap_or_default();
        if after != original {
            fail(&format!(
                "other profiles changed: {original:?} -> {after:?}"
            ));
        }
        println!(
            "smoke: a host profile was added, edited, and removed from the palette ({})",
            path.display()
        );
    }

    /// Choosing a theme from the palette: the highlighted theme is written
    /// to dreamer.yaml for the terminals and drawn in the chrome; Escape
    /// puts the file back byte for byte; Enter keeps the theme, which is
    /// then in force.
    async fn themes(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
        if std::env::var_os("DREAMER_CONFIG_DIR").is_none() {
            fail("the theme smoke needs a private DREAMER_CONFIG_DIR");
        }
        let Some(path) = workspace_config::settings_path() else {
            fail("no configuration directory");
        };
        let original = std::fs::read_to_string(&path).ok();
        let file = || std::fs::read_to_string(&path).ok();
        async fn until(cx: &mut AsyncApp, what: &str, ok: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ok() {
                if Instant::now() > deadline {
                    fail(&format!("never saw: {what}"));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }
        // Type into the theme list as keys would, previewing as they do.
        let choose = |cx: &mut AsyncApp, typed: &str| {
            let typed = typed.to_owned();
            let _ = cx.update_window(any, |_, window, cx| {
                if let Some(ws) = ws.upgrade() {
                    ws.update(cx, |w, cx| {
                        w.open_palette(window, cx);
                        if let Some(p) = w.palette.as_mut() {
                            "theme".chars().for_each(|c| p.type_char(c));
                        }
                        w.palette_enter(window, cx);
                        if let Some(p) = w.palette.as_mut() {
                            typed.chars().for_each(|c| p.type_char(c));
                        }
                        w.preview_theme(cx);
                    });
                }
            });
        };
        let chrome = |cx: &mut AsyncApp| {
            cx.update(|_| crate::live::with(|l| l.preview.map(|t| t.background)).flatten())
        };
        let nord = crate::theme::for_scheme(
            terminal_ghostty::core::schemes::find("nord").unwrap_or_else(|| fail("no nord")),
        );
        choose(cx, "nord");
        if chrome(cx) != Some(nord.background) {
            fail("the chrome did not preview the highlighted theme");
        }
        until(cx, "the previewed theme in dreamer.yaml", || {
            file().is_some_and(|t| t.contains("theme: \"nord\""))
        })
        .await;
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.close_palette(window, cx));
            }
        });
        until(cx, "dreamer.yaml put back as it was", || file() == original).await;
        if chrome(cx).is_some() {
            fail("the chrome kept the preview after Escape");
        }
        choose(cx, "gruvbox dark");
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.palette_enter(window, cx));
            }
        });
        let kept = || crate::live::with(|l| l.scheme.name).unwrap_or_default();
        until(cx, "the chosen theme in force and in dreamer.yaml", || {
            kept().starts_with("gruvbox-dark")
                && file().is_some_and(|t| t.contains(&format!("theme: \"{}\"", kept())))
        })
        .await;
        let name = kept();
        let _ = workspace_config::edit::restore(&path, original.as_deref());
        println!(
            "smoke: a theme was previewed, put back, and chosen from the palette ({name}, {})",
            path.display()
        );
    }

    /// Like `wait_for`, with its own limit (a host connects over SSH).
    async fn wait_long(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
        what: &str,
        limit: Duration,
        check: impl Fn(&Workspace, &gpui::App) -> bool,
    ) {
        let deadline = Instant::now() + limit;
        while read(any, ws, cx, &check) != Some(true) {
            if Instant::now() > deadline {
                crate::stalled(any, cx, &format!("never saw: {what}")).await;
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    }

    pub(super) async fn run(
        any: AnyWindowHandle,
        ws: WeakEntity<Workspace>,
        daemon: Vec<String>,
        cx: &mut AsyncApp,
    ) {
        wait_for(any, &ws, cx, "two tabs and three ready panes", |w, cx| {
            w.tab_count() == 2 && panes_ready(w, cx, 3)
        })
        .await;
        let ids = read(any, &ws, cx, |w, _| w.pane_ids()).unwrap_or_default();
        println!("smoke: workspace shows 2 tabs and panes {ids:?}, each following its own feed");

        // Another tmux client splits the window: the view follows.
        tmux(&daemon, &["split-window", "-t", "main:0", "/bin/sh"]);
        wait_for(any, &ws, cx, "the pane another client split off", |w, _| {
            w.pane_ids().len() == 4
        })
        .await;
        println!("smoke: a split made by another tmux client appeared");

        // Another client opens tmux copy mode in a pane: marked, with an
        // attach command whose target resolves to exactly this window.
        tmux(&daemon, &["copy-mode", "-t", "%0"]);
        wait_for(any, &ws, cx, "pane %0 marked as in copy-mode", |w, _| {
            w.pane_mode(0).as_deref() == Some("copy-mode")
        })
        .await;
        let (attach, window, session) = read(any, &ws, cx, |w, _| {
            let window = w.active_window()?.id;
            Some((w.attach_for(Some(window))?, window, w.session?))
        })
        .flatten()
        .unwrap_or_else(|| fail("no attach command"));
        let target = attach.argv.last().cloned().unwrap_or_default();
        let resolved = tmux_output(
            &daemon,
            &[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{session_id}:#{window_id}",
            ],
        );
        if resolved.trim() != format!("${session}:@{window}") {
            fail(&format!(
                "the attach target {target:?} resolves to {resolved:?}"
            ));
        }
        tmux(&daemon, &["send-keys", "-t", "%0", "-X", "cancel"]);
        wait_for(any, &ws, cx, "pane %0 out of copy-mode", |w, _| {
            w.pane_mode(0).is_none()
        })
        .await;
        println!(
            "smoke: tmux copy-mode in a pane was marked with `{}`, whose target resolves to this window",
            attach.display
        );

        // Zoom the focused pane and back, through the typed zoom action.
        let toggle = |cx: &mut AsyncApp| {
            let _ = cx.update_window(any, |_, _, cx| {
                if let Some(ws) = ws.upgrade() {
                    ws.update(cx, |w, cx| w.toggle_zoom(cx));
                }
            });
        };
        toggle(cx);
        wait_for(any, &ws, cx, "the zoomed pane alone", |w, _| {
            w.zoomed() && w.pane_ids().len() == 1
        })
        .await;
        toggle(cx);
        wait_for(any, &ws, cx, "all panes after unzooming", |w, _| {
            !w.zoomed() && w.pane_ids().len() == 4
        })
        .await;
        println!("smoke: zoom showed the focused pane alone, and unzoom brought all four back");
        palette_commands(any, &ws, cx).await;
        contributions(any, &ws, &daemon, cx).await;
        find_in_pane(any, &ws, cx).await;

        let painted = click_and_type(any, &ws, cx).await;
        if painted {
            decorations(any, &ws, &daemon, cx).await;
            panel(any, &ws, &daemon, cx).await;
        }

        // Selecting a tab switches the tmux window, seen in the topology.
        let second = read(any, &ws, cx, |w, _| {
            w.topology()
                .and_then(|t| t.windows.iter().find(|x| x.index == 1).map(|x| x.id))
        })
        .flatten()
        .unwrap_or_else(|| fail("no second window"));
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.click_tab_for_smoke(second, window, cx));
            }
        });
        wait_for(
            any,
            &ws,
            cx,
            "the second window's pane after a tab click",
            |w, cx| panes_ready(w, cx, 1),
        )
        .await;
        println!("smoke: a tab click switched the tmux window, and only its pane is shown");

        // The sidebar from the palette, and another session in this window;
        // the arrangement is saved.
        palette(any, &ws, cx, "toggle sidebar");
        wait_for(any, &ws, cx, "the sidebar", |w, _| w.sidebar).await;
        let other = read(any, &ws, cx, |w, _| {
            w.topology()
                .and_then(|t| t.sessions.iter().find(|s| s.name == "other").map(|s| s.id))
        })
        .flatten()
        .unwrap_or_else(|| fail("no session `other`"));
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.switch_session(other, window, cx));
            }
        });
        wait_for(any, &ws, cx, "session `other` in the window", |w, _| {
            w.session_name().as_deref() == Some("other") && w.pane_ids().len() == 1
        })
        .await;
        let path = workspace_config::desktop_state::desktop_state_path();
        let load = || {
            path.as_deref()
                .and_then(workspace_config::desktop_state::load_desktop)
                .unwrap_or_default()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let saved = load();
            if saved
                .first()
                .is_some_and(|s| s.session_name.as_deref() == Some("other") && s.sidebar)
            {
                break;
            }
            if Instant::now() > deadline {
                fail(&format!("the arrangement was not saved: {saved:?}"));
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
        println!(
            "smoke: the sidebar switched the window to session `other`, and the arrangement was saved"
        );
        let name = second_window(any, &ws, cx).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let saved = load();
            if saved.len() == 2
                && saved[0].session_name.as_deref() == Some("other")
                && saved[1].session_name.as_deref() == Some(name.as_str())
            {
                break;
            }
            if Instant::now() > deadline {
                fail(&format!("both windows were not saved: {saved:?}"));
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
        println!("smoke: both windows were saved, each with its own session");
        std::process::exit(if painted { 0 } else { crate::INCONCLUSIVE });
    }

    /// A filter marks a line of the focused pane: the mark is drawn over
    /// the grid, hidden when switched off from the palette, and shown
    /// again, while tmux's copy of the line stays as the program wrote it.
    async fn decorations(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        daemon: &[String],
        cx: &mut AsyncApp,
    ) {
        let (socket, pane) = read(any, ws, cx, |w, _| Some((w.socket.clone(), w.focused?)))
            .flatten()
            .unwrap_or_else(|| fail("no focused pane for the decoration check"));
        let spec = api::FilterSpec {
            name: "smoke-marks".into(),
            panes: vec![pane],
            pattern: "^MARKED-LINE".into(),
            decorate: api::DecorationStyle::Highlight as i32,
            grant: Some(api::ScriptGrant {
                methods: vec!["taps.subscribe".into()],
                panes: vec![pane],
                all_panes: false,
            }),
            ..api::FilterSpec::default()
        };
        let installed = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                let c = client::Client::connect(&socket, "smoke")
                    .await
                    .map_err(|e| e.to_string())?;
                c.call(api::Request::FiltersInstall(spec))
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        })
        .join()
        .unwrap_or_else(|_| Err("the install thread panicked".into()));
        if let Err(e) = installed {
            fail(&format!("could not install a decorating filter: {e}"));
        }
        let target = format!("%{pane}");
        tmux(
            daemon,
            &[
                "send-keys",
                "-t",
                &target,
                "printf 'MARKED-LINE %s\\n' here",
                "Enter",
            ],
        );
        let drawn =
            move |w: &Workspace, cx: &gpui::App| w.pane_shell(pane).map(|s| s.read(cx).marks_drawn);
        wait_for(
            any,
            ws,
            cx,
            "the marked line drawn highlighted",
            move |w, cx| drawn(w, cx) == Some(1),
        )
        .await;
        palette(any, ws, cx, "decorations in this pane");
        wait_for(
            any,
            ws,
            cx,
            "the mark hidden after switching it off",
            move |w, cx| w.undecorated.contains(&pane) && drawn(w, cx) == Some(0),
        )
        .await;
        palette(any, ws, cx, "decorations in this pane");
        wait_for(
            any,
            ws,
            cx,
            "the mark drawn again after switching it on",
            move |w, cx| !w.undecorated.contains(&pane) && drawn(w, cx) == Some(1),
        )
        .await;
        let tmux_sees = tmux_output(daemon, &["capture-pane", "-p", "-t", &target]);
        if !tmux_sees
            .lines()
            .any(|l| l.trim_end() == "MARKED-LINE here")
        {
            fail(&format!(
                "tmux's copy of the marked line changed: {tmux_sees:?}"
            ));
        }
        println!(
            "smoke: a filter's mark was drawn over pane %{pane}, hidden from the palette, and drawn again; tmux's copy of the line was unchanged"
        );
    }

    /// One app package shown both ways: as a pane app (`dreamer-termapp`
    /// in a tmux window) and as a native panel drawn from the same model.
    /// Both show the same rows; a key pressed in the panel moves its
    /// selection; closing the panel ends its model.
    async fn panel(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        daemon: &[String],
        cx: &mut AsyncApp,
    ) {
        const MANIFEST: &str = r#"schema_version = 1
id = "smoke.windows"
version = "0.1.0"
title = "Windows of main"
capabilities = ["tmux.topology"]

[data.windows]
method = "tmux.topology"
path = "windows"
events = ["tmux.topology"]

[view]
type = "table"
source = "windows"
row_key = "id"
columns = ["id", "name"]
"#;
        let socket = read(any, ws, cx, |w, _| w.socket.clone())
            .unwrap_or_else(|| fail("no workspace for the panel check"));
        let dir = std::env::temp_dir().join(format!("dreamer-smoke-panel-{}", std::process::id()));
        let manifest = dir.join("app.toml");
        if let Err(e) =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&manifest, MANIFEST))
        {
            fail(&format!("could not write the app package: {e}"));
        }
        let termapp = std::env::current_exe()
            .ok()
            .and_then(|e| Some(e.parent()?.join("dreamer-termapp")))
            .filter(|p| p.exists())
            .unwrap_or_else(|| fail("no dreamer-termapp beside dreamer (build it first)"));
        let runtime = socket
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        // The pane app, in a window of its own.
        let command = format!("{} {}", termapp.display(), manifest.display());
        tmux(
            daemon,
            &[
                "new-window",
                "-d",
                "-n",
                "app-pane",
                "-t",
                "main",
                "-e",
                &format!("DREAMER_RUNTIME_DIR={runtime}"),
                &command,
            ],
        );
        // The same package as a panel.
        palette(any, ws, cx, "open an app package");
        palette_more(any, ws, cx, &manifest.display().to_string());
        let names = |w: &Workspace, cx: &gpui::App| -> Option<Vec<String>> {
            let shown = w.panel.as_ref()?.read(cx).shown.clone()?;
            (shown.link == app_model::Link::Live).then(|| {
                shown
                    .rows
                    .iter()
                    .map(|r| app_model::text(&r["name"]))
                    .collect()
            })
        };
        wait_for(
            any,
            ws,
            cx,
            "the panel drawing the windows, live",
            move |w, cx| names(w, cx).is_some_and(|n| n.iter().any(|n| n == "app-pane")),
        )
        .await;
        let in_panel = read(any, ws, cx, names).flatten().unwrap_or_default();
        let deadline = Instant::now() + Duration::from_secs(10);
        let in_pane = loop {
            let text = tmux_output(daemon, &["capture-pane", "-p", "-t", "main:app-pane"]);
            if text.contains("[live]") && in_panel.iter().all(|n| text.contains(n.as_str())) {
                break text;
            }
            if Instant::now() > deadline {
                fail(&format!(
                    "the pane app never showed the panel's rows {in_panel:?}:\n{text}"
                ));
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        };
        if !in_pane.contains("Windows of main") {
            fail(&format!("the pane app is not the same package:\n{in_pane}"));
        }
        // A real key press, dispatched to the focused panel.
        let selected = |w: &Workspace, cx: &gpui::App| {
            w.panel
                .as_ref()?
                .read(cx)
                .shown
                .as_ref()
                .map(|a| a.selected)
        };
        let before = read(any, ws, cx, selected).flatten();
        let _ = cx.update_window(any, |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse("down") {
                window.dispatch_keystroke(ks, cx);
            }
        });
        wait_for(
            any,
            ws,
            cx,
            "the panel's selection moved by a key",
            move |w, cx| selected(w, cx).is_some_and(|s| Some(s) != before),
        )
        .await;
        palette(any, ws, cx, "close the app panel");
        wait_for(any, ws, cx, "the panel closed", |w, _| w.panel.is_none()).await;
        tmux(daemon, &["kill-window", "-t", "main:app-pane"]);
        let _ = std::fs::remove_dir_all(&dir);
        println!(
            "smoke: one app package showed the same rows {in_panel:?} as a pane app and as a native panel; a key moved the panel's selection"
        );
    }

    /// Installed app packages' contributions: a package's palette command
    /// runs its typed action, its keybinding opens it as a panel, its
    /// sidebar entry and status item are there, and a package with a taken
    /// id and an invalid one are refused with reasons while the first
    /// loads.
    async fn contributions(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        daemon: &[String],
        cx: &mut AsyncApp,
    ) {
        let Some(config) = std::env::var_os("DREAMER_CONFIG_DIR").map(std::path::PathBuf::from)
        else {
            fail("the contributions check needs a private DREAMER_CONFIG_DIR");
        };
        let apps = app_model::installed::apps_dir(&config);
        let first = tmux_output(
            daemon,
            &["display-message", "-p", "-t", "main:0", "#{window_id}"],
        );
        let Some(window) = first
            .trim()
            .strip_prefix('@')
            .and_then(|n| n.parse::<u32>().ok())
        else {
            fail(&format!("no id for window main:0: {first:?}"));
        };
        let package = format!(
            r#"schema_version = 1
id = "smoke.contrib"
version = "0.1.0"
title = "Contrib"
capabilities = ["tmux.topology", "actions.submit"]

[data.windows]
method = "tmux.topology"
path = "windows"

[view]
type = "table"
source = "windows"
columns = ["id", "name"]

[commands.rename]
key = "r"
method = "actions.submit"
params = {{ action = {{ kind = {{ rename_window = {{ window = {window}, name = "by-contribution" }} }} }} }}

[[contributes.commands]]
id = "rename-first"
title = "Rename the first window"
run = "rename"

[[contributes.commands]]
id = "show"
title = "Show windows"
opens = "panel"
key = "ctrl-alt-o"

[[contributes.sidebar]]
id = "windows"
title = "Contrib windows"

[[contributes.status]]
id = "count"
label = "windows"
source = "windows"
"#
        );
        let put = |dir: &str, text: &str| {
            let d = apps.join(dir);
            std::fs::create_dir_all(&d).and_then(|()| std::fs::write(d.join("app.toml"), text))
        };
        let written = put("a-contrib", &package)
            .and_then(|()| put("b-same-id", &package))
            .and_then(|()| put("c-invalid", "schema_version = 1\nid = \"Not Valid\"\n"));
        if let Err(e) = written {
            fail(&format!("could not install the packages: {e}"));
        }
        wait_for(
            any,
            ws,
            cx,
            "one package loaded, two refused, and its status item",
            |w, _| {
                let c = &w.contributed;
                c.packages.len() == 1
                    && c.problems.len() == 2
                    && c.status
                        .get("smoke.contrib.count")
                        .is_some_and(|v| v.starts_with("windows "))
            },
        )
        .await;
        let (problems, status) = read(any, ws, cx, |w, _| {
            (
                w.contributed.problems.clone(),
                w.contributed.status.values().cloned().collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
        if !problems.iter().any(|p| p.contains("already installed"))
            || !problems.iter().any(|p| p.contains("c-invalid"))
        {
            fail(&format!("the refusals do not say why: {problems:?}"));
        }
        let sidebar = read(any, ws, cx, |w, _| {
            w.contributed
                .packages
                .iter()
                .flat_map(|p| {
                    p.manifest
                        .contributes
                        .sidebar
                        .iter()
                        .map(|e| e.title.clone())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if sidebar != ["Contrib windows"] {
            fail(&format!("the sidebar entries are {sidebar:?}"));
        }

        // The palette command runs the package's typed action.
        palette(any, ws, cx, "rename the first window");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let name = tmux_output(
                daemon,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &format!("@{window}"),
                    "#{window_name}",
                ],
            );
            if name.trim() == "by-contribution" {
                break;
            }
            if Instant::now() > deadline {
                let said = read(any, ws, cx, |w, _| w.contributed.outcome.clone()).flatten();
                fail(&format!(
                    "the contributed command never renamed the window: {said:?}"
                ));
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
        wait_for(any, ws, cx, "the command's outcome", |w, _| {
            w.contributed.outcome.as_deref() == Some("Rename the first window: done")
        })
        .await;

        // Its keybinding opens the package as a panel.
        let _ = cx.update_window(any, |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse("ctrl-alt-o") {
                window.dispatch_keystroke(ks, cx);
            }
        });
        wait_for(
            any,
            ws,
            cx,
            "the panel opened by a contributed key",
            |w, _| w.panel.is_some(),
        )
        .await;
        palette(any, ws, cx, "close the app panel");
        wait_for(any, ws, cx, "the panel closed", |w, _| w.panel.is_none()).await;
        let _ = std::fs::remove_dir_all(&apps);
        println!(
            "smoke: an installed package's palette command ran its action, its key opened its panel, its sidebar entry and status item ({}) were there, and a taken id and an invalid package were refused with reasons",
            status.join(", ")
        );
    }

    /// The second run, against a new tmux server: the saved session comes
    /// back by name (its old id belongs to the previous server), with the
    /// sidebar open.
    pub(super) async fn restore(
        any: AnyWindowHandle,
        ws: WeakEntity<Workspace>,
        cx: &mut AsyncApp,
    ) {
        wait_for(
            any,
            &ws,
            cx,
            "the restored session `other` with the sidebar",
            |w, _| w.session_name().as_deref() == Some("other") && w.sidebar,
        )
        .await;
        println!(
            "smoke: the saved arrangement came back on a new tmux server: session `other` by name, sidebar open"
        );
        // Every saved window came back, not only the first.
        let saved = workspace_config::desktop_state::desktop_state_path()
            .as_deref()
            .and_then(workspace_config::desktop_state::load_desktop)
            .map_or(0, |w| w.len());
        let open = cx.update(|cx| cx.windows().len());
        if open != saved.max(1) {
            fail(&format!("{saved} windows were saved but {open} opened"));
        }
        println!("smoke: all {open} saved windows opened again");
        std::process::exit(0);
    }

    /// New Window (Cmd-N, Ctrl+Shift+N): another OS window comes up on a
    /// session of its own that did not exist before, while this window
    /// keeps its session. Returns the new session's name.
    async fn second_window(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
    ) -> String {
        let (before, here) = read(any, ws, cx, |w, _| {
            let ids: Vec<u32> = w
                .topology()
                .map(|t| t.sessions.iter().map(|s| s.id).collect())
                .unwrap_or_default();
            (ids, w.session)
        })
        .unwrap_or_else(|| fail("no workspace"));
        cx.update(|cx| cx.dispatch_action(&crate::keymap::NewWindow));
        let deadline = Instant::now() + Duration::from_secs(10);
        let name = loop {
            let other = cx.update(|cx| cx.windows().into_iter().find(|w| *w != any));
            let shown = other.and_then(|h| {
                cx.update_window(h, |root, _, cx| {
                    let ws = root.downcast::<Workspace>().ok()?;
                    let w = ws.read(cx);
                    let s = w.session?;
                    (!w.pane_ids().is_empty()).then(|| (s, w.session_name()))
                })
                .ok()
                .flatten()
            });
            match shown {
                Some((s, _)) if before.contains(&s) => {
                    fail(&format!("the new window shows the existing session ${s}"))
                }
                Some((_, Some(name))) => break name,
                _ => {}
            }
            if Instant::now() > deadline {
                fail("New Window did not open a window on a new session");
            }
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        };
        if read(any, ws, cx, |w, _| w.session) != Some(here) {
            fail("the first window changed session when another opened");
        }
        println!("smoke: New Window opened a second window on a new session `{name}`");
        name
    }

    /// Type `filter` into a freshly opened palette and press Enter; then
    /// type `name` and press Enter, if given.
    fn palette(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp, filter: &str) {
        let filter = filter.to_owned();
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| {
                    w.open_palette(window, cx);
                    if let Some(p) = w.palette.as_mut() {
                        filter.chars().for_each(|c| p.type_char(c));
                    }
                    w.palette_enter(window, cx);
                });
            }
        });
    }

    fn palette_more(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
        text: &str,
    ) {
        let text = text.to_owned();
        let _ = cx.update_window(any, |_, window, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| {
                    if let Some(p) = w.palette.as_mut() {
                        text.chars().for_each(|c| p.type_char(c));
                    }
                    w.palette_enter(window, cx);
                });
            }
        });
    }

    /// Find in a pane: text typed while the bar is open is the query, never
    /// input for the program; the mirror's match is found (and named by its
    /// row and cells); Escape closes the bar and its matches.
    async fn find_in_pane(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
        type_into_first(any, ws, cx, "echo FIND''-ME-7");
        wait_for(any, ws, cx, "FIND-ME-7 in the first pane", |w, cx| {
            first_pane_has_line(w, cx, "FIND-ME-7")
        })
        .await;
        let first = |w: &Workspace| w.pane_ids().first().and_then(|id| w.pane_shell(*id));
        let _ = cx.update_window(any, |_, window, cx| {
            let Some(ws) = ws.upgrade() else { return };
            let shell = ws
                .read(cx)
                .pane_ids()
                .first()
                .and_then(|id| ws.read(cx).pane_shell(*id));
            if let Some(s) = shell {
                s.update(cx, |s, cx| {
                    use gpui::EntityInputHandler;
                    s.open_find(cx);
                    s.replace_text_in_range(None, "find-me", window, cx);
                });
            }
        });
        wait_for(any, ws, cx, "the match found in the mirror", |w, cx| {
            first(w).is_some_and(|s| {
                let s = s.read(cx);
                let Some(f) = &s.find else { return false };
                let one = f.found.as_ref().is_some_and(|x| x.hits.len() == 1);
                let Some(hit) = f.current().filter(|_| one) else {
                    return false;
                };
                // The row it names holds the line, at the cells it names.
                let view = s.frame.view;
                hit.start_col == 0
                    && hit.end_col == 7
                    && hit
                        .row
                        .checked_sub(view.offset)
                        .and_then(|i| s.frame.rows.get(usize::try_from(i).ok()?))
                        .is_some_and(|r| r.text().starts_with("FIND-ME-7"))
            })
        })
        .await;
        cx.background_executor()
            .timer(Duration::from_millis(300))
            .await;
        let leaked = read(any, ws, cx, |w: &Workspace, cx: &gpui::App| {
            first(w).is_some_and(|s| s.read(cx).frame.text_dump().contains("find-me"))
        });
        if leaked != Some(false) {
            fail("text typed into the find bar reached the program");
        }
        let _ = cx.update_window(any, |_, _, cx| {
            let Some(ws) = ws.upgrade() else { return };
            let shell = ws
                .read(cx)
                .pane_ids()
                .first()
                .and_then(|id| ws.read(cx).pane_shell(*id));
            if let Some(s) = shell {
                s.update(cx, |s, cx| s.close_find(cx));
            }
        });
        wait_for(any, ws, cx, "the find bar closed", |w, cx| {
            first(w).is_some_and(|s| s.read(cx).find.is_none())
        })
        .await;
        println!(
            "smoke: find in a pane matched FIND-ME-7 in its mirror, and nothing typed into the bar reached the program"
        );
    }

    /// Every palette command's effect shows up in tmux; a kill needs the
    /// second Enter.
    async fn palette_commands(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
        palette(any, ws, cx, "split pane right");
        wait_for(any, ws, cx, "a fifth pane from the palette", |w, _| {
            w.pane_ids().len() == 5
        })
        .await;

        palette(any, ws, cx, "kill pane");
        cx.background_executor()
            .timer(Duration::from_millis(400))
            .await;
        let still = read(any, ws, cx, |w, _| {
            (w.pane_ids().len(), w.palette.is_some())
        });
        if still != Some((5, true)) {
            fail(&format!(
                "the first Enter on a kill must only ask: {still:?}"
            ));
        }
        palette_more(any, ws, cx, "");
        wait_for(any, ws, cx, "the pane killed after confirming", |w, _| {
            w.pane_ids().len() == 4
        })
        .await;

        let before = read(any, ws, cx, |w, _| w.focused).flatten();
        palette(any, ws, cx, "focus next pane");
        wait_for(any, ws, cx, "focus on the next pane", |w, _| {
            w.focused != before
        })
        .await;

        palette(any, ws, cx, "new window");
        wait_for(any, ws, cx, "a third window from the palette", |w, _| {
            w.tab_count() == 3 && w.pane_ids().len() == 1
        })
        .await;
        palette(any, ws, cx, "rename window");
        palette_more(any, ws, cx, "from-palette");
        wait_for(
            any,
            ws,
            cx,
            "the window renamed from the palette",
            |w, _| w.active_window().is_some_and(|x| x.name == "from-palette"),
        )
        .await;
        // From the last window, next wraps to the first; previous wraps back.
        palette(any, ws, cx, "next window");
        wait_for(any, ws, cx, "the first window after wrapping", |w, _| {
            w.active_window().is_some_and(|x| x.index == 0) && w.pane_ids().len() == 4
        })
        .await;
        palette(any, ws, cx, "previous window");
        wait_for(
            any,
            ws,
            cx,
            "the last window after wrapping back",
            |w, _| w.active_window().is_some_and(|x| x.name == "from-palette"),
        )
        .await;
        palette(any, ws, cx, "next window");
        wait_for(any, ws, cx, "the first window again", |w, _| {
            w.active_window().is_some_and(|x| x.index == 0) && w.pane_ids().len() == 4
        })
        .await;
        println!(
            "smoke: the palette split, killed only after confirming, moved focus, made and renamed a window, and switched windows"
        );
    }

    /// With a refreshing display: click a pane that is not focused, require
    /// focus to move there, type a line, and require only that pane to run
    /// it. Without one (a locked screen), say so and return `false`.
    async fn click_and_type(
        any: AnyWindowHandle,
        ws: &WeakEntity<Workspace>,
        cx: &mut AsyncApp,
    ) -> bool {
        use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, point};
        let before = terminal_render::paint_count();
        let _ = cx.update_window(any, |_, window, _| window.refresh());
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        // Every visible pane must have been painted for clicks to land.
        let painted = read(any, ws, cx, |w, cx| {
            w.pane_ids()
                .iter()
                .all(|id| w.pane_shell(*id).is_some_and(|s| s.read(cx).grid.is_some()))
        });
        if terminal_render::paint_count() == before || painted != Some(true) {
            println!(
                "smoke: the display did not refresh (screen locked or asleep?); click and keyboard checks not run"
            );
            return false;
        }
        // This window owns the tmux window's size: tmux follows its area.
        wait_for(
            any,
            ws,
            cx,
            "the tmux window sized to the workspace",
            |w, _| {
                let shown = w.active_window().map(|x| (x.width, x.height));
                w.area.is_some() && shown == w.area
            },
        )
        .await;
        let area = read(any, ws, cx, |w, _| w.area)
            .flatten()
            .unwrap_or_default();
        println!(
            "smoke: tmux sized the window to the workspace's {}x{} cells",
            area.0, area.1
        );
        let Some((target, grid)) = read(any, ws, cx, |w, cx| {
            let target = w.pane_ids().into_iter().find(|id| Some(*id) != w.focused)?;
            Some((target, w.pane_shell(target)?.read(cx).grid?))
        })
        .flatten() else {
            fail("no unfocused, painted pane to click")
        };
        let at = point(
            grid.origin.x + grid.cell.width * 2.5,
            grid.origin.y + grid.cell.height * 1.5,
        );
        for ev in [
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
            }),
        ] {
            let _ = cx.update_window(any, move |_, window, cx| {
                window.dispatch_event(ev, cx);
            });
        }
        wait_for(any, ws, cx, "focus on the clicked pane", |w, _| {
            w.focused == Some(target)
        })
        .await;
        println!("smoke: clicking pane %{target} focused it");
        const KEYS: [&str; 12] = [
            "e", "c", "h", "o", "space", "t", "y", "p", "e", "d", "space", "x",
        ];
        for key in KEYS {
            let _ = cx.update_window(any, move |_, window, cx| {
                if let Ok(ks) = gpui::Keystroke::parse(key) {
                    window.dispatch_keystroke(ks, cx);
                }
            });
        }
        let _ = cx.update_window(any, |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse("enter") {
                window.dispatch_keystroke(ks, cx);
            }
        });
        let typed = |w: &Workspace, cx: &gpui::App, id: u32| {
            w.pane_shell(id).is_some_and(|s| {
                s.read(cx)
                    .frame
                    .text_dump()
                    .lines()
                    .any(|l| l.trim_end() == "typed x")
            })
        };
        wait_for(
            any,
            ws,
            cx,
            "the typed line in the focused pane",
            |w, cx| typed(w, cx, target),
        )
        .await;
        let elsewhere = read(any, ws, cx, |w, cx| {
            w.pane_ids()
                .into_iter()
                .filter(|id| *id != target && typed(w, cx, *id))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if !elsewhere.is_empty() {
            fail(&format!("typed text also reached {elsewhere:?}"));
        }
        println!("smoke: typed text reached only the focused pane %{target}");
        // The palette opens from the keyboard and Escape closes it.
        let shortcut = if cfg!(target_os = "macos") {
            "cmd-shift-p"
        } else {
            "ctrl-shift-p"
        };
        let _ = cx.update_window(any, move |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse(shortcut) {
                window.dispatch_keystroke(ks, cx);
            }
        });
        wait_for(any, ws, cx, "the palette opened by its shortcut", |w, _| {
            w.palette.is_some()
        })
        .await;
        let _ = cx.update_window(any, |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse("escape") {
                window.dispatch_keystroke(ks, cx);
            }
        });
        wait_for(any, ws, cx, "the palette closed by Escape", |w, _| {
            w.palette.is_none()
        })
        .await;
        println!("smoke: the palette opened from the keyboard and Escape closed it");
        drag_divider(any, ws, cx).await;
        true
    }

    /// Drag a vertical divider ten cells to the left in ten moves: tmux
    /// must end at that width, with no more resizes than moves.
    async fn drag_divider(any: AnyWindowHandle, ws: &WeakEntity<Workspace>, cx: &mut AsyncApp) {
        use gpui::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
            point,
        };
        let Some((d, origin, (cw, ch))) = read(any, ws, cx, |w, _| {
            let d = w
                .dividers()
                .into_iter()
                .find(|d| d.axis == super::Axis::Cols)?;
            Some((d, w.origin?, w.cell?))
        })
        .flatten() else {
            fail("no vertical divider to drag")
        };
        let y = origin.y + ch * (d.from as f32 + 1.5);
        let x = |col: f32| origin.x + cw * col;
        let start = d.at as f32 + 0.5;
        let mut events = vec![PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: point(x(start), y),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        })];
        for step in 1..=10 {
            events.push(PlatformInput::MouseMove(MouseMoveEvent {
                position: point(x(start - step as f32), y),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }));
        }
        events.push(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: point(x(start - 10.0), y),
            modifiers: Modifiers::default(),
            click_count: 1,
        }));
        for ev in events {
            let _ = cx.update_window(any, move |_, window, cx| {
                window.dispatch_event(ev, cx);
            });
            cx.background_executor()
                .timer(Duration::from_millis(15))
                .await;
        }
        let want = d.at - 10 - d.base;
        wait_for(any, ws, cx, "the dragged pane at its new width", |w, _| {
            w.topology()
                .and_then(|t| t.panes.iter().find(|p| p.id == d.pane))
                .is_some_and(|p| p.width == want)
        })
        .await;
        let sent = read(any, ws, cx, |w, _| w.resizes_sent).unwrap_or(0);
        if !(1..=11).contains(&sent) {
            fail(&format!("{sent} resize commands for a 10-move drag"));
        }
        println!(
            "smoke: dragging a divider resized pane %{} to {want} columns with {sent} resize commands",
            d.pane
        );
    }
}

/// Which way a divider moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    /// A vertical divider: dragging changes the left pane's width.
    Cols,
    /// A horizontal divider: dragging changes the upper pane's height.
    Rows,
}

/// A draggable gap between two panes: tmux leaves one cell between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Divider {
    /// The pane on the left of (or above) the gap: the one resized.
    pub pane: u32,
    pub axis: Axis,
    /// The gap's column (Cols) or row (Rows).
    pub at: u32,
    /// Its extent along the other axis, start inclusive, end exclusive.
    pub from: u32,
    pub to: u32,
    /// The resized pane's left (Cols) or top (Rows).
    pub base: u32,
}

/// The dividers between a window's panes, one per neighbouring pair.
pub(crate) fn dividers(panes: &[api::Pane]) -> Vec<Divider> {
    let mut out = Vec::new();
    for p in panes {
        let right = p.left + p.width;
        let bottom = p.top + p.height;
        for q in panes {
            let rows = (p.top.max(q.top), bottom.min(q.top + q.height));
            if q.left == right + 1 && rows.0 < rows.1 {
                out.push(Divider {
                    pane: p.id,
                    axis: Axis::Cols,
                    at: right,
                    from: rows.0,
                    to: rows.1,
                    base: p.left,
                });
            }
            let cols = (p.left.max(q.left), right.min(q.left + q.width));
            if q.top == bottom + 1 && cols.0 < cols.1 {
                out.push(Divider {
                    pane: p.id,
                    axis: Axis::Rows,
                    at: bottom,
                    from: cols.0,
                    to: cols.1,
                    base: p.top,
                });
            }
        }
    }
    // One divider per pane and gap: segments along several neighbours merge.
    out.sort_by_key(|d| (d.pane, d.axis as u8, d.at, d.from));
    let mut merged: Vec<Divider> = Vec::new();
    for d in out {
        match merged.last_mut() {
            Some(m) if m.pane == d.pane && m.axis == d.axis && m.at == d.at => {
                m.from = m.from.min(d.from);
                m.to = m.to.max(d.to);
            }
            _ => merged.push(d),
        }
    }
    merged
}

/// A theme in miniature: its background, holding its foreground and six
/// ANSI colors (red to cyan) as dots.
fn swatch(s: &'static terminal_ghostty::core::schemes::Scheme) -> gpui::Div {
    let hex = |c: terminal_ghostty::core::Rgb| {
        gpui::rgb((u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b))
    };
    let dot = |c| div().size(px(7.0)).rounded_full().bg(hex(c));
    div()
        .flex()
        .gap(px(3.0))
        .px(px(5.0))
        .py(px(4.0))
        .rounded_sm()
        .bg(hex(s.background))
        .child(dot(s.foreground))
        .children(s.ansi[1..7].iter().map(|&c| dot(c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_window_asks_for_a_new_session_with_the_default_shell_at_home() {
        let api::action::Kind::NewSession(a) = new_session_action(Some("/home/me".into())) else {
            panic!("a new window must ask for a new session, not reuse one");
        };
        assert_eq!(a.name, None, "tmux names it");
        assert!(a.argv.is_empty(), "the default shell");
        assert_eq!(a.cwd.as_deref(), Some("/home/me"));
        let api::action::Kind::NewSession(a) = new_session_action(Some("relative".into())) else {
            panic!("new session");
        };
        assert_eq!(a.cwd, None, "never a relative directory");
    }

    #[test]
    fn a_new_sessions_pane_finds_its_session() {
        let t = api::Topology {
            panes: vec![api::Pane {
                id: 7,
                window: 3,
                ..api::Pane::default()
            }],
            windows: vec![api::Window {
                id: 3,
                sessions: vec![5],
                ..api::Window::default()
            }],
            ..api::Topology::default()
        };
        assert_eq!(session_of_pane(&t, 7), Some(5));
        assert_eq!(session_of_pane(&t, 8), None);
    }

    fn pane(id: u32, left: u32, top: u32, width: u32, height: u32) -> api::Pane {
        api::Pane {
            id,
            left,
            top,
            width,
            height,
            ..api::Pane::default()
        }
    }

    #[test]
    fn a_pane_whose_program_is_an_app_host_draws_the_app() {
        let local = replica::Via::Local;
        let mut p = pane(1, 0, 0, 40, 10);
        assert_eq!(app_drawn(&p, &local), None);
        // Also once its host stopped: the last view, marked stale.
        p.app = Some(api::PaneApp {
            instance: "local.x-1".into(),
            app_id: "local.x".into(),
            live: false,
        });
        assert_eq!(app_drawn(&p, &local), Some("local.x-1"));
        // A host's panes: its own daemon serves their views, not this one.
        assert_eq!(app_drawn(&p, &replica::Via::Host("h".into())), None);
    }

    #[test]
    fn dividers_sit_in_the_gaps_tmux_leaves_between_panes() {
        // 100x30: %0 on the left (49 wide); %1 top right, %2 below it.
        let panes = [
            pane(0, 0, 0, 49, 30),
            pane(1, 50, 0, 50, 14),
            pane(2, 50, 15, 50, 15),
        ];
        let d = dividers(&panes);
        assert_eq!(
            d,
            vec![
                Divider {
                    pane: 0,
                    axis: Axis::Cols,
                    at: 49,
                    from: 0,
                    to: 30,
                    base: 0
                },
                Divider {
                    pane: 1,
                    axis: Axis::Rows,
                    at: 14,
                    from: 50,
                    to: 100,
                    base: 0
                },
            ],
            "{d:#?}"
        );
        // A single pane has none.
        assert!(dividers(&panes[..1]).is_empty());
    }
}
