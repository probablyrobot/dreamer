//! Native panels (spec 12.2.1): an app package drawn by the desktop from
//! the same model the pane app draws (`app-model`). The model runs on a
//! thread of its own, with its own connection to the daemon; the panel
//! only sends it keys and draws what it published. No package code runs
//! on the UI thread or in the paint loop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use app_model::manifest::{FieldKind, ViewKind, field};
use app_model::{App, Endpoint, Input, text};
use gpui::{
    Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::theme::{self, c};

/// Rows drawn at most; a longer source shows the rows around the
/// selection (a log, its newest rows).
const MAX_ROWS: usize = 200;

pub(crate) struct AppPanel {
    pub(crate) path: PathBuf,
    /// The model as last published, and why the app ended, if it did.
    latest: Arc<Mutex<Option<App>>>,
    ended: Arc<Mutex<Option<String>>>,
    inputs: tokio::sync::mpsc::UnboundedSender<Input>,
    pub(crate) focus: FocusHandle,
    /// The model drawn last (smoke evidence).
    pub(crate) shown: Option<App>,
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
            latest,
            ended,
            inputs,
            focus: cx.focus_handle(),
            shown: None,
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

/// The rows shown: up to `MAX_ROWS`, around the selection (or the newest,
/// for a log), with the index of the first.
fn window(app: &App) -> (usize, &[serde_json::Value]) {
    let n = app.rows.len();
    let anchor = if app.manifest.view.kind == ViewKind::Log {
        n
    } else {
        app.selected + MAX_ROWS / 2
    };
    let end = anchor.clamp(MAX_ROWS.min(n), n);
    let start = end.saturating_sub(MAX_ROWS);
    (start, &app.rows[start..end])
}

impl Render for AppPanel {
    fn render(&mut self, window_: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme::for_window(window_);
        let latest = self.latest.lock().ok().and_then(|l| l.clone());
        if latest.is_some() {
            self.shown = latest;
        }
        let ended = self.ended();
        let focused = self.focus.is_focused(window_);
        let root = div()
            .id("app-panel")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_click(cx.listener(|p, _, window, cx| {
                window.focus(&p.focus, cx);
            }))
            .w(px(360.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(c(t.surface))
            .border_l_1()
            .border_color(c(if focused { t.focus } else { t.divider }))
            .text_size(px(12.0))
            .text_color(c(t.text));
        let Some(app) = self.shown.as_ref() else {
            return root.child(
                div()
                    .p_2()
                    .text_color(c(t.muted))
                    .child(ended.unwrap_or_else(|| "starting…".into())),
            );
        };
        let title = if app.version > 1 {
            format!("{} (reloaded {})", app.manifest.title, app.version - 1)
        } else {
            app.manifest.title.clone()
        };
        let status = match &ended {
            Some(why) => format!("ended: {why}"),
            None => app.status_text(),
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
                .child(body(app, &t)),
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

/// The view itself, by kind.
fn body(app: &App, t: &theme::Theme) -> gpui::AnyElement {
    let v = &app.manifest.view;
    let cell = |s: String| {
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(SharedString::from(s))
    };
    let line = |selected: bool| {
        div()
            .flex()
            .flex_row()
            .gap_2()
            .px_1()
            .rounded_sm()
            .when(selected, |d| d.bg(c(t.selected)))
    };
    let value = |r: &serde_json::Value| match &v.field {
        Some(p) => field(r, p).map(text).unwrap_or_default(),
        None => text(r),
    };
    match v.kind {
        ViewKind::Agent => {
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
        ViewKind::Table => {
            let (start, rows) = window(app);
            div()
                .flex()
                .flex_col()
                .child(
                    line(false)
                        .text_color(c(t.muted))
                        .children(v.columns.iter().map(|h| cell(h.clone()))),
                )
                .children(rows.iter().enumerate().map(|(i, r)| {
                    line(start + i == app.selected).children(
                        v.columns
                            .iter()
                            .map(|col| cell(field(r, col).map(text).unwrap_or_default())),
                    )
                }))
                .into_any_element()
        }
        ViewKind::List | ViewKind::Log => {
            let (start, rows) = window(app);
            let log = v.kind == ViewKind::Log;
            div()
                .flex()
                .flex_col()
                .when(log, |d| d.h_full().justify_end())
                .children(
                    rows.iter().enumerate().map(|(i, r)| {
                        line(!log && start + i == app.selected).child(cell(value(r)))
                    }),
                )
                .into_any_element()
        }
        ViewKind::Text => div()
            .child(SharedString::from(
                app.rows.first().map(value).unwrap_or_default(),
            ))
            .into_any_element(),
        ViewKind::Gauge => {
            let row = app.rows.first();
            let n = row
                .and_then(|r| match &v.field {
                    Some(p) => field(r, p).and_then(serde_json::Value::as_f64),
                    None => r.as_f64(),
                })
                .unwrap_or(0.0);
            let ratio = if n > 1.0 { n / 100.0 } else { n }.clamp(0.0, 1.0) as f32;
            let label = row
                .and_then(|r| v.label.as_deref().and_then(|p| field(r, p)))
                .map(text)
                .unwrap_or_else(|| format!("{:.0}%", ratio * 100.0));
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .h(px(8.0))
                        .w_full()
                        .rounded_sm()
                        .bg(c(t.selected))
                        .child(div().h_full().w(gpui::relative(ratio)).bg(c(t.accent))),
                )
                .child(SharedString::from(label))
                .into_any_element()
        }
        ViewKind::Chart => {
            let values: Vec<f64> = app
                .rows
                .iter()
                .map(|r| {
                    match &v.field {
                        Some(p) => field(r, p).and_then(serde_json::Value::as_f64),
                        None => r.as_f64(),
                    }
                    .unwrap_or(0.0)
                    .max(0.0)
                })
                .collect();
            let top = values.iter().copied().fold(0.0, f64::max).max(1.0);
            div()
                .h(px(80.0))
                .flex()
                .flex_row()
                .items_end()
                .gap(px(1.0))
                .children(values.iter().rev().take(MAX_ROWS).rev().map(|n| {
                    div()
                        .flex_1()
                        .h(gpui::relative((n / top) as f32))
                        .bg(c(t.accent))
                }))
                .into_any_element()
        }
        ViewKind::Form => div()
            .flex()
            .flex_col()
            .gap_1()
            .children(
                v.fields
                    .iter()
                    .zip(&app.form)
                    .enumerate()
                    .map(|(i, (f, val))| {
                        let label = f.label.clone().unwrap_or_else(|| f.name.clone());
                        let shown = match f.kind {
                            FieldKind::Toggle if val == "true" => "[x]".to_owned(),
                            FieldKind::Toggle => "[ ]".to_owned(),
                            _ => val.clone(),
                        };
                        let star = if f.required { "*" } else { "" };
                        line(i == app.focus)
                            .child(div().text_color(c(t.muted)).child(format!("{label}{star}")))
                            .child(cell(shown))
                    }),
            )
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(s: &'static str) -> gpui::Keystroke {
        gpui::Keystroke::parse(s).expect("keystroke")
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
