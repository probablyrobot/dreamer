//! The workspace window (spec 4.2, 14.1, 15.1): one tmux session, a narrow
//! tab strip of its windows, and the active window's panes, each drawn at
//! its tmux cell rectangle. tmux is authoritative for geometry; the view
//! follows `tmux.watch` and never assumes an action's result.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, FocusHandle, MouseButton, MouseDownEvent, Render, Window, div, font,
    prelude::*, px, rgb,
};
use replica::feed::Feed;
use replica::topology::TopologyFeed;
use terminal_render::cell_metrics;

use crate::{Shell, Smoke, load_frame, term_font};

/// Height of the tab strip.
const TAB_STRIP: f32 = 24.0;

/// A visible pane: its view and the feed that keeps it current.
struct PaneView {
    shell: Entity<Shell>,
}

pub(crate) struct Workspace {
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
}

impl Workspace {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        topology: Arc<TopologyFeed>,
        socket: PathBuf,
    ) -> Workspace {
        Workspace {
            topology,
            socket,
            current: None,
            session: None,
            panes: HashMap::new(),
            focused: None,
            focus: cx.focus_handle(),
            title: String::new(),
        }
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
        if self
            .session
            .is_none_or(|s| !t.sessions.iter().any(|x| x.id == s))
        {
            self.session = t.sessions.first().map(|s| s.id);
        }
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
        let feed = Feed::start(self.socket.clone(), Some(pane), None)
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
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_title(window, cx);
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
        let strip = div()
            .h(px(TAB_STRIP))
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .items_center()
            .bg(rgb(0x101216))
            .text_size(px(12.0))
            .children(tabs.into_iter().map(|w| {
                let id = w.id;
                let is_active = Some(id) == active;
                div()
                    .id(("tab", id as usize))
                    .px_2()
                    .rounded_sm()
                    .text_color(if is_active {
                        rgb(0xe6e6e6)
                    } else {
                        rgb(0x8a909a)
                    })
                    .when(is_active, |d| d.bg(rgb(0x2a2f38)))
                    .child(format!("{} {}", w.index, w.name))
                    .on_click(cx.listener(move |ws, _, _, cx| ws.click_tab(id, cx)))
            }));

        // Panes at their tmux cell rectangles, in canonical cells scaled to
        // this view; a zoomed pane fills the window's area.
        let w = self.active_window().cloned();
        let zoomed = w.as_ref().is_some_and(|w| w.zoomed);
        let unsupported = w.as_ref().is_some_and(|w| !w.layout_supported);
        let focused = self.focused;
        let panes = self.visible().into_iter().filter_map(|p| {
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
                        d.border_1().border_color(rgb(0x3d4a5c))
                    })
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                        entity.update(cx, |ws, cx| {
                            if ws.focused != Some(id) {
                                ws.click_pane(id, window, cx);
                            }
                        });
                    })
                    .child(shell),
            )
        });
        let notice = unsupported.then(|| {
            div()
                .p_4()
                .text_color(rgb(0xb8bec8))
                .child("This window uses a layout Dreamer cannot draw yet (floating panes). Attach a normal tmux client to use it.")
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x14161a))
            .track_focus(&self.focus)
            .child(strip)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .overflow_hidden()
                    // tmux leaves one cell between panes: the divider.
                    .bg(rgb(0x1f232a))
                    .children(panes)
                    .children(notice),
            )
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
            let Some(update) = topology.take() else {
                continue;
            };
            let Some(t) = update.topology else { continue };
            let alive = cx.update_window(any, |_, window, cx| {
                weak.update(cx, |ws, cx| ws.update_topology(t, window, cx))
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
    smoke: Option<Vec<String>>,
) -> Result<(), String> {
    use gpui::{Bounds, WindowBounds, WindowOptions, size};
    let socket = launch.socket.clone();
    let topology = Arc::new(
        TopologyFeed::start(socket.clone(), Some(launch))
            .map_err(|e| format!("cannot follow the tmux topology: {e}"))?,
    );
    let bounds = Bounds::centered(None, size(px(1000.), px(560.)), cx);
    let t = topology.clone();
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let ws = cx.new(|cx| Workspace::new(cx, t, socket));
                let focus = ws.read(cx).focus.clone();
                window.focus(&focus, cx);
                ws
            },
        )
        .map_err(|e| format!("could not open a window: {e:#}"))?;
    let any: gpui::AnyWindowHandle = window.into();
    let ws = window
        .update(cx, |_, _, cx| cx.entity())
        .map_err(|e| format!("{e:#}"))?;
    follow_topology(any, ws.clone(), topology, cx);
    if let Some(daemon) = smoke {
        let weak = ws.downgrade();
        cx.spawn(async move |cx| smoke::run(any, weak, daemon, cx).await)
            .detach();
    }
    Ok(())
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

    pub(super) async fn run(
        any: AnyWindowHandle,
        ws: WeakEntity<Workspace>,
        daemon: Vec<String>,
        cx: &mut AsyncApp,
    ) {
        wait_for(any, &ws, cx, "two tabs and three ready panes", |w, cx| {
            w.topology().is_some_and(|t| t.windows.len() == 2) && panes_ready(w, cx, 3)
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
        std::process::exit(if painted { 0 } else { crate::INCONCLUSIVE });
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
        true
    }
}
