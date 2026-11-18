//! Web panes: a tmux placeholder pane marked with a web page
//! (`Pane.web`), which the desktop fills with the page, drawn by Chromium
//! itself in a native view kept on the pane's rectangle.
//!
//! Chromium's view sits above everything GPUI draws. The workspace hides it
//! while the pane is not shown (another tab, or zoomed away) and while the
//! command palette is open, since the palette would be drawn under it.
//!
//! Chromium starts with the first web pane, not with the app. A build
//! without the `web` feature, or one not running from a bundle with the
//! framework and its helper apps, shows the page's address and why it
//! cannot draw it.

use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    WeakEntity, Window, div, px,
};

use crate::theme::c;
use crate::workspace::Workspace;

pub(crate) struct WebPane {
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) pane: u32,
    pub(crate) url: String,
    pub(crate) focus: FocusHandle,
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    workspace: WeakEntity<Workspace>,
    /// Nothing covers the pane: Chromium's view may show.
    shown: bool,
    /// Why the page is not drawn.
    pub(crate) problem: Option<String>,
    #[cfg(feature = "web")]
    pub(crate) view: Option<web_engine::WebView>,
}

impl WebPane {
    pub(crate) fn new(
        pane: u32,
        url: String,
        workspace: WeakEntity<Workspace>,
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
        WebPane {
            pane,
            url,
            focus: cx.focus_handle(),
            workspace,
            shown: true,
            problem,
            #[cfg(feature = "web")]
            view: None,
        }
    }

    /// Whether the page may be on screen: the pane is drawn and nothing
    /// the desktop draws covers it. Hiding takes effect at once; showing,
    /// when the pane is next painted at its place.
    pub(crate) fn set_shown(&mut self, shown: bool) {
        self.shown = shown;
        #[cfg(feature = "web")]
        if !shown && let Some(v) = &self.view {
            v.set_visible(false);
        }
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
            match web_engine::WebView::open(window, rect, &self.url, move || {
                let _ = tx.try_send(());
            }) {
                Ok(v) => {
                    engine::opened(cx.entity().downgrade());
                    // The pane was focused before its page existed.
                    if self.focus.is_focused(window) {
                        v.focus();
                    }
                    self.view = Some(v);
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
            v.set_visible(self.shown);
        }
    }
}

impl Render for WebPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::for_window(window);
        let _ = &cx;
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
        let base = div()
            .size_full()
            .relative()
            .bg(c(t.background))
            .track_focus(&self.focus)
            .child(notice);
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
