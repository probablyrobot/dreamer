//! Dreamer desktop client.
//!
//! ```text
//! dreamer [--smoke] [--fixture FILE]
//! dreamer [--smoke [--expect TEXT]] --attach %PANE [-L socket | --tmux-socket-path P] [--tmux PATH]
//! ```
//!
//! With no arguments (as when launched from Finder or a desktop launcher) it
//! follows the active pane of the default tmux server. `--fixture` paints a
//! VT fixture; `--smoke` alone paints the built-in renderer demo. With `--attach` it shows a live tmux pane: a
//! background owner thread follows the pane through `dreamerd` (started on
//! demand) and publishes frames only when the pane changed; the window
//! repaints only when woken by a new frame.
//!
//! `--smoke` exits once a frame has been presented, printing what was drawn.
//! `--smoke-repaint` then paints the same frame again and checks that every
//! row came from the shaping cache. Exit status 5 means the display did not
//! refresh at all (a locked screen), so paint checks proved nothing.
//! In attach mode it waits for a frame containing `--expect` and then
//! checks that a settled idle pane causes no view wakeups and at most one
//! stray paint in 4.5 s (see `idle_check`).

mod contrib;
mod fallback;
mod find;
mod fonts;
mod input;
mod keymap;
mod live;
mod palette;
mod panel;
mod prefix;
mod theme;
mod theme_pick;
mod tmux_keys;
mod web;

use theme::{c, ca};
mod bench_gui;
mod capture;
mod clip;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, DispatchPhase, ElementInputHandler, EntityInputHandler, FocusHandle,
    KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent,
    UTF16Selection, Window, WindowBounds, WindowOptions, canvas, div, font, point, prelude::*, px,
    size,
};
use replica::feed::{Feed, SelectEvent};
use terminal_ghostty::core::{FrameSnapshot, GridSize};
use terminal_ghostty::{Engine, EngineConfig};
use terminal_render::{TerminalElement, cell_metrics, last_paint_stats, paint_count};

/// The desktop identity: the Wayland app id and X11 window class, which
/// is also the name of the desktop file and the icon on Linux.
pub(crate) const APP_ID: &str = "dev.dreamer.Dreamer";

/// Give up if no frame is presented within this time in smoke mode.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Exit status when the display does not refresh at all (for example a
/// locked screen), so paint-based checks prove nothing either way.
pub(crate) const INCONCLUSIVE: i32 = 5;
/// Idle time a live smoke run watches for unwanted wakeups.
const IDLE_CHECK: Duration = Duration::from_millis(4500);
/// Quiet time with no new frame that ends the settle wait.
const IDLE_SETTLE: Duration = Duration::from_millis(500);
/// Longest settle wait; past it the idle windows start anyway.
const IDLE_SETTLE_MAX: Duration = Duration::from_millis(3000);
/// Paints during the idle check not caused by the pane (see `idle_check`).
const IDLE_STRAY_PAINTS: u64 = 1;
const GRID: GridSize = GridSize { cols: 80, rows: 16 };

/// A stream exercising styles, colors, wide and combining text, emoji, box
/// drawing, and a bar cursor.
const DEMO: &[u8] = "\x1b[1mdreamer\x1b[0m renderer proof  \x1b[2mM0-08\x1b[0m\r\n\
\x1b[31mred \x1b[32mgreen \x1b[33myellow \x1b[34mblue \x1b[35mmagenta \x1b[36mcyan\x1b[0m \
\x1b[38;2;255;140;0mtruecolor\x1b[0m \x1b[48;5;24m bg256 \x1b[0m \x1b[7minverse\x1b[0m\r\n\
\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[4:3mcurly\x1b[0m \
\x1b[9mstrike\x1b[0m \x1b[2mfaint\x1b[0m\r\n\
wide: 世界 こんにちは 한국어   combining: e\u{301} n\u{303} a\u{308}\r\n\
emoji: 👍 👍🏽 👨\u{200d}👩\u{200d}👧 🚀 ✅   box: ┌──┬──┐ │▓▒░│ └──┴──┘\r\n\
grid check: |0123456789|ABCDEFGHIJ|世界世界世|\r\n\
\r\n\x1b[6 qcursor (bar) here:"
    .as_bytes();

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Smoke {
    Off,
    /// Exit after the first presented frame.
    First,
    /// Also paint the same frame again and check the shaping cache.
    Repaint,
    /// Live: wait for a frame containing this text, then check idleness.
    Expect(String),
    /// Live: wait for this text, then type and compose through the window
    /// and check what reaches the pane.
    Input(String),
}

/// The terminal font for this run, set once before the window opens.
#[derive(Debug, Clone)]
pub(crate) struct TermFont {
    family: String,
    size: f32,
    line_height: f32,
    letter_spacing: f32,
    source: String,
}

impl TermFont {
    fn spacing(&self) -> terminal_render::Spacing {
        terminal_render::Spacing {
            line_height: self.line_height,
            letter_spacing: self.letter_spacing,
        }
    }
}

/// Clipboard writes the smoke received: recorded, never put on the
/// system clipboard.
static SMOKE_CLIPBOARD: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());
/// Links the input smoke "opened": recorded instead of launching anything.
static SMOKE_OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn term_font() -> TermFont {
    live::with(|l| l.font.clone()).unwrap_or_else(|| {
        let d = workspace_config::Settings::default();
        TermFont {
            family: d.font.family,
            size: d.font.size,
            line_height: d.font.line_height,
            letter_spacing: d.font.letter_spacing,
            source: "defaults".into(),
        }
    })
}

pub(crate) struct Shell {
    /// One pane among others in a workspace: no outer padding, and the
    /// workspace owns the window title and focus reports.
    embedded: bool,
    /// The pane's title and working directory, as the daemon reports them.
    meta: replica::PaneMeta,
    smoke: Smoke,
    frame_seen: bool,
    pub(crate) frame: Arc<FrameSnapshot>,
    /// Live frames applied to the view.
    live_frames: u64,
    pub(crate) focus: FocusHandle,
    /// Where key presses and text go (live panes only).
    pub(crate) feed: Option<Arc<Feed>>,
    /// IME composition in progress: drawn at the cursor, never sent.
    preedit: Option<String>,
    /// Window activation becomes focus in or out for the pane.
    _activation: Option<gpui::Subscription>,
    /// Mouse state, and where the grid was last painted.
    pointer: input::Pointer,
    /// A left-button selection gesture is in progress.
    selecting: bool,
    /// Last pointer position over the window, and the hyperlink under it
    /// while the open modifier is held.
    pointer_at: Option<gpui::Point<Pixels>>,
    link: Option<replica::feed::Link>,
    /// A short message shown over the grid (a refused link).
    notice: Option<String>,
    /// The window title last set, and when the last bell flashed.
    title: String,
    flash: Option<std::time::Instant>,
    pub(crate) grid: Option<input::Grid>,
    /// Progress of the input smoke after typing.
    input_phase: u8,
    /// Filters' marks on this pane's lines, and whether they are shown
    /// (switched per pane from the palette). Drawn over the grid only.
    marks: Vec<api::Decoration>,
    marks_shown: bool,
    /// Marks drawn in the last render (smoke evidence).
    pub(crate) marks_drawn: usize,
    /// Find in this pane, while its bar is open; and the search waiting
    /// to run or running.
    pub(crate) find: Option<find::Find>,
    find_task: Option<gpui::Task<()>>,
    find_busy: bool,
    /// tmux's prefix was pressed; the next key is the layer's.
    prefixed: bool,
    /// Dreamer's copy mode on this pane, and the history rows its text
    /// was read at.
    copy: Option<replica::copy_mode::CopyMode>,
    copy_rows: u64,
    /// The buffer chooser (prefix `=`).
    chooser: Option<prefix::Chooser>,
}

impl Shell {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        smoke: Smoke,
        frame: Arc<FrameSnapshot>,
        feed: Option<Arc<Feed>>,
        embedded: bool,
    ) -> Shell {
        Shell {
            embedded,
            meta: replica::PaneMeta::default(),
            smoke,
            frame_seen: false,
            frame,
            live_frames: 0,
            focus: cx.focus_handle(),
            feed,
            preedit: None,
            _activation: None,
            pointer: input::Pointer::default(),
            selecting: false,
            pointer_at: None,
            link: None,
            notice: None,
            title: String::new(),
            flash: None,
            grid: None,
            input_phase: 0,
            marks: Vec::new(),
            marks_shown: true,
            marks_drawn: 0,
            find: None,
            find_task: None,
            find_busy: false,
            prefixed: false,
            copy: None,
            copy_rows: 0,
            chooser: None,
        }
    }

    /// Open the find bar (or keep it), with the query as it was.
    pub(crate) fn open_find(&mut self, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.find = Some(find::Find::default());
        }
        cx.notify();
    }

    /// Close the find bar: matches go, and any search stops.
    pub(crate) fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find = None;
        self.find_task = None;
        self.find_busy = false;
        if let Some(feed) = &self.feed {
            feed.cancel_search();
        }
        cx.notify();
    }

    /// Search again after a short pause; `reveal` brings the current match
    /// into view when it changes (typing does, new output does not).
    pub(crate) fn search_soon(&mut self, reveal: bool, cx: &mut Context<Self>) {
        let (Some(f), Some(feed)) = (self.find.as_ref(), self.feed.clone()) else {
            return;
        };
        if f.query.is_empty() {
            self.find_task = None;
            feed.cancel_search();
            return;
        }
        let query = f.query();
        self.find_busy = true;
        self.find_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(find::SETTLE).await;
            let result = feed.search(query).await;
            let _ = this.update(cx, |s, cx| {
                s.find_busy = false;
                let view = s.frame.view;
                let Some(f) = s.find.as_mut() else { return };
                match result {
                    Ok(None) => {}
                    Err(e) => f.error = Some(e),
                    Ok(Some(found)) => {
                        let bottom = (view.offset + view.len).saturating_sub(1);
                        if let Some(hit) = f.take(found, bottom)
                            && reveal
                            && let Some(feed) = &s.feed
                        {
                            feed.view(replica::feed::ViewCommand::Reveal(hit.row));
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    /// A new frame: when the history changed under an open find bar,
    /// search again.
    fn frame_changed(&mut self, cx: &mut Context<Self>) {
        let rows = self.frame.view.total_rows;
        if self.copy.is_some() && rows != self.copy_rows {
            self.copy_rows = rows;
            self.copy_text(cx);
        }
        if let Some(f) = &self.find
            && f.found.is_some()
            && f.searched_rows != Some(rows)
            && !self.find_busy
        {
            self.search_soon(false, cx);
        }
    }

    /// The open find bar's matches in view, as boxes over their cells (the
    /// current one stronger), and the bar itself: top right, over the grid.
    fn find_ui(
        &self,
        window: &mut Window,
        t: &theme::Theme,
        term: &TermFont,
    ) -> (Vec<gpui::Div>, Option<gpui::Div>) {
        let Some(f) = &self.find else {
            return (Vec::new(), None);
        };
        let m = cell_metrics(
            window,
            &font(term.family.clone()),
            px(term.size),
            term.spacing(),
        );
        let view = self.frame.view;
        let current = f.current();
        let boxes = f
            .found
            .iter()
            .flat_map(|found| found.hits.iter())
            .filter(|h| h.row >= view.offset && h.row < view.offset + view.len)
            .map(|h| {
                let y = (h.row - view.offset) as f32;
                let mark = div()
                    .absolute()
                    .left(m.width * f32::from(h.start_col))
                    .top(m.height * y)
                    .w(m.width * f32::from(h.end_col.saturating_sub(h.start_col).max(1)))
                    .h(m.height)
                    .rounded_sm();
                if Some(*h) == current {
                    mark.bg(ca((t.accent << 8) | 0x66))
                        .border_1()
                        .border_color(c(t.accent))
                } else {
                    mark.bg(ca(t.mark))
                }
            })
            .collect();
        let status = f.status();
        let bar = div()
            .absolute()
            .top(px(6.0))
            .right(px(10.0))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py(px(3.0))
            .rounded_md()
            .shadow_md()
            .bg(c(t.raised))
            .border_1()
            .border_color(c(t.focus))
            .font_family(term.family.clone())
            .text_size(px(term.size * 0.9))
            .text_color(c(t.text))
            .child(
                div()
                    .text_color(c(t.accent))
                    .child(if f.regex { ".*" } else { "⌕" }),
            )
            .child(if f.query.is_empty() {
                div().text_color(c(t.faint)).child("Find")
            } else {
                div().child(format!("{}▏", f.query))
            })
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(if f.error.is_some() {
                            c(t.danger)
                        } else {
                            c(t.muted)
                        })
                        .child(status),
                )
            });
        (boxes, Some(bar))
    }

    /// Keys while the find bar is open; text arrives through the text
    /// input. Nothing reaches the program.
    fn find_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some(f) = self.find.as_mut() else { return };
        let mut step = None;
        match k.key.as_str() {
            "escape" => return self.close_find(cx),
            "enter" => step = Some(!k.modifiers.shift),
            "up" => step = Some(true),
            "down" => step = Some(false),
            "backspace" => {
                f.query.pop();
                f.edited();
                self.search_soon(true, cx);
            }
            "r" if k.modifiers.alt => {
                f.regex = !f.regex;
                f.edited();
                self.search_soon(true, cx);
            }
            _ => {}
        }
        if let Some(older) = step
            && let Some(hit) = self.find.as_mut().and_then(|f| f.step(older))
            && let Some(feed) = &self.feed
        {
            feed.view(replica::feed::ViewCommand::Reveal(hit.row));
        }
        cx.notify();
    }

    /// A pane's current marks, and whether to draw them.
    pub(crate) fn set_marks(
        &mut self,
        marks: Vec<api::Decoration>,
        shown: bool,
        cx: &mut Context<Self>,
    ) {
        if marks != self.marks || shown != self.marks_shown {
            self.marks = marks;
            self.marks_shown = shown;
            cx.notify();
        }
    }

    /// The marks on rows in the viewport, by viewport row. A mark names its
    /// line by row from the top of history; it is drawn only while that row
    /// still holds the marked text at the same width, so a view that has
    /// not caught up (or has moved on) draws nothing rather than the wrong
    /// line.
    fn visible_marks(&self) -> Vec<(usize, api::Decoration)> {
        let f = &self.frame;
        if !self.marks_shown || f.view.alt_screen {
            return Vec::new();
        }
        self.marks
            .iter()
            .filter(|d| d.cols == u32::from(f.size.cols))
            .filter_map(|d| {
                let i = usize::try_from(d.row.checked_sub(f.view.offset)?).ok()?;
                let row = f.rows.get(i)?;
                (row.text().trim_end() == d.text.trim_end()).then(|| (i, d.clone()))
            })
            .collect()
    }

    /// Apply what a pane's feed delivered (workspace panes; the single-pane
    /// window has its own loop with the smoke checks).
    pub(crate) fn take_feed(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(feed) = self.feed.clone() else {
            return false;
        };
        let Some(frame) = feed.take() else {
            return false;
        };
        self.frame = frame;
        self.live_frames += 1;
        self.frame_changed(cx);
        self.meta = feed.meta();
        if feed.take_bells() > 0 {
            self.bell(cx);
        }
        if let Some(text) = feed.take_clipboard() {
            self.clipboard_write(text, window, cx);
        }
        cx.notify();
        true
    }

    /// The window title this pane asks for: its title, else its working
    /// directory, else the app name.
    pub(crate) fn wanted_title(&self) -> String {
        self.meta
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.meta.cwd.clone())
            .unwrap_or_else(|| "Dreamer".into())
    }

    /// Copy the selection to the clipboard. The text is asked of the owner
    /// thread, never of the daemon.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            if let Some(text) = feed.selection_text().await {
                to_tmux_buffer(&text);
                cx.update(|cx| clip::write(cx, text));
            }
        })
        .detach();
    }

    /// A mouse selection was finished: like marking text in tmux, it
    /// becomes tmux's newest buffer, and with `copy_on_select` it goes to
    /// the clipboard too (and on Linux, to the primary selection).
    fn selection_done(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            let Some(text) = feed.selection_text().await.filter(|t| !t.is_empty()) else {
                return;
            };
            to_tmux_buffer(&text);
            cx.update(|cx| {
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                clip::write_primary(cx, text.clone());
                if live::with(|l| l.copy_on_select).unwrap_or(false) {
                    clip::write(cx, text);
                }
            });
        })
        .detach();
    }

    /// The middle button pastes tmux's newest buffer, as in tmux.
    fn paste_tmux_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let Some(text) = (match TMUX_BUFFERS.get() {
                Some(b) => b.newest().await,
                None => None,
            }) else {
                return;
            };
            feed.send(api::terminal_input::Event::Paste(api::PasteInput {
                data: text.into_bytes(),
                ..api::PasteInput::default()
            }));
        })
        .detach();
    }

    /// Paste the clipboard's text into the pane.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        if let Some(text) = clip::read(cx) {
            feed.send(api::terminal_input::Event::Paste(api::PasteInput {
                data: text.into_bytes(),
                ..api::PasteInput::default()
            }));
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        if self.find.is_some() {
            // Text typed into the bar comes through the text input; every
            // other key the bar uses or drops, so the program never sees it.
            let printable = ev.keystroke.key_char.is_some()
                && !(ev.keystroke.modifiers.control
                    || ev.keystroke.modifiers.platform
                    || ev.keystroke.modifiers.alt);
            if !printable {
                self.find_key(ev, cx);
                if !(ev.keystroke.modifiers.platform || input::is_palette_shortcut(&ev.keystroke)) {
                    cx.stop_propagation();
                }
            }
            return;
        }
        if self.tmux_key(ev, cx) {
            cx.stop_propagation();
            return;
        }
        let view = self.frame.view;
        let page = self.frame.size.rows.saturating_sub(1).max(1) as isize;
        if !view.alt_screen
            && let Some(to) = input::scroll_key(&ev.keystroke, page)
        {
            feed.view(replica::feed::ViewCommand::Scroll(to));
            cx.stop_propagation();
            return;
        }
        match input::intent(&ev.keystroke, ev.is_held) {
            input::KeyIntent::Copy => {
                self.copy(cx);
                cx.stop_propagation();
            }
            input::KeyIntent::Paste => {
                self.paste(cx);
                cx.stop_propagation();
            }
            input::KeyIntent::Send(event) => {
                feed.send(event);
                cx.stop_propagation();
            }
            input::KeyIntent::Pass => {}
        }
    }

    /// The prefix layer, copy mode, and the buffer chooser. True when the
    /// key was theirs (it never reaches the program).
    fn tmux_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(key) = tmux_keys::from_keystroke(&ev.keystroke) else {
            return false;
        };
        if let Some(ch) = self.chooser.as_mut() {
            match ch.key(&key) {
                prefix::Choice::Stay => {}
                prefix::Choice::Close => self.chooser = None,
                prefix::Choice::Paste(name) => {
                    self.chooser = None;
                    self.paste_buffer(Some(name), cx);
                }
            }
            cx.notify();
            return true;
        }
        if let Some(copy) = self.copy.as_mut() {
            use replica::copy_mode::Outcome;
            match copy.key(key) {
                Outcome::Stay => self.show_copy(),
                Outcome::Copy(text) => {
                    self.copied(text, cx);
                    self.leave_copy();
                }
                Outcome::Leave => self.leave_copy(),
            }
            cx.notify();
            return true;
        }
        if self.prefixed {
            self.prefixed = false;
            self.after_prefix(key, cx);
            prefix::refresh(cx);
            cx.notify();
            return true;
        }
        if TMUX_BUFFERS.get().is_some()
            && self.feed.as_ref().and_then(|f| f.pane()).is_some()
            && prefix::is_prefix(&key)
        {
            self.prefixed = true;
            cx.notify();
            return true;
        }
        false
    }

    /// The key after the prefix.
    fn after_prefix(&mut self, key: replica::keys::Key, cx: &mut Context<Self>) {
        match key.text() {
            Some('[') => return self.enter_copy(cx),
            Some(']') => return self.paste_buffer(None, cx),
            Some('=') => return self.open_chooser(cx),
            _ => {}
        }
        let (Some(b), Some(pane)) = (
            TMUX_BUFFERS.get(),
            self.feed.as_ref().and_then(|f| f.pane()),
        ) else {
            return;
        };
        let name = key.tmux_name();
        cx.spawn(async move |this, cx| {
            let r = b.prefix_key(pane, name).await;
            let _ = this.update(cx, |s, cx| match r {
                // tmux's copy mode (prefix PageUp, say) cannot be shown
                // here; Dreamer's takes its place.
                Ok(r) if r.mode_cancelled.as_deref() == Some("copy-mode") => s.enter_copy(cx),
                Ok(r) => {
                    if let Some(mode) = r.mode_cancelled {
                        s.show_notice(
                            format!(
                                "tmux opened {mode}, which Dreamer does not draw; it was closed"
                            ),
                            cx,
                        );
                    }
                }
                Err(e) => s.show_notice(format!("tmux key not sent: {e}"), cx),
            });
        })
        .detach();
    }

    /// A short message at the bottom of the pane for four seconds.
    fn show_notice(&mut self, text: String, cx: &mut Context<Self>) {
        self.notice = Some(text.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |s, cx| {
                if s.notice.as_deref() == Some(text.as_str()) {
                    s.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Enter copy mode on the history as the replica has it now, at the
    /// view's place, the cursor where the terminal's is (or at the bottom
    /// left of the view when that is out of sight).
    fn enter_copy(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Some(text) = feed.text().await else {
                return;
            };
            let _ = this.update(cx, |s, cx| {
                let f = &s.frame;
                let v = f.view;
                let cursor = if f.cursor.visible {
                    replica::copy_mode::Pos {
                        row: (v.offset + u64::from(f.cursor.y)) as usize,
                        col: usize::from(f.cursor.x),
                    }
                } else {
                    replica::copy_mode::Pos {
                        row: (v.offset + v.len.saturating_sub(1)) as usize,
                        col: 0,
                    }
                };
                s.copy_rows = v.total_rows;
                s.copy = Some(replica::copy_mode::CopyMode::new(
                    prefix::table(),
                    &text,
                    v.total_rows as usize,
                    v.len as usize,
                    usize::from(f.size.cols),
                    v.offset as usize,
                    cursor,
                ));
                s.show_copy();
                cx.notify();
            });
        })
        .detach();
    }

    /// Read the history's text again for copy mode (more output arrived).
    fn copy_text(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Some(text) = feed.text().await else {
                return;
            };
            let _ = this.update(cx, |s, cx| {
                let rows = s.frame.view.total_rows as usize;
                if let Some(c) = s.copy.as_mut() {
                    c.set_text(&text, rows);
                    s.show_copy();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Scroll the view to copy mode's rows.
    fn show_copy(&self) {
        if let (Some(c), Some(feed)) = (&self.copy, &self.feed)
            && c.top() as u64 != self.frame.view.offset
        {
            feed.view(replica::feed::ViewCommand::Top(c.top() as u64));
        }
    }

    fn leave_copy(&mut self) {
        self.copy = None;
        if let Some(feed) = &self.feed {
            feed.view(replica::feed::ViewCommand::Scroll(
                replica::feed::Scroll::Bottom,
            ));
        }
    }

    /// Copy mode's copy: as in tmux, a new paste buffer; and the system
    /// clipboard when `clipboard.tmux` brings tmux's copies there.
    fn copied(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(b) = TMUX_BUFFERS.get() {
            b.write(text.clone());
        }
        let (from, write) = live::with(|l| (l.clipboard_tmux.from_tmux(), l.clipboard))
            .unwrap_or((true, workspace_config::ClipboardWrite::Focused));
        if from && write != workspace_config::ClipboardWrite::Deny {
            clip::write(cx, text);
        }
    }

    fn open_chooser(&mut self, cx: &mut Context<Self>) {
        let Some(b) = TMUX_BUFFERS.get() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let buffers = b.list().await;
            let _ = this.update(cx, |s, cx| {
                if buffers.is_empty() {
                    s.show_notice("no tmux buffers".into(), cx);
                } else {
                    s.chooser = Some(prefix::Chooser { buffers, at: 0 });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Paste a tmux buffer (`name`, or the newest) as a paste: bracketed
    /// when the program asked for it.
    fn paste_buffer(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        let (Some(feed), Some(b)) = (self.feed.clone(), TMUX_BUFFERS.get()) else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Some(text) = b.named(name).await {
                feed.send(api::terminal_input::Event::Paste(api::PasteInput {
                    data: text.into_bytes(),
                    ..api::PasteInput::default()
                }));
            }
        })
        .detach();
    }

    /// Copy mode drawn over the grid: the selection in `mode-style`'s
    /// background (over the text, translucent), the cursor, the position
    /// indicator at the top right, and a search prompt or message at the
    /// bottom. The buffer chooser, when open, is a list over the pane.
    fn copy_ui(&self, window: &mut Window, t: &theme::Theme, term: &TermFont) -> Vec<gpui::Div> {
        let mut out = Vec::new();
        let m = cell_metrics(
            window,
            &font(term.family.clone()),
            px(term.size),
            term.spacing(),
        );
        let rgb = |c: terminal_ghostty::core::Rgb| {
            (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
        };
        let (fg, bg) = prefix::mode_colors(&self.frame.colors);
        let bg = bg.map_or(t.accent, rgb);
        let fg = fg.map_or(t.background, rgb);
        let label = |text: String| {
            div()
                .px_1()
                .bg(c(bg))
                .text_color(c(fg))
                .font_family(term.family.clone())
                .text_size(px(term.size * 0.85))
                .child(text)
        };
        if self.prefixed {
            out.push(
                div()
                    .absolute()
                    .bottom_0()
                    .right_0()
                    .child(label("prefix".into())),
            );
        }
        if let Some(cm) = &self.copy {
            let top = self.frame.view.offset as usize;
            let rows = self.frame.view.len as usize;
            let in_view = |row: usize| row >= top && row < top + rows;
            for sp in cm.visible_spans().into_iter().filter(|s| in_view(s.row)) {
                out.push(
                    div()
                        .absolute()
                        .left(m.width * sp.start as f32)
                        .top(m.height * (sp.row - top) as f32)
                        .w(m.width * (sp.end - sp.start) as f32)
                        .h(m.height)
                        .bg(ca((bg << 8) | 0x88)),
                );
            }
            let cur = cm.cursor();
            if in_view(cur.row) {
                out.push(
                    div()
                        .absolute()
                        .left(m.width * cur.col as f32)
                        .top(m.height * (cur.row - top) as f32)
                        .w(m.width)
                        .h(m.height)
                        .border_1()
                        .border_color(c(bg))
                        .bg(ca((bg << 8) | 0x55)),
                );
            }
            out.push(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .child(label(cm.indicator())),
            );
            if let Some(text) = cm.prompt().or_else(|| cm.message().map(str::to_owned)) {
                out.push(div().absolute().bottom_0().left_0().child(label(text)));
            }
        }
        if let Some(ch) = &self.chooser {
            let rows = ch.buffers.iter().enumerate().map(|(i, b)| {
                div()
                    .flex()
                    .gap_2()
                    .px_2()
                    .when(i == ch.at, |d| d.bg(c(t.selected)))
                    .child(
                        div()
                            .text_color(c(t.muted))
                            .child(format!("{}  {} B", b.name, b.size)),
                    )
                    .child(div().text_color(c(t.text)).child(b.sample.clone()))
            });
            out.push(
                div()
                    .absolute()
                    .top(px(6.0))
                    .left(px(10.0))
                    .right(px(10.0))
                    .flex()
                    .flex_col()
                    .py_1()
                    .rounded_md()
                    .shadow_md()
                    .bg(c(t.raised))
                    .border_1()
                    .border_color(c(t.focus))
                    .font_family(term.family.clone())
                    .text_size(px(term.size * 0.9))
                    .overflow_hidden()
                    .children(rows),
            );
        }
        out
    }

    /// Look up the link under `at` (when the open modifier is held) and
    /// show it; clear it otherwise.
    fn hover(
        &mut self,
        grid: &input::Grid,
        at: gpui::Point<Pixels>,
        mods: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.pointer_at = Some(at);
        let Some(feed) = self.feed.clone().filter(|_| input::open_modifier(mods)) else {
            if self.link.take().is_some() {
                cx.notify();
            }
            return;
        };
        let (col, row) = grid.cell_at(at);
        let (col, row) = (col as u16, row as u16);
        let covered = self.link.as_ref().is_some_and(|l| {
            l.cells
                .iter()
                .any(|c| c.row == row && (c.start..=c.end).contains(&col))
        });
        if covered {
            return;
        }
        cx.spawn(async move |this, cx| {
            let link = feed.link_at(col, row).await;
            let _ = this.update(cx, |s, cx| {
                if s.link != link {
                    s.link = link;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Open the link under a modifier-click, if there is one and its scheme
    /// is allowed. Never triggered by terminal output.
    fn open_link_at(&mut self, cell: (u32, u32), cx: &mut Context<Self>) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        let smoke = self.smoke != Smoke::Off;
        cx.spawn(async move |this, cx| {
            let Some(link) = feed.link_at(cell.0 as u16, cell.1 as u16).await else {
                return;
            };
            let allowed = live::with(|l| l.links.clone())
                .unwrap_or_else(|| workspace_config::Settings::default().links)
                .allows(&link.uri);
            if !allowed {
                let why = format!("not opened: {} (scheme not in links.schemes)", link.uri);
                let _ = this.update(cx, |s, cx| {
                    s.notice = Some(why);
                    cx.notify();
                });
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let _ = this.update(cx, |s, cx| {
                    s.notice = None;
                    cx.notify();
                });
                return;
            }
            if smoke {
                if let Ok(mut o) = SMOKE_OPENED.lock() {
                    o.push(link.uri);
                }
            } else {
                cx.update(|cx| cx.open_url(&link.uri));
            }
        })
        .detach();
    }

    /// A program's clipboard write, addressed to this window by the daemon
    /// (it is the pane's focused client), applied under `clipboard.write`.
    fn clipboard_write(&mut self, text: Vec<u8>, window: &Window, cx: &mut Context<Self>) {
        use workspace_config::ClipboardWrite;
        let allowed = match live::with(|l| l.clipboard).unwrap_or_default() {
            ClipboardWrite::Deny => false,
            ClipboardWrite::Allow => true,
            ClipboardWrite::Focused => {
                window.is_window_active() || self.smoke != Smoke::Off || clip::in_memory()
            }
        };
        if !allowed {
            return;
        }
        if self.smoke != Smoke::Off {
            if let Ok(mut c) = SMOKE_CLIPBOARD.lock() {
                c.push(text);
            }
            return;
        }
        clip::write(cx, String::from_utf8_lossy(&text).into_owned());
    }

    /// The window title follows the pane: its title, else its working
    /// directory, else the app name.
    fn show_meta(&mut self, meta: &replica::PaneMeta, window: &mut Window) {
        self.meta = meta.clone();
        if self.embedded {
            return;
        }
        let title = self.wanted_title();
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }

    /// A visual bell: a brief flash, at most one per `BELL_FLASH` so a
    /// stream of bells does not strobe.
    fn bell(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        if self
            .flash
            .is_some_and(|t| now.duration_since(t) < BELL_FLASH * 2)
        {
            return;
        }
        self.flash = Some(now);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn select(&self, event: SelectEvent) {
        if let Some(feed) = &self.feed {
            feed.view(replica::feed::ViewCommand::Select(event));
        }
    }

    fn send_all(&self, events: impl IntoIterator<Item = api::terminal_input::Event>) {
        if let Some(feed) = &self.feed {
            for e in events {
                feed.send(e);
            }
        }
    }

    /// The wheel goes to applications that track the mouse, becomes arrow
    /// keys on an alternate screen without tracking, and otherwise scrolls
    /// the local history with no daemon round trip.
    fn wheel(&mut self, grid: &input::Grid, ev: &ScrollEvent) {
        let view = self.frame.view;
        if view.mouse_tracking {
            let out = self
                .pointer
                .scroll(grid, ev.position, ev.delta, &ev.modifiers);
            self.send_all(out);
            return;
        }
        let steps = self.pointer.steps(grid, ev.delta);
        if steps == 0 {
            return;
        }
        if view.alt_screen {
            self.send_all(input::wheel_arrows(steps));
        } else if let Some(feed) = &self.feed {
            feed.view(replica::feed::ViewCommand::Scroll(
                replica::feed::Scroll::Delta(-(steps as isize)),
            ));
        }
    }

    /// Mouse listeners for the grid painted at `grid`. Presses and wheel
    /// steps count only inside the grid; a drag keeps reporting (clamped)
    /// outside it until the button is released.
    fn listen_mouse(
        entity: gpui::Entity<Shell>,
        grid: input::Grid,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        let e = entity.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && bounds.contains(&ev.position) {
                e.update(cx, |s, cx| {
                    // The open modifier with the left button is only ever a
                    // link click: never a selection or an application click.
                    if ev.button == gpui::MouseButton::Left && input::open_modifier(&ev.modifiers) {
                        s.open_link_at(grid.cell_at(ev.position), cx);
                        return;
                    }
                    // A right click opens the workspace's context menu
                    // unless the application tracks the mouse; with Shift
                    // it does even then. Either way it is not reported.
                    if ev.button == gpui::MouseButton::Right
                        && s.embedded
                        && (!s.frame.view.mouse_tracking || ev.modifiers.shift)
                    {
                        return;
                    }
                    // Selection unless the application tracks the mouse;
                    // Shift selects even then.
                    let select = ev.button == gpui::MouseButton::Left
                        && (!s.frame.view.mouse_tracking || ev.modifiers.shift);
                    let paste = ev.button == gpui::MouseButton::Middle
                        && (!s.frame.view.mouse_tracking || ev.modifiers.shift)
                        && live::with(|l| l.middle_click)
                            == Some(workspace_config::MiddleClick::Paste);
                    if paste {
                        s.paste_tmux_buffer(cx);
                    } else if select {
                        s.selecting = true;
                        s.select(SelectEvent::Press {
                            at: grid.cell_pos(ev.position),
                            time_ns: monotonic_ns(),
                        });
                    } else {
                        let out = s.pointer.down(&grid, ev.button, ev.position, &ev.modifiers);
                        s.send_all(out);
                    }
                });
            }
        });
        let e = entity.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                e.update(cx, |s, cx| {
                    if s.selecting && ev.button == gpui::MouseButton::Left {
                        s.selecting = false;
                        s.select(SelectEvent::Release {
                            at: grid.cell_pos(ev.position),
                        });
                        s.selection_done(cx);
                    } else if ev.button == gpui::MouseButton::Middle {
                        // A middle click that pasted sends no release either.
                    } else {
                        let out = s.pointer.up(&grid, ev.button, ev.position, &ev.modifiers);
                        s.send_all(out);
                    }
                });
            }
        });
        let e = entity.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble
                && (ev.pressed_button.is_some() || bounds.contains(&ev.position))
            {
                e.update(cx, |s, cx| {
                    s.hover(&grid, ev.position, &ev.modifiers, cx);
                    if s.selecting {
                        s.select(SelectEvent::Drag {
                            at: grid.cell_pos(ev.position),
                        });
                    } else {
                        let out = s.pointer.moved(&grid, ev.position, &ev.modifiers);
                        s.send_all(out);
                    }
                });
            }
        });
        window.on_mouse_event(move |ev: &ScrollEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && bounds.contains(&ev.position) {
                entity.update(cx, |s, _| s.wheel(&grid, ev));
            }
        });
    }

    /// The terminal cursor's cell, relative to the terminal's origin.
    fn cursor_cell(&self, window: &mut Window) -> Bounds<Pixels> {
        let tf = term_font();
        let m = cell_metrics(window, &font(tf.family.clone()), px(tf.size), tf.spacing());
        let c = self.frame.cursor;
        Bounds::new(
            point(m.width * f32::from(c.x), m.height * f32::from(c.y)),
            size(m.width, m.height),
        )
    }
}

/// Text input from the platform, including IME. The terminal has no
/// editable text of its own: committed text is sent to the pane and
/// composition is only shown.
impl EntityInputHandler for Shell {
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
        // Keys of the prefix layer, copy mode, and the chooser are never
        // typed into the program.
        if self.prefixed || self.copy.is_some() || self.chooser.is_some() {
            return;
        }
        if let Some(f) = self.find.as_mut() {
            if !text.is_empty() {
                f.query.push_str(text);
                f.edited();
                self.search_soon(true, cx);
            }
            cx.notify();
            return;
        }
        if !text.is_empty()
            && let Some(feed) = &self.feed
        {
            feed.send(api::terminal_input::Event::Text(api::TextInput {
                text: text.to_owned(),
            }));
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
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The IME candidate window opens at the terminal cursor.
        let cell = self.cursor_cell(window);
        Some(Bounds::new(element_bounds.origin + cell.origin, cell.size))
    }
}

impl Shell {
    fn report_frame(window: &mut Window) {
        let tf = term_font();
        let terminal_font = font(tf.family.clone());
        let metrics = cell_metrics(window, &terminal_font, px(tf.size), tf.spacing());
        println!(
            "smoke: font {} {}pt x{} ({})",
            tf.family, tf.size, tf.line_height, tf.source
        );
        let size = window.viewport_size();
        println!(
            "smoke: frame presented ({}x{} logical px, scale {})",
            f32::from(size.width),
            f32::from(size.height),
            window.scale_factor()
        );
        println!(
            "smoke: cell {}x{} px, baseline {} px",
            f32::from(metrics.width),
            f32::from(metrics.height),
            f32::from(metrics.ascent)
        );
        println!("smoke: skin {}", theme::for_window(window).name);
        println!("smoke: painted {:?}", last_paint_stats());
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(self.smoke, Smoke::First | Smoke::Repaint) && !self.frame_seen {
            self.frame_seen = true;
            let this = cx.entity();
            let repaint = self.smoke == Smoke::Repaint;
            let handle = window.window_handle();
            window.on_next_frame(move |window, cx| {
                Shell::report_frame(window);
                if !repaint {
                    cx.quit();
                    return;
                }
                let first = last_paint_stats();
                // Paint the same frame again: every row must come from the
                // shaping cache.
                let before = paint_count();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(300)).await;
                    this.update(cx, |_, cx| cx.notify());
                    let _ = handle.update(cx, |_, window, _| window.refresh());
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    let painted = paint_count() - before;
                    let again = last_paint_stats();
                    let code = if painted == 0 {
                        println!("smoke: the display did not refresh (screen locked or asleep?); repaint checks not run");
                        INCONCLUSIVE
                    } else {
                        println!("smoke: repainted {again:?}");
                        // Nothing reshaped, and no image converted or uploaded again.
                        let cached = again.shaped_lines == 0
                            && again.reused_lines == first.shaped_lines
                            && again.image_uploads == 0
                            && again.images == first.images;
                        if cached { 0 } else { 3 }
                    };
                    cx.update(|cx| cx.quit());
                    std::process::exit(code);
                })
                .detach();
            });
            cx.notify();
        }
        let t = theme::for_window(window);
        let term = term_font();
        let entity = cx.entity();
        let grid_size = self.frame.size;
        let focus = self.focus.clone();
        let preedit = self.preedit.clone().map(|text| {
            let cell = self.cursor_cell(window);
            div()
                .absolute()
                .left(cell.origin.x)
                .top(cell.origin.y)
                .bg(c(t.selected))
                .text_color(c(t.text))
                .font_family(term.family.clone())
                .text_size(px(term.size))
                .underline()
                .child(text)
        });
        // The hovered link: underlined, with a pointing hand.
        let underline: Vec<_> = self
            .link
            .iter()
            .flat_map(|l| l.cells.iter())
            .map(|c| {
                let tf = term_font();
                let m = cell_metrics(window, &font(tf.family.clone()), px(tf.size), tf.spacing());
                div()
                    .absolute()
                    .left(m.width * f32::from(c.start))
                    .top(m.height * f32::from(c.row + 1) - px(2.0))
                    .w(m.width * f32::from(c.end - c.start + 1))
                    .h(px(1.0))
                    .bg(theme::c(t.accent))
            })
            .collect();
        let notice = self.notice.clone().map(|text| {
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .px_2()
                .bg(c(t.danger_surface))
                .text_color(c(t.danger_text))
                .text_size(px(term.size * 0.85))
                .child(text)
        });
        // A bell flashes the grid briefly.
        let flash = self
            .flash
            .filter(|t| t.elapsed() < BELL_FLASH)
            .map(|_| div().absolute().size_full().bg(ca(t.flash)));
        // Shown only while scrolled back into history.
        let back = self.frame.view.rows_back();
        let scrolled = (back > 0).then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .px_2()
                .bg(c(t.selected))
                .text_color(c(t.muted))
                .font_family(term.family.clone())
                .text_size(px(term.size * 0.85))
                .child(format!("↑ {back} lines"))
        });
        let (found, find_bar) = self.find_ui(window, &t, &term);
        let copy_ui = self.copy_ui(window, &t, &term);
        let marks = self.visible_marks();
        self.marks_drawn = marks.len();
        let marks: Vec<_> = if marks.is_empty() {
            Vec::new()
        } else {
            let m = cell_metrics(
                window,
                &font(term.family.clone()),
                px(term.size),
                term.spacing(),
            );
            let bg = self.frame.colors.background;
            // The terminal's own background, partly or almost opaque.
            let veil = |alpha: u32| {
                ca((u32::from(bg.r) << 24)
                    | (u32::from(bg.g) << 16)
                    | (u32::from(bg.b) << 8)
                    | alpha)
            };
            marks
                .into_iter()
                .map(|(i, d)| {
                    let line = div()
                        .absolute()
                        .left_0()
                        .top(m.height * i as f32)
                        .w_full()
                        .h(m.height);
                    match api::DecorationStyle::try_from(d.style) {
                        Ok(api::DecorationStyle::Highlight) => line.bg(ca(t.mark)),
                        Ok(api::DecorationStyle::Dim) => line.bg(veil(0x99)),
                        // Folded away: covered, with a marker; the row stays
                        // so nothing below moves.
                        Ok(api::DecorationStyle::Fold) => line
                            .bg(veil(0xf0))
                            .text_color(c(t.faint))
                            .font_family(term.family.clone())
                            .text_size(px(term.size))
                            .child("⋯"),
                        _ => line.flex().justify_end().child(
                            div()
                                .px_1()
                                .h(m.height)
                                .bg(c(t.accent))
                                .text_color(c(t.background))
                                .font_family(term.family.clone())
                                .text_size(px(term.size * 0.85))
                                .child(d.badge.clone().unwrap_or_else(|| d.filter.clone())),
                        ),
                    }
                })
                .collect()
        };
        div()
            .size_full()
            .bg(c(t.background))
            .when(!self.embedded, |d| d.p_2())
            .track_focus(&self.focus)
            .on_action(cx.listener(|s, _: &keymap::Copy, _, cx| s.copy(cx)))
            .on_action(cx.listener(|s, _: &keymap::Paste, _, cx| s.paste(cx)))
            .on_action(cx.listener(|s, _: &keymap::Find, _, cx| s.open_find(cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_modifiers_changed(cx.listener(|s, ev: &gpui::ModifiersChangedEvent, _, cx| {
                if let (Some(grid), Some(at)) = (s.grid, s.pointer_at) {
                    s.hover(&grid, at, &ev.modifiers, cx);
                }
            }))
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(TerminalElement {
                        frame: Arc::clone(&self.frame),
                        font: font(term.family.clone()),
                        font_size: px(term.size),
                        spacing: term.spacing(),
                    })
                    .children(marks)
                    .children(found)
                    .children(find_bar)
                    .children(preedit)
                    .children(scrolled)
                    .children(underline)
                    .children(notice)
                    .children(copy_ui)
                    .children(flash)
                    .when(self.link.is_some(), |d| d.cursor_pointer())
                    // Registers the platform text input (IME) each frame.
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, entity.clone()),
                                    cx,
                                );
                                let tf = term_font();
                                let m = cell_metrics(
                                    window,
                                    &font(tf.family.clone()),
                                    px(tf.size),
                                    tf.spacing(),
                                );
                                let grid = input::Grid {
                                    origin: bounds.origin,
                                    cell: size(m.width, m.height),
                                    cols: grid_size.cols,
                                    rows: grid_size.rows,
                                };
                                entity.update(cx, |s, _| s.grid = Some(grid));
                                Shell::listen_mouse(entity.clone(), grid, bounds, window);
                            },
                        )
                        // Over the grid, not after it in the flow.
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    ),
            )
    }
}

/// How long a visual bell shows.
const BELL_FLASH: Duration = Duration::from_millis(120);

/// Monotonic nanoseconds for counting repeat clicks.
fn monotonic_ns() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

/// The display backend GPUI will use, by its own selection rule: Wayland
/// when `WAYLAND_DISPLAY` is set, else X11 when `DISPLAY` is set, else a
/// headless platform that draws nothing.
fn display_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        return "macOS (Metal)";
    }
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set("WAYLAND_DISPLAY") {
        "Wayland"
    } else if set("DISPLAY") {
        "X11"
    } else {
        "headless"
    }
}

/// A gradient image placed through the Kitty graphics protocol, like any
/// program would: 12x4 cells at row 7, column 66.
fn demo_image() -> Vec<u8> {
    let (w, h) = (96u32, 48u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 200, 255]);
        }
    }
    let mut out = b"\x1b7\x1b[8;67H".to_vec();
    out.extend_from_slice(format!("\x1b_Ga=T,f=32,s={w},v={h},c=12,r=4,C=1,i=1,q=2;").as_bytes());
    out.extend_from_slice(base64(&rgba).as_bytes());
    out.extend_from_slice(b"\x1b\\\x1b8");
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                T[(n >> (18 - 6 * i)) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    out
}

fn load_frame(bytes: &[u8]) -> Result<Arc<FrameSnapshot>, String> {
    let mut config = EngineConfig::new(GRID);
    config.capture_replies = false;
    let mut engine = Engine::new(config).map_err(|e| e.to_string())?;
    // Canonical cell pixels, as on mirrors and replicas: image placements
    // are sized in these units.
    engine
        .resize(GRID, terminal_ghostty::core::family::CANONICAL_CELL_PX)
        .map_err(|e| e.to_string())?;
    engine.feed(bytes).map_err(|e| e.to_string())?;
    engine.frame().map_err(|e| e.to_string())
}

#[derive(Debug, Default)]
struct Args {
    smoke: bool,
    repaint: bool,
    input: bool,
    expect: Option<String>,
    fixture: Option<String>,
    attach: Option<u32>,
    /// A workspace window (tabs and every pane of the active tmux window)
    /// instead of a single pane.
    workspace: bool,
    /// The workspace smoke's second run: check the restored arrangement.
    restore: bool,
    /// The clipboard smoke: copy and paste in every kind of pane.
    clipboard: bool,
    /// The web pane smoke, and where its captures go.
    web: Option<std::path::PathBuf>,
    /// Parts of the workspace to open at start (`sidebar`, `palette`).
    open: Vec<String>,
    /// `dreamerd` options for a daemon started on demand.
    daemon: Vec<String>,
    /// Show this remote host's tmux, through the local daemon.
    host: Option<String>,
    /// The remote smoke (with `host`).
    remote: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--smoke" => a.smoke = true,
            "--smoke-input" => {
                a.smoke = true;
                a.input = true;
            }
            "--smoke-repaint" => {
                a.smoke = true;
                a.repaint = true;
            }
            "--workspace" => a.workspace = true,
            // Reference scenes (spec 14.4): open with the sidebar or the
            // command palette showing.
            "--open-sidebar" => a.open.push("sidebar".into()),
            "--open-palette" => a.open.push("palette".into()),
            "--smoke-workspace" => {
                a.smoke = true;
                a.workspace = true;
            }
            "--smoke-clipboard" => {
                a.smoke = true;
                a.workspace = true;
                a.clipboard = true;
            }
            "--smoke-web" => {
                a.smoke = true;
                a.workspace = true;
                a.web = Some(value("--smoke-web")?.into());
            }
            "--smoke-workspace-restore" => {
                a.smoke = true;
                a.workspace = true;
                a.restore = true;
            }
            "--host" => {
                a.host = Some(value("--host")?);
                a.workspace = true;
            }
            "--smoke-remote" => {
                a.smoke = true;
                a.remote = true;
                a.workspace = true;
            }
            "--expect" => a.expect = Some(value("--expect")?),
            "--fixture" => a.fixture = Some(value("--fixture")?),
            "--attach" => {
                let p = value("--attach")?;
                a.attach = Some(
                    p.trim_start_matches('%')
                        .parse()
                        .map_err(|_| format!("`{p}` is not a pane id"))?,
                );
            }
            "-L" | "--tmux-socket" => {
                a.daemon.push("--tmux-socket".into());
                a.daemon.push(value("-L")?);
            }
            "--tmux-socket-path" | "--tmux" => {
                a.daemon.push(arg.clone());
                a.daemon.push(value(arg)?);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(a)
}

/// tmux's paste buffers for this process's local daemon, once a window
/// has started following them.
pub(crate) static TMUX_BUFFERS: std::sync::OnceLock<replica::clipboard::TmuxBuffers> =
    std::sync::OnceLock::new();

/// Send `text` to tmux as its newest paste buffer, if the settings say
/// Dreamer's copies go there.
pub(crate) fn to_tmux_buffer(text: &str) {
    if live::with(|l| l.clipboard_tmux.to_tmux()).unwrap_or(false)
        && let Some(b) = TMUX_BUFFERS.get()
    {
        b.write(text.to_owned());
    }
}

/// When the process started (the GUI benchmark's launch time).
pub(crate) static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() -> ExitCode {
    STARTED.get_or_init(std::time::Instant::now);
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dreamer: {e}");
            return ExitCode::from(2);
        }
    };
    let frame = match &args.fixture {
        Some(path) => std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|b| load_frame(&b)),
        None if args.attach.is_some() || !args.smoke => {
            load_frame(b"\x1b[2mconnecting to dreamerd...\x1b[0m")
        }
        None => load_frame(&[DEMO, &demo_image()].concat()),
    };
    let frame = match frame {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend = display_backend();
    if args.smoke {
        // No smoke run reads or replaces what the user copied.
        clip::use_memory();
        // Nor saves its windows anywhere but a private directory.
        workspace::SMOKE_RUN.store(true, std::sync::atomic::Ordering::Relaxed);
        println!("smoke: backend {backend}");
        if backend == "headless" {
            // A headless platform would "present" frames without drawing.
            eprintln!("smoke: no display server in the environment; nothing would be drawn");
            return ExitCode::from(4);
        }
    }
    let smoke = match (args.smoke, args.attach, &args.expect) {
        (false, _, _) => Smoke::Off,
        (true, Some(_), Some(text)) if args.input => Smoke::Input(text.clone()),
        (true, Some(_), Some(text)) => Smoke::Expect(text.clone()),
        (true, Some(_), None) => Smoke::Expect(String::new()),
        (true, None, _) if args.repaint => Smoke::Repaint,
        (true, None, _) => Smoke::First,
    };
    // Launched with nothing to show (Finder, a desktop launcher): the
    // workspace of the default tmux server's first session.
    let workspace_mode =
        args.workspace || (args.attach.is_none() && !args.smoke && args.fixture.is_none());
    let follow = match (args.attach, workspace_mode) {
        (Some(p), false) => Some(Some(p)),
        _ => None,
    };
    let feed = match follow {
        Some(pane) => {
            let launch = client::Launch::new(args.daemon.clone());
            match Feed::start(launch.socket.clone(), pane, Some(launch)) {
                Ok(f) => Some(Arc::new(f)),
                Err(e) => {
                    eprintln!("error: cannot start the pane feed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    // A local workspace window: connect to the daemon (starting it if
    // need be) and ask for the topology now, while GPUI starts up.
    if workspace_mode && args.host.is_none() {
        let launch = client::Launch::new(args.daemon.clone());
        if let Ok(t) = replica::topology::TopologyFeed::start(launch.socket.clone(), Some(launch))
            && let Ok(mut early) = workspace::EARLY_TOPOLOGY.lock()
        {
            *early = Some(Arc::new(t));
        }
    }

    let settings = match workspace_config::load_settings() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dreamer: {e}; using default settings");
            workspace_config::Settings::default()
        }
    };

    fonts::prefetch(&settings.font.family);
    let app = gpui_platform::application();
    // macOS: clicking the Dock icon with no window open opens one.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            keymap::new_window(cx);
        }
    });
    let window_args = args.daemon.clone();
    app.run(move |cx: &mut App| {
        keymap::install(cx, move |cx| {
            let launch = client::Launch::new(window_args.clone());
            // A new window is a new session, as a terminal's is a new shell.
            if let Err(e) = workspace::open(cx, launch, None, Vec::new(), workspace::Start::NewSession)
            {
                eprintln!("dreamer: {e}");
            }
        });
        // The settings in force, then kept live: a valid change to the
        // configuration applies at once, an invalid one is shown and waits.
        // Every theme file is read off the launch path: only the palette's
        // Theme command lists them.
        live::apply(cx, &settings, Vec::new());
        cx.background_spawn(async { live::set_themes(live::themes()) })
            .detach();
        live::watch(cx);

        if workspace_mode {
            let launch = client::Launch::new(args.daemon.clone());
            if let Some(host) = &args.host {
                let via = replica::Via::Host(host.clone());
                // Connect the host unless it is live (a profile, or a host
                // the daemon knows); the window shows it once it is.
                replica::hosts::connect_once(launch.clone(), host.clone());
                match workspace::open_via(
                    cx,
                    launch,
                    via,
                    None,
                    args.open.clone(),
                    workspace::Start::Saved(None),
                ) {
                    Ok(w) if args.remote => workspace::smoke_remote(cx, w),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
                cx.activate(true);
                return;
            }
            if args.remote {
                eprintln!("error: --smoke-remote needs --host NAME");
                std::process::exit(2);
            }
            let run = match (args.restore, args.clipboard, &args.web) {
                (_, _, Some(out)) => workspace::SmokeRun::Web(out.clone()),
                (true, _, _) => workspace::SmokeRun::Restore,
                (false, true, _) => workspace::SmokeRun::Clipboard,
                (false, false, _) => workspace::SmokeRun::Workspace,
            };
            let smoke = args.smoke.then(|| (args.daemon.clone(), run));
            if let Err(e) = workspace::open_saved(cx, launch, smoke, args.open.clone()) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            cx.activate(true);
            return;
        }
        let bounds = Bounds::centered(None, size(px(900.), px(420.)), cx);
        let smoke_mode = smoke.clone();
        let view_feed = feed.clone();
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some(crate::APP_ID.into()),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(|cx| Shell::new(cx, smoke_mode, frame, view_feed, false));
                let focus = shell.read(cx).focus.clone();
                window.focus(&focus, cx);
                shell.update(cx, |s, cx| {
                    // Sent only if the application asked for focus reports.
                    s._activation = Some(cx.observe_window_activation(window, |s, window, _| {
                        if let Some(feed) = &s.feed {
                            feed.send(api::terminal_input::Event::Focus(api::FocusInput {
                                gained: window.is_window_active(),
                            }));
                        }
                    }));
                });
                shell
            },
        );
        let window = match opened {
            Ok(w) => w,
            Err(e) => {
                eprintln!("error: could not open a window: {e:#}");
                std::process::exit(1);
            }
        };
        if let Some(feed) = feed {
            // Wake only when the feed has a newer frame; never poll.
            cx.spawn(async move |cx| {
                while feed.wake.recv().await.is_ok() {
                    let Some(frame) = feed.take() else { continue };
                    let text = frame.text_dump();
                    let meta = feed.meta();
                    let bells = feed.take_bells();
                    let clipboard = feed.take_clipboard();
                    let done = window.update(cx, |shell, window, cx| {
                        shell.frame = frame;
                        shell.live_frames += 1;
                        shell.frame_changed(cx);
                        shell.show_meta(&meta, window);
                        if bells > 0 {
                            shell.bell(cx);
                        }
                        if let Some(text) = clipboard {
                            shell.clipboard_write(text, window, cx);
                        }
                        cx.notify();
                        match &shell.smoke {
                            Smoke::Expect(want) | Smoke::Input(want)
                                if !shell.frame_seen && text.contains(want.as_str()) =>
                            {
                                shell.frame_seen = true;
                                window.refresh();
                                Some(matches!(shell.smoke, Smoke::Input(_)))
                            }
                            Smoke::Input(_) if shell.frame_seen => {
                                let line = |want: &str| text.lines().any(|l| l.trim_end() == want);
                                match shell.input_phase {
                                    // Output of the typed command: exactly one line.
                                    0 if line("hi日本") => {
                                        println!(
                                            "smoke: typed and composed input reached the pane: hi日本"
                                        );
                                        shell.input_phase = 1;
                                        shell.send_all([api::terminal_input::Event::Text(
                                            api::TextInput { text: MOUSE_PROBE.into() },
                                        )]);
                                    }
                                    1 if line("MOUSE-READY") => {
                                        shell.input_phase = 2;
                                        if let Some(grid) = shell.grid {
                                            let any = window.window_handle();
                                            cx.spawn(async move |_, cx| click_and_scroll(any, grid, cx).await)
                                                .detach();
                                        }
                                    }
                                    2 if line(MOUSE_EXPECTED) => {
                                        println!(
                                            "smoke: click, release, and wheel reached the pane as {MOUSE_EXPECTED}"
                                        );
                                        shell.input_phase = 3;
                                        shell.send_all([api::terminal_input::Event::Text(
                                            api::TextInput {
                                                text: "seq 1 200; echo SCROLL-''READY\r".into(),
                                            },
                                        )]);
                                    }
                                    3 if line("SCROLL-READY") && shell.frame.view.rows_back() == 0 => {
                                        shell.input_phase = 4;
                                        if let Some(grid) = shell.grid {
                                            let any = window.window_handle();
                                            cx.spawn(async move |_, cx| wheel_history(any, grid, cx).await)
                                                .detach();
                                        }
                                    }
                                    4 if shell.frame.view.rows_back() == WHEEL_LINES as u64 => {
                                        println!(
                                            "smoke: the wheel scrolled {WHEEL_LINES} lines of local history"
                                        );
                                        shell.input_phase = 5;
                                        // Typing returns the view to the bottom first.
                                        shell.send_all([api::terminal_input::Event::Text(
                                            api::TextInput {
                                                text: "clear; echo SEL''ECT-ME\r".into(),
                                            },
                                        )]);
                                    }
                                    5 if shell.frame.view.rows_back() == 0 => {
                                        let row = shell
                                            .frame
                                            .rows
                                            .iter()
                                            .rposition(|r| r.text().trim_end() == SELECT_TEXT);
                                        if let (Some(row), Some(grid)) = (row, shell.grid) {
                                            shell.input_phase = 6;
                                            let any = window.window_handle();
                                            cx.spawn(async move |_, cx| {
                                                drag_select(any, grid, row as f32, cx).await
                                            })
                                            .detach();
                                        }
                                    }
                                    // The release's frame: the whole word is highlighted.
                                    6 if !shell.selecting
                                        && shell.frame.selection.iter().any(|c| {
                                            c.start == 0 && usize::from(c.end) + 1 == SELECT_TEXT.len()
                                        }) =>
                                    {
                                        shell.input_phase = 7;
                                        let selection = shell.frame.selection.clone();
                                        if let Some(feed) = shell.feed.clone() {
                                            cx.spawn(async move |this, cx| {
                                                let text = feed.selection_text().await;
                                                if text.as_deref() == Some(SELECT_TEXT) {
                                                    println!(
                                                        "smoke: a drag selected {SELECT_TEXT:?} locally ({selection:?})"
                                                    );
                                                    let _ = this.update(cx, |s, _| {
                                                        s.input_phase = 8;
                                                        s.send_all([api::terminal_input::Event::Text(
                                                            api::TextInput { text: LINK_PROBE.into() },
                                                        )]);
                                                    });
                                                    return;
                                                }
                                                eprintln!("smoke: the drag selected {text:?}, not {SELECT_TEXT:?}");
                                                std::process::exit(3);
                                            })
                                            .detach();
                                        }
                                    }
                                    10 if line("TITLED") && shell.title == SMOKE_TITLE && shell.flash.is_some() => {
                                        println!(
                                            "smoke: the window title follows the pane ({SMOKE_TITLE:?}) and a bell flashed"
                                        );
                                        shell.input_phase = 11;
                                        // The focus event window activation sends (a locked
                                        // screen never activates the window), then a write.
                                        shell.send_all([
                                            api::terminal_input::Event::Focus(api::FocusInput {
                                                gained: true,
                                            }),
                                            api::terminal_input::Event::Text(api::TextInput {
                                                text: CLIPBOARD_PROBE.into(),
                                            }),
                                        ]);
                                    }
                                    11 if line("CLIPPED") => {
                                        let got = SMOKE_CLIPBOARD.lock().map(|c| c.clone()).unwrap_or_default();
                                        if got == [SMOKE_CLIPBOARD_TEXT.as_bytes()] {
                                            println!(
                                                "smoke: a program's clipboard write reached this window once ({SMOKE_CLIPBOARD_TEXT:?})"
                                            );
                                            std::process::exit(0);
                                        }
                                        eprintln!("smoke: clipboard writes received: {got:?}");
                                        std::process::exit(3);
                                    }
                                    8 => {
                                        let row = shell
                                            .frame
                                            .rows
                                            .iter()
                                            .position(|r| r.text().trim_end() == LINK_LINE);
                                        if let (Some(row), Some(grid)) = (row, shell.grid) {
                                            shell.input_phase = 9;
                                            let any = window.window_handle();
                                            cx.spawn(async move |_, cx| {
                                                link_check(any, grid, row as f32, cx).await
                                            })
                                            .detach();
                                        }
                                    }
                                    _ => {}
                                }
                                None
                            }
                            _ => None,
                        }
                    });
                    match done.ok().flatten() {
                        Some(false) => {
                            let feed = feed.clone();
                            cx.spawn(async move |cx| idle_check(feed, cx).await)
                                .detach();
                        }
                        Some(true) => {
                            cx.spawn(async move |cx| type_into(window, cx).await)
                                .detach();
                        }
                        None => {}
                    }
                }
            })
            .detach();
        }
        if smoke != Smoke::Off {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(SMOKE_TIMEOUT).await;
                eprintln!("smoke: no matching frame presented within {SMOKE_TIMEOUT:?}");
                std::process::exit(2);
            })
            .detach();
        }
        cx.activate(true);
    });
    ExitCode::SUCCESS
}

/// Type through the window as a person would: plain keys go through the
/// platform text input, then an IME composition ("日" composed, "日本"
/// committed), then Enter as a key event. Only committed text may reach
/// the pane, so the shell must print exactly `hi日本`.
async fn type_into(window: gpui::WindowHandle<Shell>, cx: &mut gpui::AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    // Dispatch without holding the shell view: its own key and text input
    // handlers must be able to run.
    let any: gpui::AnyWindowHandle = window.into();
    let press = |cx: &mut gpui::AsyncApp, k: &'static str| {
        let _ = cx.update_window(any, move |_, window, cx| {
            if let Ok(ks) = gpui::Keystroke::parse(k) {
                window.dispatch_keystroke(ks, cx);
            }
        });
    };
    for k in ["e", "c", "h", "o", "space", "h", "i"] {
        press(cx, k);
    }
    let _ = window.update(cx, |shell, window, cx| {
        shell.replace_and_mark_text_in_range(None, "日", None, window, cx);
        shell.replace_text_in_range(None, "日本", window, cx);
    });
    press(cx, "enter");
    fail_unless_past(
        any,
        0,
        cx,
        "the typed input never produced `hi日本` in the pane",
    )
    .await;
}

/// Turns on SGR mouse reporting, reads one press, release, and wheel step,
/// and prints them with ESC shown as `E`.
const MOUSE_PROBE: &str = "printf '\\033[?1000h\\033[?1006h'; stty raw -echo; printf 'MOUSE-''READY\\r\\n'; head -c 28 | tr '\\033' E; printf '\\r\\n'; stty sane; printf '\\033[?1000l\\033[?1006l'\r";
/// Left press and release at cell (3, 1), then one wheel step down there.
const MOUSE_EXPECTED: &str = "E[<0;4;2ME[<0;4;2mE[<65;4;2M";

/// Click and scroll through the window as a person would, at cell (3, 1).
async fn click_and_scroll(any: gpui::AnyWindowHandle, grid: input::Grid, cx: &mut gpui::AsyncApp) {
    use gpui::{Modifiers, MouseButton, PlatformInput, ScrollDelta, TouchPhase};
    let at = point(
        grid.origin.x + grid.cell.width * 3.5,
        grid.origin.y + grid.cell.height * 1.5,
    );
    let events = [
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
        PlatformInput::Scroll(ScrollEvent {
            position: at,
            delta: ScrollDelta::Lines(point(0.0, -1.0)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        }),
    ];
    for ev in events {
        let _ = cx.update_window(any, move |_, window, cx| {
            window.dispatch_event(ev, cx);
        });
    }
    fail_unless_past(
        any,
        2,
        cx,
        "the click and wheel never reached the pane as expected",
    )
    .await;
}

/// Writes "SMOKE-CLIP" to the clipboard with OSC 52, then prints a marker.
const CLIPBOARD_PROBE: &str = "printf '\\033]52;c;U01PS0UtQ0xJUA==\\007'; echo CLIP''PED\r";
const SMOKE_CLIPBOARD_TEXT: &str = "SMOKE-CLIP";

/// Sets the pane title and rings the bell, then prints a marker.
const TITLE_PROBE: &str = "printf '\\033]2;SMOKE-TITLE\\007\\007'; echo T''ITLED\r";
const SMOKE_TITLE: &str = "SMOKE-TITLE";

/// Clears the screen and prints an OSC 8 link followed by plain text.
const LINK_PROBE: &str = "clear; printf '\\033]8;;https://example.com/dreamer-smoke\\033\\\\LINK-HERE\\033]8;;\\033\\\\ plain\\n'\r";
const LINK_LINE: &str = "LINK-HERE plain";
const LINK_URI: &str = "https://example.com/dreamer-smoke";

/// A plain click on the link must not open it; hovering with the open
/// modifier must underline exactly its cells; a modifier-click opens it
/// (recorded, never launched, in smoke mode).
async fn link_check(
    any: gpui::AnyWindowHandle,
    grid: input::Grid,
    row: f32,
    cx: &mut gpui::AsyncApp,
) {
    use gpui::{Modifiers, MouseButton, PlatformInput};
    let at = point(
        grid.origin.x + grid.cell.width * 2.5,
        grid.origin.y + grid.cell.height * (row + 0.5),
    );
    let open = if cfg!(target_os = "macos") {
        Modifiers {
            platform: true,
            ..Modifiers::default()
        }
    } else {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    };
    let click = |modifiers: Modifiers| {
        [
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers,
                click_count: 1,
            }),
        ]
    };
    let dispatch = |cx: &mut gpui::AsyncApp, ev: PlatformInput| {
        let _ = cx.update_window(any, move |_, window, cx| {
            window.dispatch_event(ev, cx);
        });
    };
    let fail = |why: String| -> ! {
        eprintln!("smoke: {why}");
        std::process::exit(3)
    };
    for ev in click(Modifiers::default()) {
        dispatch(cx, ev);
    }
    dispatch(
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: None,
            modifiers: open,
        }),
    );
    // The hover lookup runs on the feed thread; wait for its result.
    let mut link = None;
    for _ in 0..100 {
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
        link = cx
            .update_window(any, |view, _, cx| {
                view.downcast::<Shell>()
                    .ok()
                    .and_then(|s| s.read(cx).link.clone())
            })
            .ok()
            .flatten();
        if link.is_some() {
            break;
        }
    }
    let Some(link) = link else {
        stalled(
            any,
            cx,
            "hovering the link with the open modifier found nothing",
        )
        .await
    };
    let want = [terminal_ghostty::core::SelectedCells {
        row: row as u16,
        start: 0,
        end: 8,
    }];
    if link.uri != LINK_URI || link.cells != want {
        fail(format!("the hovered link was {link:?}"));
    }
    if SMOKE_OPENED.lock().map(|o| o.len()).unwrap_or(1) != 0 {
        fail("a plain click opened the link".into());
    }
    for ev in click(open) {
        dispatch(cx, ev);
    }
    for _ in 0..100 {
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
        let opened = SMOKE_OPENED.lock().map(|o| o.clone()).unwrap_or_default();
        if !opened.is_empty() {
            if opened != [LINK_URI] {
                fail(format!("opened {opened:?}"));
            }
            println!(
                "smoke: the link opened only on modifier-click ({LINK_URI}, cells {:?})",
                link.cells
            );
            // Next: a title and a bell from the program.
            let _ = cx.update_window(any, |view, _, cx| {
                if let Ok(shell) = view.downcast::<Shell>() {
                    shell.update(cx, |s, _| {
                        s.input_phase = 10;
                        s.send_all([api::terminal_input::Event::Text(api::TextInput {
                            text: TITLE_PROBE.into(),
                        })]);
                    });
                }
            });
            fail_unless_past(any, 10, cx, "the title and bell never reached the window").await;
            return;
        }
    }
    stalled(any, cx, "the modifier-click never opened the link").await
}

/// The line the smoke selects by dragging across it.
const SELECT_TEXT: &str = "SELECT-ME";

/// Press at the left of the first cell of `row`, drag to the right half of
/// its ninth cell, and release: the drag must select exactly that word.
async fn drag_select(
    any: gpui::AnyWindowHandle,
    grid: input::Grid,
    row: f32,
    cx: &mut gpui::AsyncApp,
) {
    use gpui::{Modifiers, MouseButton, PlatformInput};
    let at = |col: f32| {
        point(
            grid.origin.x + grid.cell.width * col,
            grid.origin.y + grid.cell.height * (row + 0.5),
        )
    };
    let end = SELECT_TEXT.len() as f32 - 0.1;
    let events = [
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: at(0.1),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at(4.5),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at(end),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: at(end),
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    ];
    for ev in events {
        let _ = cx.update_window(any, move |_, window, cx| {
            window.dispatch_event(ev, cx);
        });
    }
    fail_unless_past(any, 6, cx, "the drag never selected anything").await;
}

/// Wheel lines the smoke scrolls through local history.
const WHEEL_LINES: usize = 5;

/// With no mouse tracking, the wheel scrolls the replica's history.
async fn wheel_history(any: gpui::AnyWindowHandle, grid: input::Grid, cx: &mut gpui::AsyncApp) {
    use gpui::{Modifiers, PlatformInput, ScrollDelta, TouchPhase};
    let at = point(
        grid.origin.x + grid.cell.width * 2.5,
        grid.origin.y + grid.cell.height * 2.5,
    );
    let ev = PlatformInput::Scroll(ScrollEvent {
        position: at,
        delta: ScrollDelta::Lines(point(0.0, WHEEL_LINES as f32)),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    let _ = cx.update_window(any, move |_, window, cx| {
        window.dispatch_event(ev, cx);
    });
    fail_unless_past(any, 4, cx, "the wheel never scrolled the local history").await;
}

/// A smoke wait ran out. If the window then does not paint when asked, it
/// was not being drawn (behind the frontmost app, or the display asleep)
/// and the wait proved nothing: exit `INCONCLUSIVE`. Otherwise the check
/// failed: exit 3.
pub(crate) async fn stalled(any: gpui::AnyWindowHandle, cx: &mut gpui::AsyncApp, why: &str) -> ! {
    let before = paint_count();
    let _ = cx.update_window(any, |_, window, _| window.refresh());
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    if paint_count() == before {
        println!(
            "smoke: {why}, but the window did not repaint when asked (behind another app, or the display asleep?); not run"
        );
        std::process::exit(INCONCLUSIVE);
    }
    eprintln!("smoke: {why}");
    std::process::exit(3)
}

/// Fail the input smoke if it is still at `phase` after ten seconds.
async fn fail_unless_past(
    any: gpui::AnyWindowHandle,
    phase: u8,
    cx: &mut gpui::AsyncApp,
    why: &str,
) {
    cx.background_executor()
        .timer(Duration::from_secs(10))
        .await;
    let stuck = cx
        .update_window(any, |view, _, cx| {
            view.downcast::<Shell>()
                .is_ok_and(|s| s.read(cx).input_phase <= phase)
        })
        .unwrap_or(true);
    if stuck {
        stalled(any, cx, why).await;
    }
}

/// After the expected frame: an idle pane must cause no view wakeups and
/// no paints.
///
/// First the pane settles: the check waits until no frame has arrived for
/// `IDLE_SETTLE` (at most `IDLE_SETTLE_MAX`), so a late resize after attach
/// is not counted. Then over `IDLE_CHECK` any view wakeup fails: the pane
/// wrote nothing, so the mirror must not change. GPUI repaints a background
/// window once now and then for activity elsewhere on the desktop (seen
/// with the window inactive and the pointer outside it), so
/// `IDLE_STRAY_PAINTS` paint is allowed; a repaint loop of any rate faster
/// than one per 2 s paints more and fails.
async fn idle_check(feed: Arc<Feed>, cx: &mut gpui::AsyncApp) {
    let wakeups = || feed.stats.view_wakeups.load(Ordering::Relaxed);
    let timer = |d| cx.background_executor().timer(d);
    // Let the presentation of the expected frame finish first.
    timer(Duration::from_millis(300)).await;
    cx.update(|cx| {
        if let Some(w) = cx.windows().first() {
            let _ = w.update(cx, |_, window, _| Shell::report_frame(window));
        }
    });
    println!(
        "smoke: live frames painted: {} paints so far",
        paint_count()
    );
    let settle_from = std::time::Instant::now();
    let mut seen = wakeups();
    loop {
        timer(IDLE_SETTLE).await;
        let now = wakeups();
        if now == seen || settle_from.elapsed() >= IDLE_SETTLE_MAX {
            break;
        }
        seen = now;
    }
    println!(
        "smoke: live settled in {} ms",
        settle_from.elapsed().as_millis()
    );
    let (wakes, paints) = (wakeups(), paint_count());
    timer(IDLE_CHECK).await;
    let (wakes, paints) = (wakeups() - wakes, paint_count() - paints);
    let window = cx
        .update(|cx| {
            cx.windows().first().and_then(|w| {
                w.update(cx, |_, window, _| {
                    let (m, b) = (window.mouse_position(), window.bounds().size);
                    let inside = m.x >= gpui::px(0.)
                        && m.y >= gpui::px(0.)
                        && m.x < b.width
                        && m.y < b.height;
                    format!(
                        "window {}, pointer {}",
                        if window.is_window_active() {
                            "active"
                        } else {
                            "inactive"
                        },
                        if inside { "inside" } else { "outside" }
                    )
                })
                .ok()
            })
        })
        .unwrap_or_default();
    println!(
        "smoke: live pane idle {:.1}s: {wakes} view wakeups, {paints} paints, at most {IDLE_STRAY_PAINTS} allowed ({window}; {} frames built, {} owner wakeups)",
        IDLE_CHECK.as_secs_f32(),
        feed.stats.frames.load(Ordering::Relaxed),
        feed.stats.owner_wakeups.load(Ordering::Relaxed)
    );
    let mut code = if wakes == 0 && paints <= IDLE_STRAY_PAINTS {
        0
    } else if wakes == 0 {
        // Repaints the pane never asked for: the OS redrew the window
        // (activation, the pointer, another app moving over it). That says
        // nothing about the pane, so the check could not run; a view
        // wakeup is what would mean the idle pane did work.
        println!(
            "smoke: {paints} repaints with no view wakeup came from outside the pane (desktop activity); idle check not run"
        );
        INCONCLUSIVE
    } else {
        3
    };
    if code == 0 {
        // Control: few paints prove nothing if the window was not being
        // drawn at all (occluded, or the display asleep).
        let before = paint_count();
        cx.update(|cx| {
            if let Some(w) = cx.windows().first() {
                let _ = w.update(cx, |_, window, _| window.refresh());
            }
        });
        timer(Duration::from_millis(500)).await;
        if paint_count() == before {
            println!(
                "smoke: the window did not repaint when asked (occluded, or the display asleep?); idle check not run"
            );
            code = INCONCLUSIVE;
        }
    }
    cx.update(|cx| cx.quit());
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn attach_arguments_pass_tmux_options_to_the_daemon() {
        let a = parse(&["--attach".into(), "%3".into(), "-L".into(), "x".into()]).expect("parse");
        assert_eq!(a.attach, Some(3));
        assert_eq!(a.daemon, ["--tmux-socket", "x"]);
        assert!(parse(&["--attach".into(), "nope".into()]).is_err());
    }
}
