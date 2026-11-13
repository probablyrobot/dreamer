//! The workspace window (spec 4.2, 14.1, 15.1): one tmux session, a narrow
//! tab strip of its windows, and the active window's panes, each drawn at
//! its tmux cell rectangle. tmux is authoritative for geometry; the view
//! follows `tmux.watch` and never assumes an action's result.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, FocusHandle, MouseButton, MouseDownEvent, Render, Window, div, font,
    prelude::*, px,
};
use replica::feed::Feed;
use replica::topology::TopologyFeed;
use terminal_render::cell_metrics;

use crate::theme::c;
use crate::{Shell, Smoke, load_frame, term_font};

/// Height of the tab strip.
const TAB_STRIP: f32 = 24.0;

/// A visible pane: its view and the feed that keeps it current.
struct PaneView {
    shell: Entity<Shell>,
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
    /// The pane keyboard input goes to (local; tmux is told on a click).
    focused: Option<u32>,
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
    /// The sidebar (sessions) is open.
    pub(crate) sidebar: bool,
    /// Where the arrangement is saved, what was saved at startup, and
    /// whether the first topology has been matched against it yet.
    state_path: Option<PathBuf>,
    saved: Option<workspace_config::desktop_state::DesktopState>,
    restored: bool,
    bounds: Option<[f32; 4]>,
    _bounds: Option<gpui::Subscription>,
    /// A save is running, and the arrangement changed since it began.
    saving: bool,
    save_dirty: bool,
}

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
            focused: None,
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
            sidebar: false,
            state_path: None,
            saved: None,
            restored: false,
            bounds: None,
            _bounds: None,
            saving: false,
            save_dirty: false,
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
        self.palette = Some(crate::palette::Palette::new());
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
        self.palette = None;
        if let Some(view) = self.focused.and_then(|f| self.panes.get(&f)) {
            let handle = view.shell.read(cx).focus.clone();
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
        cx.stop_propagation();
        cx.notify();
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
                window.remove_window();
                None
            }
            C::KillPane => pane.map(|pane| Kind::KillPane(api::KillPane { pane })),
            C::KillWindow => win.map(|window| Kind::KillWindow(api::KillWindow { window })),
            C::ToggleSidebar => {
                self.toggle_sidebar(cx);
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

    pub(crate) fn zoomed(&self) -> bool {
        self.active_window().is_some_and(|w| w.zoomed)
    }

    pub(crate) fn pane_ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.panes.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub(crate) fn pane_shell(&self, pane: u32) -> Option<Entity<Shell>> {
        self.panes.get(&pane).map(|p| p.shell.clone())
    }

    pub(crate) fn topology(&self) -> Option<&api::Topology> {
        self.current.as_ref()
    }

    /// Remember where the arrangement lives and what was saved there.
    pub(crate) fn with_state(&mut self, path: Option<PathBuf>) {
        self.saved = path
            .as_deref()
            .and_then(workspace_config::desktop_state::load_desktop_state);
        self.sidebar = self.saved.as_ref().is_some_and(|s| s.sidebar);
        self.state_path = path;
    }

    /// The arrangement as it is now, and where it goes.
    fn snapshot(&self) -> Option<(PathBuf, workspace_config::desktop_state::DesktopState)> {
        let (path, t) = (self.state_path.clone()?, self.current.as_ref()?);
        let session = t.sessions.iter().find(|s| Some(s.id) == self.session);
        Some((
            path,
            workspace_config::desktop_state::DesktopState {
                version: 0,
                endpoint: t.endpoint.clone(),
                incarnation: t.incarnation,
                session_id: session.map(|s| s.id),
                session_name: session.map(|s| s.name.clone()),
                sidebar: self.sidebar,
                bounds: self.bounds,
            },
        ))
    }

    /// Save the arrangement off the UI thread, one write at a time: the
    /// writer takes the latest state when it writes, and writes again if
    /// it changed meanwhile, so an older state never lands last.
    fn save_state(&mut self, cx: &mut Context<Self>) {
        self.save_dirty = true;
        if self.saving || self.state_path.is_none() {
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
                    ws.snapshot()
                }) else {
                    return;
                };
                if let Some((path, state)) = next {
                    cx.background_spawn(async move {
                        let _ = workspace_config::desktop_state::save_desktop_state(&path, &state);
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
        )
        .map(drop)
        {
            eprintln!("dreamer: {e}");
        }
    }

    /// The sidebar's hosts: each with its state; a click opens a window on
    /// it (connecting first). Nothing when the daemon knows no hosts.
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
        let launch = crate::FALLBACK_LAUNCH
            .get()
            .and_then(|t| crate::fallback::launch_argv(t, &attach));
        div()
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .child(
                div()
                    .px_1()
                    .bg(c(t.surface))
                    .font_family(term_font().family)
                    .child(attach.display.clone()),
            )
            .child(
                div()
                    .id(("fallback-copy", id))
                    .px_1()
                    .rounded_sm()
                    .bg(c(t.selected))
                    .child("Copy")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))
                    })),
            )
            .children(launch.map(|argv| {
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
            }))
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
        if !self.restored {
            // The first topology: the saved session, by id only for the same
            // server incarnation, else by name (spec 15.2: never reopen a
            // stale id from another server epoch).
            let sessions: Vec<(u32, &str)> =
                t.sessions.iter().map(|s| (s.id, s.name.as_str())).collect();
            self.session = workspace_config::desktop_state::choose_session(
                self.saved.as_ref(),
                &t.endpoint,
                t.incarnation,
                &sessions,
            );
            self.restored = true;
        } else if self
            .session
            .is_none_or(|s| !t.sessions.iter().any(|x| x.id == s))
        {
            self.session = t.sessions.first().map(|s| s.id);
        }
        let first_save = self.current.is_none();
        self.current = Some(t);
        let visible = self.visible();
        self.panes
            .retain(|id, _| visible.iter().any(|p| p.id == *id));
        for p in &visible {
            if !self.panes.contains_key(&p.id)
                && let Some(view) = self.open_pane(p.id, window, cx)
            {
                self.panes.insert(p.id, view);
            }
        }
        if first_save {
            // Record the server incarnation the session id now belongs to.
            self.save_state(cx);
        }
        // Keep focus on a visible pane: tmux's active one if ours is gone.
        if self.focused.is_none_or(|f| !self.panes.contains_key(&f)) {
            let active = self.active_window().and_then(|w| w.active_pane);
            let target = active.filter(|a| self.panes.contains_key(a));
            self.set_focus(target.or_else(|| visible.first().map(|p| p.id)), window, cx);
        }
        cx.notify();
    }

    fn open_pane(
        &self,
        pane: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneView> {
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
        Some(PaneView { shell })
    }

    /// Move keyboard focus to `pane`, and tell the panes' programs (focus
    /// reports, and which window the daemon addresses clipboard writes to).
    fn set_focus(&mut self, pane: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        if pane == self.focused {
            return;
        }
        let report = |id: Option<u32>, gained: bool, cx: &mut Context<Self>| {
            if let Some(view) = id.and_then(|i| self.panes.get(&i))
                && let Some(feed) = view.shell.read(cx).feed.clone()
            {
                feed.send(api::terminal_input::Event::Focus(api::FocusInput {
                    gained,
                }));
            }
        };
        report(self.focused, false, cx);
        self.focused = pane;
        report(pane, true, cx);
        if let Some(view) = pane.and_then(|p| self.panes.get(&p)) {
            let handle = view.shell.read(cx).focus.clone();
            window.focus(&handle, cx);
        }
    }

    /// A click on a pane: focus it here and make it tmux's active pane.
    pub(crate) fn click_pane(&mut self, pane: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.set_focus(Some(pane), window, cx);
        self.topology
            .act(api::action::Kind::SelectPane(api::SelectPane { pane }));
        cx.notify();
    }

    pub(crate) fn click_tab_for_smoke(&mut self, window_id: u32, cx: &mut Context<Self>) {
        self.click_tab(window_id, cx);
    }

    fn click_tab(&mut self, window_id: u32, cx: &mut Context<Self>) {
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
            .map(|p| p.shell.read(cx).wanted_title())
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

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_title(window, cx);
        let t = *crate::theme::for_window(window);
        let tf = term_font();
        let m = cell_metrics(
            window,
            &font(tf.family.clone()),
            px(tf.size),
            tf.line_height,
        );
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
                TopologyStatus::Connecting => Some("connecting…".to_owned()),
                TopologyStatus::Following => {
                    let behind = self
                        .panes
                        .values()
                        .filter(|p| {
                            p.shell
                                .read(cx)
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
        let strip = div()
            .h(px(TAB_STRIP))
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .items_center()
            .bg(c(t.surface))
            .text_size(px(12.0))
            .children(tabs.into_iter().map(|w| {
                let id = w.id;
                let is_active = Some(id) == active;
                div()
                    .id(("tab", id as usize))
                    .px_2()
                    .rounded_sm()
                    .text_color(if is_active { c(t.text) } else { c(t.muted) })
                    .when(is_active, |d| d.bg(c(t.selected)))
                    .child(format!("{} {}", w.index, w.name))
                    .on_click(cx.listener(move |ws, _, _, cx| ws.click_tab(id, cx)))
            }))
            .child(div().flex_1())
            .children(status.map(|s| div().px_2().text_color(c(t.warning)).child(s)));

        // The sidebar, on demand: sessions, the shown one highlighted.
        let sidebar = self.sidebar.then(|| {
            let sessions: Vec<api::Session> = self
                .current
                .as_ref()
                .map(|t| t.sessions.clone())
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
        });

        // Panes at their tmux cell rectangles, in canonical cells scaled to
        // this view; a zoomed pane fills the window's area.
        let w = self.active_window().cloned();
        let zoomed = w.as_ref().is_some_and(|w| w.zoomed);
        let unsupported = w.as_ref().is_some_and(|w| !w.layout_supported);
        let focused = self.focused;
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
                let shell = self.panes.get(&p.id)?.shell.clone();
                let (left, top, cols, rows) = match (&w, zoomed) {
                    (Some(w), true) => (0, 0, w.width, w.height),
                    _ => (p.left, p.top, p.width, p.height),
                };
                let id = p.id;
                let entity = entity.clone();
                Some(
                    div()
                        .absolute()
                        .left(m.width * left as f32)
                        .top(m.height * top as f32)
                        .w(m.width * cols as f32)
                        .h(m.height * rows as f32)
                        .when(Some(id) == focused && !zoomed, |d| {
                            d.border_1().border_color(c(t.focus))
                        })
                        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                            entity.update(cx, |ws, cx| {
                                if ws.focused != Some(id) {
                                    ws.click_pane(id, window, cx);
                                }
                            });
                        })
                        .child(shell)
                        .children(mode_banners.remove(&id))
                        .when(Some(id) == focused, |d| {
                            // Zoom control on the focused pane.
                            d.child(
                                div()
                                    .id(("zoom", id as usize))
                                    .absolute()
                                    .top_0()
                                    .right_0()
                                    .px_1()
                                    .text_size(px(11.0))
                                    .text_color(c(t.faint))
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
            let (prompt, rows): (String, Vec<(String, bool)>) = match &p.stage {
                Stage::Choose => (
                    format!("> {}", p.filter),
                    p.shown()
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (c.label().to_owned(), i == p.selected))
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
                        .map(|(i, (label, value, _))| (format!("{label}: {value}"), i == f.at))
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
                        .w(px(460.0))
                        .p_2()
                        .rounded_md()
                        .bg(c(t.raised))
                        .border_1()
                        .border_color(c(t.focus))
                        .text_size(px(13.0))
                        .text_color(c(t.text))
                        .child(div().pb_1().child(prompt))
                        .children(rows.into_iter().map(|(label, selected)| {
                            div()
                                .px_2()
                                .rounded_sm()
                                .when(selected, |d| d.bg(c(t.selected)))
                                .when(label.starts_with("Kill"), |d| d.text_color(c(t.danger)))
                                .child(label)
                        })),
                )
        });
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
            .track_focus(&self.focus)
            .on_action(cx.listener(|ws, _: &crate::keymap::ZoomPane, _, cx| ws.toggle_zoom(cx)))
            .on_action(
                cx.listener(|ws, _: &crate::keymap::CommandPalette, window, cx| {
                    ws.open_palette(window, cx)
                }),
            )
            .on_action(
                cx.listener(|ws, _: &crate::keymap::ToggleSidebar, _, cx| ws.toggle_sidebar(cx)),
            )
            .children(palette)
            .child(strip)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .children(sidebar)
                    .child(area),
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
            // tmux leaves one cell between panes: the divider.
            .bg(c(t.divider))
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

/// Open a workspace window following the daemon `launch` names. With
/// `smoke` (the daemon's arguments, to find the isolated tmux), run the
/// workspace smoke.
pub(crate) fn open(
    cx: &mut gpui::App,
    launch: client::Launch,
    smoke: Option<(Vec<String>, bool)>,
    open_at_start: Vec<String>,
) -> Result<(), String> {
    open_via(cx, launch, replica::Via::Local, smoke, open_at_start).map(drop)
}

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
    smoke: Option<(Vec<String>, bool)>,
    open_at_start: Vec<String>,
) -> Result<(gpui::AnyWindowHandle, gpui::WeakEntity<Workspace>), String> {
    use gpui::{Bounds, WindowBounds, WindowOptions, size};
    let socket = launch.socket.clone();
    let daemon_args = launch.args.clone();
    let topology = Arc::new(
        TopologyFeed::start_via(socket.clone(), via.clone(), Some(launch))
            .map_err(|e| format!("cannot follow the tmux topology: {e}"))?,
    );
    // The saved arrangement. A smoke run uses one only in a private
    // directory it was given, never the user's. A host's window does not
    // save one: the arrangement is the local tmux's.
    let state_path = if via != replica::Via::Local
        || (smoke.is_some() && std::env::var_os("DREAMER_DESKTOP_STATE_DIR").is_none())
    {
        None
    } else {
        workspace_config::desktop_state::desktop_state_path()
    };
    let saved_bounds = state_path
        .as_deref()
        .and_then(workspace_config::desktop_state::load_desktop_state)
        .and_then(|s| s.bounds);
    let bounds = match saved_bounds {
        Some([x, y, w, h]) if w >= 200.0 && h >= 120.0 => {
            Bounds::new(gpui::point(px(x), px(y)), size(px(w), px(h)))
        }
        _ => Bounds::centered(None, size(px(1000.), px(560.)), cx),
    };
    let t = topology.clone();
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some(crate::APP_ID.into()),
                ..Default::default()
            },
            |window, cx| {
                let ws = cx.new(|cx| {
                    let mut ws = Workspace::new(cx, t, socket, daemon_args);
                    ws.via = via.clone();
                    ws.with_state(state_path);
                    ws
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
    if let Some((daemon, restore)) = smoke {
        let weak = ws.downgrade();
        if restore {
            cx.spawn(async move |cx| smoke::restore(any, weak, cx).await)
                .detach();
        } else {
            cx.spawn(async move |cx| smoke::run(any, weak, daemon, cx).await)
                .detach();
        }
    }
    Ok((any, ws.downgrade()))
}

mod smoke {
    //! `dreamer --smoke-workspace`: against an isolated tmux whose session
    //! `main` has two windows, the first split into three panes, each pane
    //! having printed `READY-%<pane id>`.

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
                fail(&format!("never saw: {what}"));
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
                fail(&format!("never saw: {what}"));
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

        let painted = click_and_type(any, &ws, cx).await;

        // Selecting a tab switches the tmux window, seen in the topology.
        let second = read(any, &ws, cx, |w, _| {
            w.topology()
                .and_then(|t| t.windows.iter().find(|x| x.index == 1).map(|x| x.id))
        })
        .flatten()
        .unwrap_or_else(|| fail("no second window"));
        let _ = cx.update_window(any, |_, _, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.click_tab_for_smoke(second, cx));
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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let saved = path
                .as_deref()
                .and_then(workspace_config::desktop_state::load_desktop_state);
            if saved
                .as_ref()
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
        std::process::exit(if painted { 0 } else { crate::INCONCLUSIVE });
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
        std::process::exit(0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
