//! Web panes: a tmux placeholder pane marked with a web page
//! (`Pane.web`), which the desktop fills with the page, drawn by Chromium
//! itself in a native view kept on the pane's rectangle.
//!
//! Chromium's view sits above everything GPUI draws. The workspace places
//! each web pane ([`Place`]): its view follows the pane's rectangle while
//! the pane is drawn, and is hidden (the page kept) while it is not, on
//! another tab or zoomed away. While an overlay of the desktop's (the
//! palette, the context menu) is open, the pane shows the page's last
//! snapshot instead, taken as the overlay opens; the overlay shows once
//! the live page is off the screen ([`decide`]).
//!
//! Input: Chromium's view takes the mouse, the wheel, and trackpad
//! gestures itself, and the keys while the pane has Dreamer's focus; a
//! click in the page gives the pane that focus. Dreamer's own shortcuts
//! come first: AppKit offers every shortcut to the window's own view
//! before the page sees the key, so they work from inside a page, and the
//! edit shortcuts (copy, cut, paste, select all) become Chromium's own
//! edit commands there.
//!
//! The page's title and scroll position are kept in the placeholder
//! (`web.state`), so a page opened again (after the desktop restarts, or
//! once its view was let go) scrolls back to where it was.
//!
//! The same view draws a `web` node of an app pane's tree (an app's own
//! page): it is then placed where the node is drawn, opens in the app's
//! own browser profile, may navigate only to the app's origin and the
//! origins the app allows, and its page on the app's origin calls the
//! daemon through the bridge as the app ([`web_engine::site`]). Its title
//! and scroll position are not kept anywhere.
//!
//! Chromium starts with the first web pane, not with the app. A build
//! without the `web` feature, or one not running from a bundle with the
//! framework and its helper apps, shows the page's address and why it
//! cannot draw it.

use std::sync::Arc;

use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render, RenderImage,
    Styled, StyledImage as _, Subscription, WeakEntity, Window, div, px,
};
use web_engine::Edit;

use crate::theme::c;
use crate::workspace::Workspace;

/// Where the workspace has a web pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// Not in the window: on another tab, or another pane is zoomed.
    Hidden,
    /// Drawn, with nothing of the desktop's over it.
    Shown,
    /// Drawn, under an overlay of the desktop's.
    Covered,
}

/// A covered page's snapshot.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
enum Snapshot {
    None,
    /// Asked of Chromium; the page stays live until it comes.
    Asked,
    /// The image, or `None` when Chromium gave none in time.
    Taken(Option<Arc<RenderImage>>),
}

/// Whether the page's view is on screen, and whether to ask for a snapshot
/// first, for a pane at `place` whose view is `live` (on screen now) and
/// whose snapshot is `asked` for or `taken`. A hidden pane's page is never
/// on screen. A covered page stays live until its snapshot is taken, so
/// the pane never goes blank under the overlay; a page that was not live
/// is not shown for it.
fn decide(place: Place, live: bool, asked: bool, taken: bool) -> (bool, bool) {
    match place {
        Place::Hidden => (false, false),
        Place::Shown => (true, false),
        Place::Covered if !live || taken => (false, false),
        Place::Covered => (true, !asked),
    }
}

pub(crate) struct WebPane {
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) pane: u32,
    pub(crate) url: String,
    pub(crate) focus: FocusHandle,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    workspace: WeakEntity<Workspace>,
    place: Place,
    /// Whether the page's view may be on screen (see [`decide`]). Hiding
    /// takes effect at once; showing, when the pane is next painted.
    live: bool,
    snapshot: Snapshot,
    /// The page's title and scroll position as the placeholder keeps them.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    kept: (String, i32, i32),
    /// Why the page is not drawn.
    pub(crate) problem: Option<String>,
    /// The app whose page this is, for a `web` node of an app pane.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    app: Option<web_engine::AppPage>,
    #[cfg(feature = "web")]
    pub(crate) view: Option<web_engine::WebView>,
    /// Waiting for Chromium to make or load the page's profile before the
    /// page opens.
    #[cfg(feature = "web")]
    profile_wait: bool,
    /// The page has the keys exactly while the pane has Dreamer's focus.
    _focus: [Subscription; 2],
}

impl WebPane {
    /// `kept`: the page's title and scroll position as the placeholder
    /// keeps them; the page opens scrolled there. `app`: the app whose
    /// page it is, for a `web` node in app pane `pane`.
    pub(crate) fn new(
        pane: u32,
        url: String,
        kept: (String, i32, i32),
        app: Option<web_engine::AppPage>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WebPane {
        // Chromium starts with the first web pane, here rather than while
        // the pane paints.
        #[cfg(feature = "web")]
        let problem = engine::ensure(cx).err();
        #[cfg(not(feature = "web"))]
        let problem = Some(
            "This build of Dreamer has no web engine (web panes are built on macOS with the `web` feature)."
                .to_owned(),
        );
        // Whatever takes Dreamer's focus from the pane (another pane, the
        // palette, the find bar) takes the keys from the page too.
        let focus = cx.focus_handle();
        let _focus = [
            cx.on_focus(&focus, window, |w, _, _| w.take_focus()),
            cx.on_blur(&focus, window, |w, _, _| w.release_focus()),
        ];
        WebPane {
            pane,
            url,
            focus,
            workspace,
            place: Place::Shown,
            live: true,
            snapshot: Snapshot::None,
            kept,
            problem,
            app,
            #[cfg(feature = "web")]
            view: None,
            #[cfg(feature = "web")]
            profile_wait: false,
            _focus,
        }
    }

    /// The workspace laid the pane out at `place`. Returns whether an
    /// overlay may show over it: false while its live page waits for its
    /// snapshot.
    pub(crate) fn set_place(
        &mut self,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        #[cfg(feature = "web")]
        let on_screen = self.view.as_ref().is_some_and(web_engine::WebView::visible);
        #[cfg(not(feature = "web"))]
        let on_screen = false;
        let (asked, taken) = match self.snapshot {
            Snapshot::None => (false, false),
            Snapshot::Asked => (true, false),
            Snapshot::Taken(_) => (true, true),
        };
        let (live, ask) = decide(place, on_screen, asked, taken);
        if place != Place::Covered {
            if let Snapshot::Taken(Some(image)) =
                std::mem::replace(&mut self.snapshot, Snapshot::None)
            {
                let _ = window.drop_image(image);
            }
            self.snapshot = Snapshot::None;
        }
        #[cfg(feature = "web")]
        {
            if ask {
                self.ask_snapshot(cx);
            }
            // Leaving the screen: keep where the page is.
            if self.place == Place::Shown && place != Place::Shown {
                self.keep_state(cx);
            }
            if !live && let Some(v) = &self.view {
                v.set_visible(false);
            }
        }
        #[cfg(not(feature = "web"))]
        let _ = ask;
        if (place, live) != (self.place, self.live) {
            cx.notify();
        }
        self.place = place;
        self.live = live;
        !(live && place == Place::Covered)
    }

    /// Keyboard focus moved to this pane: the page takes the keys.
    pub(crate) fn take_focus(&self) {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            v.focus();
        }
    }

    /// Whether the page has the window's keyboard focus now (a click in
    /// it), as opposed to Chromium reporting focus it took by itself.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn page_has_focus(&self) -> bool {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            return v.has_focus();
        }
        false
    }

    /// Keyboard focus moved elsewhere: the window's own view takes it back.
    pub(crate) fn release_focus(&self) {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            v.release_focus();
        }
    }

    /// Navigations of an app's page refused so far (smoke evidence).
    #[cfg(feature = "web")]
    pub(crate) fn blocked(&self) -> Vec<String> {
        self.view
            .as_ref()
            .map(web_engine::WebView::blocked)
            .unwrap_or_default()
    }

    /// One of Chromium's edit commands in the page, for the desktop's edit
    /// shortcuts.
    fn edit(&mut self, edit: Edit) {
        #[cfg(feature = "web")]
        if let Some(v) = &self.view {
            v.edit(edit);
        }
        #[cfg(not(feature = "web"))]
        let _ = edit;
    }

    /// Whether the pane shows the page's snapshot now (smoke evidence).
    #[cfg(feature = "web")]
    pub(crate) fn shows_snapshot(&self) -> bool {
        !self.live && matches!(self.snapshot, Snapshot::Taken(Some(_)))
    }

    /// Ask Chromium for the page as drawn now. The page stays live until
    /// the image comes (decoded off this thread), or at most
    /// [`SNAPSHOT_WAIT`], and the workspace lays the pane out again then.
    #[cfg(feature = "web")]
    fn ask_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        self.snapshot = Snapshot::Asked;
        let (tx, rx) = async_channel::bounded(1);
        v.capture(move |reply| {
            let _ = tx.try_send(reply);
        });
        cx.spawn(async move |this, cx| {
            let ex = cx.background_executor().clone();
            let decoded = ex.spawn(async move {
                let reply = rx.recv().await.ok()?.ok()?;
                decode_snapshot(&reply).ok()
            });
            let timer = ex.timer(SNAPSHOT_WAIT);
            let image = futures_lite::future::or(decoded, async {
                timer.await;
                None
            })
            .await;
            let _ = this.update(cx, |w, cx| {
                if matches!(w.snapshot, Snapshot::Asked) {
                    w.snapshot = Snapshot::Taken(image);
                    w.relayout(cx);
                }
            });
        })
        .detach();
    }

    /// Lay the panes out again: what this pane may show changed.
    #[cfg(feature = "web")]
    fn relayout(&self, cx: &mut Context<Self>) {
        cx.notify();
        let _ = self.workspace.update(cx, |_, cx| cx.notify());
    }

    /// Ask the page where it is scrolled to, and keep that and its title in
    /// the placeholder if they changed.
    #[cfg(feature = "web")]
    fn keep_state(&self, cx: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        // An app's page has no placeholder to keep it in.
        if self.app.is_some() || v.load_status().is_none() {
            return;
        }
        let title = v.title();
        let (tx, rx) = async_channel::bounded(1);
        v.scroll_position(move |at| {
            let _ = tx.try_send(at);
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((x, y))) = rx.recv().await else {
                return;
            };
            let _ = this.update(cx, |w, cx| {
                let kept = (title, x, y);
                if w.kept == kept {
                    return;
                }
                let state = api::WebState {
                    pane: w.pane,
                    title: kept.0.clone(),
                    scroll_x: x,
                    scroll_y: y,
                    ..api::WebState::default()
                };
                w.kept = kept;
                let _ = w.workspace.update(cx, |ws, _| ws.keep_web_state(state));
            });
        })
        .detach();
    }

    /// Whether the page's browser profile is ready for it to open. If not,
    /// Chromium is making (or loading) it, and the pane paints again once
    /// it is.
    #[cfg(feature = "web")]
    fn profile_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.profile_wait {
            return false;
        }
        let (tx, rx) = async_channel::bounded::<()>(1);
        let app = self.app.as_ref().map(|a| a.app.as_str());
        match web_engine::profile_ready(app, move || {
            let _ = tx.try_send(());
        }) {
            Ok(true) => true,
            Ok(false) => {
                self.profile_wait = true;
                cx.spawn(async move |this, cx| {
                    let _ = rx.recv().await;
                    let _ = this.update(cx, |w, cx| {
                        w.profile_wait = false;
                        cx.notify();
                    });
                })
                .detach();
                false
            }
            Err(e) => {
                self.problem = Some(e);
                cx.notify();
                false
            }
        }
    }

    /// Painted at `bounds`: open the page on first paint, then keep its
    /// view on the pane.
    #[cfg(feature = "web")]
    fn painted(
        &mut self,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.problem.is_some() {
            return;
        }
        let rect = web_engine::Rect {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        };
        if self.view.is_none() && !self.profile_ready(cx) {
            return;
        }
        if self.view.is_none() {
            // A click in the page focuses this pane, as a click on a
            // terminal pane does.
            let (tx, rx) = async_channel::bounded::<()>(1);
            let any = window.window_handle();
            let (ws, pane) = (self.workspace.clone(), self.pane);
            cx.spawn(async move |_, cx| {
                use gpui::AppContext as _;
                while rx.recv().await.is_ok() {
                    let _ = cx.update_window(any, |_, window, cx| {
                        let _ = ws.update(cx, |ws, cx| ws.page_focused(pane, window, cx));
                    });
                }
            })
            .detach();
            let app = self.app.clone();
            match web_engine::WebView::open(window, rect, &self.url, app, move || {
                let _ = tx.try_send(());
            }) {
                Ok(v) => {
                    engine::opened(cx.entity().downgrade());
                    // The pane was focused before its page existed.
                    if self.focus.is_focused(window) {
                        v.focus();
                    }
                    let (_, x, y) = self.kept;
                    if (x, y) != (0, 0) {
                        v.restore_scroll(x, y);
                    }
                    self.view = Some(v);
                    // While the page is on screen, its state is kept every
                    // few seconds.
                    cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor().timer(KEEP_EVERY).await;
                            let alive = this.update(cx, |w, cx| {
                                if w.place == Place::Shown {
                                    w.keep_state(cx);
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                    })
                    .detach();
                }
                Err(e) => {
                    self.problem = Some(e);
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(v) = &self.view {
            v.place(rect);
            v.set_visible(self.live);
        }
    }
}

/// How long a covered page waits for its snapshot (and the overlay with
/// it) before it is hidden without one.
#[cfg(feature = "web")]
const SNAPSHOT_WAIT: std::time::Duration = std::time::Duration::from_millis(400);

/// How often a page on screen has its title and scroll position kept.
#[cfg(feature = "web")]
const KEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The image in Chromium's reply to a capture (`Page.captureScreenshot`:
/// a PNG, base64, in `data`). Slow: never on the UI thread.
#[cfg(feature = "web")]
fn decode_snapshot(reply: &[u8]) -> Result<Arc<RenderImage>, String> {
    let v: serde_json::Value = serde_json::from_slice(reply).map_err(|e| e.to_string())?;
    let data = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or("no image in the reply")?;
    terminal_render::decode_image(&api::json::decode_base64(data)?)
}

impl Render for WebPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::for_window(window);
        // What shows where the page is not drawn: under the page for a
        // moment as it starts, or instead of it.
        let notice = div()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(12.0))
            .text_color(c(t.muted))
            .child(format!("web: {}", shorten(&self.url, 200)))
            .children(
                self.problem
                    .clone()
                    .map(|p| div().text_color(c(t.warning)).child(p)),
            );
        // Under an overlay, the page as it was when the overlay opened.
        let snapshot = match &self.snapshot {
            Snapshot::Taken(Some(image)) if !self.live => Some(
                gpui::img(image.clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(gpui::ObjectFit::Fill),
            ),
            _ => None,
        };
        let base = div()
            .size_full()
            .relative()
            .bg(c(t.background))
            .track_focus(&self.focus)
            .key_context(crate::keymap::WEB_PANE)
            .on_action(cx.listener(|w, _: &crate::keymap::Copy, _, _| w.edit(Edit::Copy)))
            .on_action(cx.listener(|w, _: &crate::keymap::Cut, _, _| w.edit(Edit::Cut)))
            .on_action(cx.listener(|w, _: &crate::keymap::Paste, _, _| w.edit(Edit::Paste)))
            .on_action(cx.listener(|w, _: &crate::keymap::SelectAll, _, _| w.edit(Edit::SelectAll)))
            .child(notice)
            .children(snapshot);
        #[cfg(feature = "web")]
        let base = {
            let entity = cx.entity();
            base.child(
                gpui::canvas(
                    |bounds, _, _| bounds,
                    move |_, bounds, window, cx| {
                        entity.update(cx, |w, cx| w.painted(bounds, window, cx));
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
        };
        base
    }
}

/// At most `max` characters of `s`, with an ellipsis when cut.
fn shorten(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Place, decide};

    #[test]
    fn a_page_shows_only_where_its_pane_is_drawn_and_uncovered() {
        // On another tab or zoomed away: never on screen, no snapshot.
        for live in [false, true] {
            assert_eq!(decide(Place::Hidden, live, false, false), (false, false));
        }
        // Drawn and uncovered: on screen (again).
        assert_eq!(decide(Place::Shown, false, true, true), (true, false));
    }

    #[test]
    fn a_covered_page_stays_until_its_snapshot_is_taken() {
        // The overlay opens: snapshot first, the page still live.
        assert_eq!(decide(Place::Covered, true, false, false), (true, true));
        // Asked once, waiting.
        assert_eq!(decide(Place::Covered, true, true, false), (true, false));
        // Taken (or given up on): off the screen, the overlay shows.
        assert_eq!(decide(Place::Covered, true, true, true), (false, false));
        // A page that was not on screen does not come back for an overlay.
        assert_eq!(decide(Place::Covered, false, false, false), (false, false));
    }
}

#[cfg(feature = "web")]
mod engine {
    //! Chromium for the whole process: started with the first web pane,
    //! its work pumped from a GPUI foreground task, shut down at quit.

    use std::cell::RefCell;
    use std::time::Duration;

    use gpui::WeakEntity;

    use super::WebPane;

    thread_local! {
        /// Every web pane that opened a page, to close them all at quit.
        static PANES: RefCell<Vec<WeakEntity<WebPane>>> = const { RefCell::new(Vec::new()) };
    }

    /// The longest CEF waits between pumps while it runs (cefclient's
    /// external pump uses the same bound).
    const MAX_PUMP_DELAY: Duration = Duration::from_millis(33);

    pub(super) fn opened(pane: WeakEntity<WebPane>) {
        PANES.with(|p| p.borrow_mut().push(pane));
    }

    /// Start Chromium in this process unless it runs.
    pub(super) fn ensure(cx: &mut gpui::App) -> Result<(), String> {
        if web_engine::started() {
            return Ok(());
        }
        let profile = workspace_config::desktop_state::state_dir()
            .ok_or("no state directory for the web profile")?
            .join("web");
        let (tx, rx) = async_channel::unbounded::<i64>();
        web_engine::start(&profile, move |delay| {
            let _ = tx.try_send(delay);
        })?;
        // CEF asks for its work (with a delay) from any thread; it runs
        // here, on the main thread, between GPUI's own events.
        cx.spawn(async move |cx| {
            let ex = cx.background_executor().clone();
            loop {
                let timer = ex.timer(MAX_PUMP_DELAY);
                let asked = futures_lite::future::or(async { rx.recv().await.ok() }, async {
                    timer.await;
                    None
                })
                .await;
                if let Some(d) = asked.filter(|d| *d > 0) {
                    ex.timer(Duration::from_millis(d as u64).min(MAX_PUMP_DELAY))
                        .await;
                }
                cx.update(|_| web_engine::do_work());
                if !web_engine::started() {
                    return;
                }
            }
        })
        .detach();
        cx.on_app_quit(|cx| {
            for pane in PANES.with(|p| std::mem::take(&mut *p.borrow_mut())) {
                let _ = pane.update(cx, |w, _| w.view = None);
            }
            if let Err(e) = web_engine::shutdown(Duration::from_secs(2)) {
                eprintln!("dreamer: {e}");
            }
            async {}
        })
        .detach();
        Ok(())
    }
}
