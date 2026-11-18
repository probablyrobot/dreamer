//! Bound nodes in a terminal: a `transcript`, a `composer`, and a `log`
//! bound to a tap, drawn from the streams the host follows
//! (`app_model::bound`), not from the tree.

use app_model::bound::{Data, Streams};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use view_tree::transcript::{Entry, State, ToolStatus, Transcript, tool_mark};
use view_tree::{Bind, Kind, Node};

use crate::view::{ACCENT, rich_lines, role};

/// Output lines a tool card shows at most (its newest).
const TOOL_LINES: usize = 8;
/// Composer lines shown at most.
pub const COMPOSER_LINES: usize = 6;

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// A composer's rows: its text's lines (at most [`COMPOSER_LINES`]), and
/// one for its attachments when it has any.
pub fn composer_height(n: &Node) -> u16 {
    let Kind::Composer {
        value, attachments, ..
    } = &n.kind
    else {
        return 1;
    };
    let lines = value.split('\n').count().clamp(1, COMPOSER_LINES);
    (lines + usize::from(!attachments.is_empty())) as u16
}

/// `line` cut into lines `width` columns wide, styles kept.
fn wrap(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    if line.width() <= width {
        return vec![line];
    }
    let style = line.style;
    let mut out = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let w = Span::raw(ch.to_string()).width();
            if used + w > width && used > 0 {
                if !piece.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                out.push(Line::from(std::mem::take(&mut cur)).style(style));
                used = 0;
            }
            piece.push(ch);
            used += w;
        }
        if !piece.is_empty() {
            cur.push(Span::styled(piece, span.style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur).style(style));
    }
    out
}

/// One entry's lines, unwrapped.
fn entry_lines(e: &Entry, thinking: bool, width: u16) -> Vec<Line<'static>> {
    match e {
        Entry::User(t) => t
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                Line::from(vec![
                    Span::styled(
                        if i == 0 { "> " } else { "  " },
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(l.to_owned(), Style::default().add_modifier(Modifier::BOLD)),
                ])
            })
            .collect(),
        Entry::Answer(md) => rich_lines(md.lines().cloned().collect(), width),
        Entry::Thinking(t) if thinking => t
            .lines()
            .map(|l| Line::from(format!("┊ {l}")).style(dim().add_modifier(Modifier::ITALIC)))
            .collect(),
        Entry::Thinking(t) => vec![
            Line::from(format!(
                "┊ thinking ({} lines; t shows it)",
                t.lines().count().max(1)
            ))
            .style(dim().add_modifier(Modifier::ITALIC)),
        ],
        Entry::Tool(t) => {
            let color = match t.status {
                ToolStatus::Finished => Color::Green,
                ToolStatus::Failed => Color::Red,
                _ => Color::Yellow,
            };
            let summary = t.summary();
            let summary = if t.is_command() {
                format!("$ {summary}")
            } else {
                summary
            };
            let mut out = vec![Line::from(vec![
                Span::styled(
                    format!("{} ", tool_mark(t.status)),
                    Style::default().fg(color),
                ),
                Span::styled(
                    t.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(summary.replace('\n', " "), dim()),
            ])];
            let lines = t.output_lines();
            let cut = lines.len().saturating_sub(TOOL_LINES);
            if cut > 0 {
                out.push(Line::from(format!("  │ … {cut} lines before")).style(dim()));
            }
            for l in &lines[cut..] {
                let mut spans = vec![Span::styled("  │ ", dim())];
                spans.extend(
                    l.spans
                        .iter()
                        .map(|s| Span::styled(s.text.clone(), role(s.role))),
                );
                out.push(Line::from(spans));
            }
            out
        }
        Entry::Approval(a) => {
            let warn = Style::default().fg(Color::Yellow);
            let mut out = vec![Line::from(vec![
                Span::styled("? ", warn.add_modifier(Modifier::BOLD)),
                Span::styled(
                    a.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])];
            if let Some(m) = &a.message {
                out.extend(m.lines().map(|l| Line::from(format!("  {l}"))));
            }
            out.push(match &a.answer {
                None => {
                    let mut spans = vec![Span::raw("  ")];
                    if a.options.is_empty() {
                        spans.push(Span::styled("[y] allow", warn.add_modifier(Modifier::BOLD)));
                    } else {
                        for (i, o) in a.options.iter().enumerate() {
                            spans.push(Span::styled(
                                format!("[{}] {o}", i + 1),
                                warn.add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::raw("  "));
                        }
                    }
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled("[n] deny", Style::default().fg(Color::Red)));
                    Line::from(spans)
                }
                Some((ok, opt, by)) => Line::from(format!(
                    "  {}{} by {by}",
                    if *ok { "allowed" } else { "denied" },
                    opt.as_deref()
                        .map(|o| format!(" ({o})"))
                        .unwrap_or_default()
                ))
                .style(if *ok {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                }),
            });
            out
        }
        Entry::Error(m) => {
            vec![Line::from(format!("✗ {m}")).style(Style::default().fg(Color::Red))]
        }
        Entry::Note(n) => vec![Line::from(format!("· {n}")).style(dim())],
    }
}

/// The plan, a line per step.
fn plan_lines(t: &Transcript) -> Vec<Line<'static>> {
    if t.plan.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Line::from("Plan").style(Style::default().add_modifier(Modifier::BOLD))];
    for s in &t.plan {
        let (mark, style) = match s.status.as_str() {
            "completed" => ("✓", Style::default().fg(Color::Green)),
            "in_progress" => ("▸", Style::default().fg(ACCENT)),
            _ => ("○", dim()),
        };
        out.push(Line::from(vec![
            Span::styled(format!("{mark} "), style),
            Span::raw(s.content.clone()),
        ]));
    }
    out
}

/// The status line under a transcript: its state, and the usage.
fn footer(t: &Transcript) -> Line<'static> {
    let mut s = t.state.name().to_owned();
    if let Some(u) = &t.usage {
        s.push_str(" · ");
        s.push_str(&u.line());
    }
    let style = match t.state {
        State::Working => Style::default().fg(ACCENT),
        State::Waiting => Style::default().fg(Color::Yellow),
        State::Detached | State::Closed => Style::default().fg(Color::Red),
        _ => dim(),
    };
    Line::from(s).style(style)
}

/// Why a bound node shows nothing yet.
fn missing(n: &Node, d: &Data) -> Line<'static> {
    let why = match &n.bind {
        None => "no agent session bound".to_owned(),
        Some(b) => match d.error(b) {
            Some(e) => format!("not shown: {e}"),
            None => "connecting…".to_owned(),
        },
    };
    Line::from(why).style(dim())
}

/// A transcript: the newest lines that fit, `back` lines up from the
/// bottom; then the plan, and the state and usage on the last row. Only
/// the entries that can show are turned into lines.
pub fn draw_transcript(n: &Node, streams: &Streams, f: &mut Frame, area: Rect) {
    let Kind::Transcript { thinking, back } = n.kind else {
        return;
    };
    if area.height == 0 {
        return;
    }
    streams.with(|d| {
        let t = match &n.bind {
            Some(Bind::Session(s)) => d.transcript(s),
            _ => None,
        };
        let Some(t) = t else {
            f.render_widget(Paragraph::new(missing(n, d)), area);
            return;
        };
        let mut tail: Vec<Line<'static>> = plan_lines(t)
            .into_iter()
            .flat_map(|l| wrap(l, area.width))
            .collect();
        let room = usize::from(area.height.saturating_sub(1));
        let want = room + back as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for e in t.entries.iter().rev() {
            if lines.len() + tail.len() >= want {
                break;
            }
            let mut el: Vec<Line<'static>> = entry_lines(e, thinking, area.width)
                .into_iter()
                .flat_map(|l| wrap(l, area.width))
                .collect();
            el.append(&mut lines);
            lines = el;
        }
        lines.append(&mut tail);
        let back = (back as usize).min(lines.len().saturating_sub(room));
        let end = lines.len() - back;
        let start = end.saturating_sub(room);
        let mut shown: Vec<Line<'static>> = lines.drain(start..end).collect();
        shown.push(footer(t));
        f.render_widget(Paragraph::new(shown), area);
    });
}

/// A composer: its text (the newest lines that fit) with the cursor at
/// the end when focused, the placeholder when empty, what Esc does while
/// a turn runs, and its attachments.
pub fn draw_composer(n: &Node, focused: bool, streams: &Streams, f: &mut Frame, area: Rect) {
    let Kind::Composer {
        label,
        value,
        placeholder,
        attachments,
    } = &n.kind
    else {
        return;
    };
    let busy = match &n.bind {
        Some(Bind::Session(s)) => streams.with(|d| d.transcript(s).is_some_and(|t| t.state.busy())),
        _ => false,
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let prompt = if label.is_empty() {
        "> ".to_owned()
    } else {
        format!("{label} ")
    };
    let pad = " ".repeat(prompt.chars().count());
    if value.is_empty() && !focused {
        lines.push(Line::from(vec![
            Span::styled(prompt.clone(), Style::default().fg(ACCENT)),
            Span::styled(placeholder.clone(), dim()),
        ]));
    } else {
        let all: Vec<&str> = value.split('\n').collect();
        let from = all.len().saturating_sub(COMPOSER_LINES);
        let last = all.len() - 1;
        for (i, l) in all.iter().enumerate().skip(from) {
            let mut spans = vec![Span::styled(
                if i == from {
                    prompt.clone()
                } else {
                    pad.clone()
                },
                Style::default().fg(ACCENT),
            )];
            spans.push(Span::raw((*l).to_owned()));
            if focused && i == last {
                spans.push(Span::raw("▏"));
            }
            lines.push(Line::from(spans));
        }
    }
    if busy && let Some(first) = lines.first_mut() {
        first
            .spans
            .push(Span::styled("   working · Esc stops", dim()));
    }
    if !attachments.is_empty() {
        let mut spans = vec![Span::styled(pad, dim())];
        for a in attachments {
            spans.push(Span::styled(
                format!("[{a}]"),
                Style::default().fg(ACCENT).add_modifier(Modifier::DIM),
            ));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    let style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    f.render_widget(Paragraph::new(lines).style(style), area);
}

/// A log bound to a tap: the pane's newest committed lines.
pub fn draw_tap(n: &Node, pane: u32, streams: &Streams, f: &mut Frame, area: Rect) {
    streams.with(|d| {
        let Some(t) = d.taps.get(&pane) else {
            f.render_widget(Paragraph::new(missing(n, d)), area);
            return;
        };
        let room = usize::from(area.height);
        let skip = t.lines.len().saturating_sub(room);
        let lines: Vec<Line<'static>> = t
            .lines
            .iter()
            .skip(skip)
            .map(|l| Line::from(l.clone()))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{ViewState, draw_tree};
    use api::agent_session_event::Kind as K;

    fn ev(seq: u64, kind: K) -> api::AgentSessionEvent {
        api::AgentSessionEvent {
            seq,
            unix_ms: 0,
            kind: Some(kind),
        }
    }

    fn text(t: &str) -> api::AgentText {
        api::AgentText { text: t.into() }
    }

    fn screen(t: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let l: String = (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect();
                l.trim_end().to_owned() + "\n"
            })
            .collect()
    }

    #[test]
    fn a_streamed_transcript_draws_as_text_with_its_cards() {
        let mut state = ViewState::default();
        let streams = state.streams.clone();
        let mut events = vec![
            ev(1, K::UserMessage(text("use a tool"))),
            ev(2, K::ThinkingDelta(text("Hmm.\nMore."))),
        ];
        // A long answer, a few characters per delta.
        let answer: String = (1..=40)
            .map(|i| format!("Paragraph {i} is **here**.\n\n"))
            .collect::<String>()
            + "The end.";
        let chars: Vec<char> = answer.chars().collect();
        for c in chars.chunks(5) {
            let seq = events.len() as u64 + 1;
            events.push(ev(seq, K::TextDelta(text(&c.iter().collect::<String>()))));
        }
        let n = events.len() as u64;
        events.extend([
            ev(
                n + 1,
                K::ToolCall(api::AgentToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    status: api::AgentToolStatus::Finished as i32,
                    input: Some(r#"{"command":"ls"}"#.into()),
                    output: Some("file-a\nfile-b".into()),
                }),
            ),
            ev(
                n + 2,
                K::ApprovalRequest(api::AgentApprovalRequest {
                    id: "ui-1".into(),
                    title: "Allow bash?".into(),
                    message: Some("rm x".into()),
                    options: vec![],
                    expires_unix_ms: None,
                }),
            ),
        ]);
        // Delivered in batches, as a follow pushes them.
        for batch in events.chunks(7) {
            assert!(streams.take(&api::Notification::AgentSessionsEvents(
                api::AgentSessionEvents {
                    session: "s1".into(),
                    subscription: Some(1),
                    events: batch.to_vec(),
                    more: false,
                }
            )));
        }
        let mut root = Node::new("main", Kind::Column);
        root.children = vec![
            Node::new(
                "chat",
                Kind::Transcript {
                    thinking: false,
                    back: 0,
                },
            )
            .bound(Bind::Session("s1".into())),
            Node::new(
                "prompt",
                Kind::Composer {
                    label: String::new(),
                    value: "next\nquestion".into(),
                    placeholder: "Ask".into(),
                    attachments: vec!["@%3".into()],
                },
            )
            .bound(Bind::Session("s1".into())),
        ];
        let tree = view_tree::Frame {
            title: "Chat".into(),
            status: String::new(),
            focus: Some("prompt".into()),
            root,
        };
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).expect("terminal");
        t.draw(|f| draw_tree(&tree, &app_model::embeds::Embeds::default(), &mut state, f))
            .expect("draw");
        let s = screen(&t);
        // The newest of the long answer, then the cards, then the state.
        for want in [
            "Paragraph 40 is here.",
            "The end.",
            "✓ bash $ ls",
            "│ file-a",
            "? Allow bash?",
            "  rm x",
            "  [y] allow  [n] deny",
            "waiting for approval",
            "> next",
            "  question▏",
            "[@%3]",
        ] {
            assert!(s.contains(want), "{want:?} not in\n{s}");
        }
        assert!(
            !s.contains("Paragraph 1 "),
            "the oldest scrolled away:\n{s}"
        );
        // Scrolled back, the start shows: the prompt and folded thinking.
        let mut back = tree.clone();
        back.root.children[0].kind = Kind::Transcript {
            thinking: false,
            back: 1000,
        };
        t.draw(|f| draw_tree(&back, &app_model::embeds::Embeds::default(), &mut state, f))
            .expect("draw");
        let s = screen(&t);
        assert!(s.contains("> use a tool"), "{s}");
        assert!(s.contains("┊ thinking (2 lines; t shows it)"), "{s}");
        assert!(s.contains("Paragraph 1 is here."), "{s}");
    }
}
