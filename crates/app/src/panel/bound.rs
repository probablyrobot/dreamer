//! Bound nodes drawn natively: a `transcript` (an agent session's
//! conversation as cards), a `composer`, and a `log` bound to a tap. Their
//! content comes from the streams the panel follows itself
//! (`app_model::bound`), never from the tree or the app.
//!
//! Only the newest part of a long transcript is built each frame
//! ([`MAX_LINES`] lines), and an answer's markdown is parsed once per
//! finished block (`view_tree::transcript::Markdown`), so a frame's work
//! stays bounded however long the conversation grows.

use std::collections::HashMap;

use app_model::bound::Data;
use app_model::session::{CLICK_ATTACH, CLICK_DETACH, CLICK_SEND, CLICK_STOP, CLICK_THINKING};
use gpui::{
    InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use view_tree::rich::{self, Block as RichBlock};
use view_tree::transcript::{Approval, Entry, State, Tool, ToolStatus, Transcript, tool_mark};
use view_tree::{Bind, Kind, Node};

use super::{LINE_HEIGHT, Lines, rich_text};
use crate::theme::{self, c, ca};

/// Lines of a transcript built per frame at most: the newest.
pub(crate) const MAX_LINES: usize = 600;
/// Output lines a tool card shows (its newest).
const TOOL_LINES: usize = 12;

/// Where a transcript's scroll was, to follow the newest while it is at
/// the bottom: its handle and the revision drawn last.
#[derive(Default)]
pub(crate) struct Follow {
    pub(crate) handle: gpui::ScrollHandle,
    pub(crate) revision: u64,
}

/// One line of markdown, as the markdown component draws it.
fn md_line(t: &theme::Theme, l: &rich::Line) -> gpui::Div {
    let d = div().min_h(px(LINE_HEIGHT));
    match l.block {
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
        RichBlock::Code => d
            .whitespace_nowrap()
            .px_2()
            .bg(c(t.raised))
            .child(rich_text(t, l)),
        RichBlock::Rule => d
            .flex()
            .items_center()
            .child(div().h(px(1.0)).w_full().bg(c(t.divider))),
        RichBlock::Text => d.child(rich_text(t, l)),
    }
}

/// A small button: a click on it runs `on`.
fn button(
    t: &theme::Theme,
    text: &str,
    strong: bool,
    on: impl Fn(&mut gpui::App) + 'static,
) -> gpui::Div {
    div()
        .flex_none()
        .cursor_pointer()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(c(if strong { t.accent } else { t.divider }))
        .bg(c(if strong { t.accent } else { t.raised }))
        .text_color(c(if strong { t.background } else { t.text }))
        .child(SharedString::from(text.to_owned()))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            on(cx);
        })
}

fn card(t: &theme::Theme, border: u32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_1()
        .rounded_sm()
        .border_1()
        .border_color(c(border))
        .bg(c(t.raised))
}

fn tool_card(t: &theme::Theme, tool: &Tool) -> gpui::Div {
    let color = match tool.status {
        ToolStatus::Finished => t.success,
        ToolStatus::Failed => t.danger,
        _ => t.warning,
    };
    let summary = tool.summary();
    let head = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(div().text_color(c(color)).child(tool_mark(tool.status)))
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from(tool.name.clone())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(c(t.muted))
                .child(SharedString::from(if tool.is_command() {
                    format!("$ {}", summary.replace('\n', " "))
                } else {
                    summary.replace('\n', " ")
                })),
        );
    let out = tool.output_lines();
    let cut = out.len().saturating_sub(TOOL_LINES);
    card(t, t.divider).child(head).when(!out.is_empty(), |d| {
        d.child(
            div()
                .flex()
                .flex_col()
                .px_1()
                .rounded_sm()
                .bg(c(t.background))
                .when(cut > 0, |d| {
                    d.child(
                        div()
                            .text_color(c(t.faint))
                            .child(SharedString::from(format!("… {cut} lines before"))),
                    )
                })
                .children(out[cut..].iter().map(|l| {
                    div()
                        .min_h(px(LINE_HEIGHT))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(rich_text(t, l))
                })),
        )
    })
}

/// An approval card's button pressed: which transcript, session, and
/// request, and the answer (with the option chosen, by index and name).
#[derive(Debug, Clone)]
pub(crate) struct Answering {
    pub(crate) node: String,
    pub(crate) session: String,
    pub(crate) approval: String,
    pub(crate) allow: bool,
    pub(crate) option: Option<(usize, String)>,
}

/// What a click on an approval card's button does: the panel answers it
/// for the app (with `for_pane`), or asks the app to.
pub(crate) type Answer = std::rc::Rc<dyn Fn(&mut gpui::App, Answering)>;

fn approval_card(
    t: &theme::Theme,
    node: &str,
    session: &str,
    a: &Approval,
    answer: &Answer,
) -> gpui::Div {
    let mut d = card(
        t,
        if a.answer.is_none() {
            t.warning
        } else {
            t.divider
        },
    )
    .child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(div().text_color(c(t.warning)).child("?"))
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(SharedString::from(a.title.clone())),
            ),
    );
    if let Some(m) = &a.message {
        d = d.child(
            div()
                .text_color(c(t.muted))
                .child(SharedString::from(m.clone())),
        );
    }
    match &a.answer {
        None => {
            let mut row = div().flex().flex_row().flex_wrap().gap_1();
            let base = Answering {
                node: node.to_owned(),
                session: session.to_owned(),
                approval: a.id.clone(),
                allow: true,
                option: None,
            };
            let options: Vec<Option<(usize, String)>> = if a.options.is_empty() {
                vec![None]
            } else {
                a.options.iter().cloned().enumerate().map(Some).collect()
            };
            for o in options {
                let label = o
                    .as_ref()
                    .map_or_else(|| "Allow".to_owned(), |(_, o)| o.clone());
                let (answer, what) = (
                    answer.clone(),
                    Answering {
                        option: o,
                        ..base.clone()
                    },
                );
                row = row.child(button(t, &label, true, move |cx| answer(cx, what.clone())));
            }
            let (answer, what) = (
                answer.clone(),
                Answering {
                    allow: false,
                    ..base
                },
            );
            row = row.child(button(t, "Deny", false, move |cx| answer(cx, what.clone())));
            d.child(row)
        }
        Some((ok, opt, by)) => d.child(
            div()
                .text_color(c(if *ok { t.success } else { t.danger }))
                .child(SharedString::from(format!(
                    "{}{} by {by}",
                    if *ok { "Allowed" } else { "Denied" },
                    opt.as_deref()
                        .map(|o| format!(" ({o})"))
                        .unwrap_or_default()
                ))),
        ),
    }
}

/// The entries to build: the newest, up to [`MAX_LINES`] lines.
fn newest(tr: &Transcript, thinking: bool) -> usize {
    let mut lines = 0;
    let mut first = tr.entries.len();
    for (i, e) in tr.entries.iter().enumerate().rev() {
        lines += match e {
            Entry::Answer(md) => md.len(),
            Entry::Thinking(s) if thinking => s.lines().count(),
            Entry::Tool(t) => 2 + t.output_lines().len().min(TOOL_LINES),
            _ => 2,
        };
        if lines > MAX_LINES && first < tr.entries.len() {
            break;
        }
        first = i;
    }
    first
}

/// A transcript: its cards in a column that follows the newest while it
/// is scrolled to the bottom, the plan, and its state and usage.
pub(crate) fn transcript(
    n: &Node,
    t: &theme::Theme,
    lines: &Lines,
    data: &Data,
    follow: &mut HashMap<String, Follow>,
    answer: &Answer,
) -> gpui::AnyElement {
    let Kind::Transcript { thinking, .. } = n.kind else {
        return div().into_any_element();
    };
    let (session, tr) = match &n.bind {
        Some(Bind::Session(s)) => (s.as_str(), data.transcript(s)),
        _ => ("", None),
    };
    let Some(tr) = tr else {
        return missing(n, t, data);
    };
    let f = follow.entry(n.key.clone()).or_default();
    // At the bottom (or never scrolled): keep following the newest.
    let at_bottom = -f32::from(f.handle.offset().y) >= f32::from(f.handle.max_offset().y) - 4.0;
    if tr.revision != f.revision && at_bottom {
        f.handle.scroll_to_bottom();
    }
    f.revision = tr.revision;
    let first = newest(tr, thinking);
    let entries = tr.entries[first..].iter().map(|e| match e {
        Entry::User(text) => div()
            .px_1()
            .rounded_sm()
            .bg(c(t.selected))
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Entry::Answer(md) => div()
            .flex()
            .flex_col()
            .children(md.lines().map(|l| md_line(t, l)))
            .into_any_element(),
        Entry::Thinking(s) => lines
            .wrap(
                &n.key,
                CLICK_THINKING,
                true,
                div()
                    .cursor_pointer()
                    .italic()
                    .text_color(c(t.faint))
                    .border_l_2()
                    .border_color(c(t.divider))
                    .pl_2()
                    .child(SharedString::from(if thinking {
                        s.clone()
                    } else {
                        format!("Thinking ({} lines)", s.lines().count().max(1))
                    })),
            )
            .into_any_element(),
        Entry::Tool(tool) => tool_card(t, tool).into_any_element(),
        Entry::Approval(a) => approval_card(t, &n.key, session, a, answer).into_any_element(),
        Entry::Error(m) => div()
            .text_color(c(t.danger))
            .child(SharedString::from(m.clone()))
            .into_any_element(),
        Entry::Note(m) => div()
            .text_color(c(t.faint))
            .child(SharedString::from(m.clone()))
            .into_any_element(),
    });
    let plan = (!tr.plan.is_empty()).then(|| {
        card(t, t.divider)
            .child(div().text_color(c(t.muted)).child("Plan"))
            .children(tr.plan.iter().map(|s| {
                let (mark, color) = match s.status.as_str() {
                    "completed" => ("✓", t.success),
                    "in_progress" => ("▸", t.accent),
                    _ => ("○", t.faint),
                };
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(div().text_color(c(color)).child(mark))
                    .child(SharedString::from(s.content.clone()))
            }))
    });
    let state_color = match tr.state {
        State::Working => t.accent,
        State::Waiting => t.warning,
        State::Detached | State::Closed => t.danger,
        _ => t.faint,
    };
    let footer = div()
        .flex()
        .flex_row()
        .gap_2()
        .text_size(px(11.0))
        .child(div().text_color(c(state_color)).child(tr.state.name()))
        .children(tr.usage.map(|u| {
            div()
                .text_color(c(t.faint))
                .child(SharedString::from(u.line()))
        }));
    div()
        .flex()
        .flex_col()
        .size_full()
        .child(
            div()
                .id(SharedString::from(format!("app-transcript-{}", n.key)))
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&f.handle)
                .when(first > 0, |d| {
                    d.child(
                        div()
                            .text_color(c(t.faint))
                            .child(SharedString::from(format!(
                                "{} earlier entries not shown",
                                first as u64 + tr.dropped
                            ))),
                    )
                })
                .children(entries)
                .children(plan),
        )
        .child(footer)
        .into_any_element()
}

/// A composer: its text with the cursor when focused (a click focuses
/// it), its attachments with a remove button each, and Send, Stop (while
/// a turn runs), and Attach selection. Clicks go to the app as clicks on
/// the node.
pub(crate) fn composer(
    n: &Node,
    focused: bool,
    t: &theme::Theme,
    lines: &Lines,
    data: &Data,
) -> gpui::AnyElement {
    let Kind::Composer {
        label,
        value,
        placeholder,
        attachments,
    } = &n.kind
    else {
        return div().into_any_element();
    };
    let busy = match &n.bind {
        Some(Bind::Session(s)) => data.transcript(s).is_some_and(|tr| tr.state.busy()),
        _ => false,
    };
    let empty = value.is_empty();
    let click = |i: usize, d: gpui::Div| lines.wrap(&n.key, i, true, d);
    let text = div()
        .flex()
        .flex_col()
        .min_h(px(LINE_HEIGHT * 2.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(c(if focused { t.focus } else { t.divider }))
        .bg(c(t.background))
        .when(empty && !focused, |d| {
            d.child(
                div()
                    .text_color(c(t.faint))
                    .child(SharedString::from(placeholder.clone())),
            )
        })
        .when(!empty || focused, |d| {
            let all: Vec<&str> = value.split('\n').collect();
            let last = all.len() - 1;
            d.children(all.into_iter().enumerate().map(|(i, l)| {
                div()
                    .flex()
                    .flex_row()
                    .min_h(px(LINE_HEIGHT))
                    .child(SharedString::from(l.to_owned()))
                    .when(focused && i == last, |d| {
                        d.child(div().text_color(c(t.accent)).child("▏"))
                    })
            }))
        });
    let chips =
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .children(attachments.iter().enumerate().map(|(i, a)| {
                click(
                    CLICK_DETACH + i,
                    div()
                        .cursor_pointer()
                        .px_1()
                        .rounded_sm()
                        .text_size(px(11.0))
                        .bg(ca((t.accent << 8) | 0x33))
                        .text_color(c(t.accent))
                        .child(SharedString::from(format!("{a} ×"))),
                )
            }));
    let small = |text: &str, strong: bool| {
        div()
            .flex_none()
            .cursor_pointer()
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(c(if strong { t.accent } else { t.divider }))
            .bg(c(if strong { t.accent } else { t.raised }))
            .text_color(c(if strong { t.background } else { t.text }))
            .child(SharedString::from(text.to_owned()))
    };
    let actions = div()
        .flex()
        .flex_row()
        .gap_1()
        .items_center()
        .children((!label.is_empty()).then(|| {
            div()
                .flex_1()
                .text_color(c(t.muted))
                .child(SharedString::from(label.clone()))
        }))
        .when(label.is_empty(), |d| d.child(div().flex_1()))
        .child(click(CLICK_ATTACH, small("Attach selection", false)))
        .when(busy, |d| d.child(click(CLICK_STOP, small("Stop", false))))
        .child(click(CLICK_SEND, small("Send", true)));
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(click(0, text))
        .when(!attachments.is_empty(), |d| d.child(chips))
        .child(actions)
        .into_any_element()
}

/// A log bound to a tap: the pane's newest lines, following the newest.
pub(crate) fn tap(n: &Node, pane: u32, t: &theme::Theme, data: &Data) -> gpui::AnyElement {
    let Some(lines) = data.taps.get(&pane) else {
        return missing(n, t, data);
    };
    let skip = lines.lines.len().saturating_sub(super::MAX_ROWS);
    div()
        .flex()
        .flex_col()
        .size_full()
        .justify_end()
        .overflow_hidden()
        .children(lines.lines.iter().skip(skip).map(|l| {
            div()
                .whitespace_nowrap()
                .min_h(px(LINE_HEIGHT))
                .child(SharedString::from(l.clone()))
        }))
        .into_any_element()
}

/// Why a bound node shows nothing yet.
fn missing(n: &Node, t: &theme::Theme, data: &Data) -> gpui::AnyElement {
    let why = match &n.bind {
        None => "No agent session bound".to_owned(),
        Some(b) => match data.error(b) {
            Some(e) => format!("Not shown: {e}"),
            None => "Connecting…".to_owned(),
        },
    };
    div()
        .text_color(c(t.faint))
        .child(SharedString::from(why))
        .into_any_element()
}
