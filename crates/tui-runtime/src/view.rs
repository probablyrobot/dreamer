//! Drawing an app with Ratatui, as a terminal app in a pane: its view
//! tree (every app but an agentic pane), or an agentic pane's prompt and
//! transcript.

use std::collections::HashMap;

use app_model::App;
use app_model::embeds::{Embeds, Screen};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction as Axis, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Tabs,
    Wrap,
};
use view_tree::rich::{self, Block as RichBlock, Role};
use view_tree::{Direction, Frame as Tree, Kind, Node, Tone};

/// What the terminal keeps per node key between draws, so an update to
/// the tree does not lose it: how far each table, list, and scroll node
/// is scrolled. And the streams bound nodes draw, as the host follows
/// them.
#[derive(Debug, Default)]
pub struct ViewState {
    offsets: HashMap<String, usize>,
    pub streams: app_model::bound::Streams,
}

/// Draw `app` once, with nothing kept from an earlier draw.
pub fn draw(app: &App, f: &mut Frame) {
    draw_with(app, &mut ViewState::default(), f);
}

/// Draw `app`, keeping scroll positions in `state` across draws.
pub fn draw_with(app: &App, state: &mut ViewState, f: &mut Frame) {
    match app.frame() {
        Some(tree) => draw_tree(&tree, &app.embeds, state, f),
        None => {
            let [main, status] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
            let title = if app.version > 1 {
                format!(" {} (reloaded {}) ", app.manifest.title, app.version - 1)
            } else {
                format!(" {} ", app.manifest.title)
            };
            draw_agent(app, f, main, Block::bordered().title(title));
            f.render_widget(Paragraph::new(Line::from(app.status_text())), status);
        }
    }
}

/// Draw a view tree in the whole frame: its title around the tree, and
/// its status line below. `embeds` holds the screens of the panes its
/// `terminal` nodes show.
pub fn draw_tree(tree: &Tree, embeds: &Embeds, state: &mut ViewState, f: &mut Frame) {
    let [main, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let block = Block::bordered().title(format!(" {} ", tree.title));
    let inner = block.inner(main);
    f.render_widget(block, main);
    draw_node(&tree.root, tree, embeds, state, f, inner);
    f.render_widget(Paragraph::new(Line::from(tree.status.clone())), status);
}

fn highlight() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// The rows a node wants, to share a column or a scroll.
fn natural(n: &Node) -> u16 {
    let border = if n.title.is_some() { 2 } else { 0 };
    let count = |k: usize| u16::try_from(k).unwrap_or(u16::MAX);
    let own = match &n.kind {
        Kind::Column | Kind::Scroll => n.children.iter().map(natural).fold(0, u16::saturating_add),
        Kind::Row | Kind::Split { .. } => n.children.iter().map(natural).max().unwrap_or(0),
        Kind::Tabs { .. } => 1 + n.shown().iter().map(natural).max().unwrap_or(0),
        Kind::Table { .. } => count(n.children.len()).saturating_add(1),
        Kind::List { .. } | Kind::Log => count(n.children.len()),
        Kind::Item { .. } => 1,
        Kind::Text { text } => count(text.lines().count().max(1)),
        Kind::Gauge { .. } => 1,
        Kind::Chart { .. } => 3,
        Kind::Form { fields, .. } => count(fields.len()),
        Kind::Markdown { text } => count(rich::markdown(text).len()),
        Kind::Code { text, .. } | Kind::Diff { text } => count(text.lines().count()),
        Kind::Transcript { .. } => 10,
        Kind::Composer { .. } => crate::bound::composer_height(n),
        Kind::Button { .. }
        | Kind::Input { .. }
        | Kind::Select { .. }
        | Kind::Toggle { .. }
        | Kind::Badge { .. }
        | Kind::Progress { .. }
        | Kind::Status { .. }
        | Kind::Image { .. } => 1,
        Kind::Terminal { .. } => 5,
    };
    own.saturating_add(border)
}

/// Takes the rows it needs in a column, not a share of what is left:
/// text, gauges, controls, badges, progress and status lines, and rows or
/// columns of only those.
fn fixed(n: &Node) -> bool {
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
        Kind::Row | Kind::Column => !n.children.is_empty() && n.children.iter().all(fixed),
        _ => false,
    }
}

/// How a child shares a column: fixed nodes take what they need, the rest
/// share what is left.
fn share(n: &Node) -> Constraint {
    if fixed(n) {
        Constraint::Length(natural(n))
    } else {
        Constraint::Fill(1)
    }
}

/// How a child shares a row: a badge or a button takes its own width
/// (and a space), the rest share what is left.
fn share_row(n: &Node) -> Constraint {
    let width = |s: &str| u16::try_from(Line::from(s.to_owned()).width()).unwrap_or(u16::MAX);
    match &n.kind {
        Kind::Badge { text, .. } if n.title.is_none() => Constraint::Length(width(text) + 3),
        Kind::Button { label } if n.title.is_none() => Constraint::Length(width(label) + 5),
        _ => Constraint::Fill(1),
    }
}

/// Colours come from the terminal's own palette, by ANSI index, never as
/// RGB: in Dreamer that palette is the one scheme, so the terminal draws
/// the components in the same colours the desktop takes from it.
pub(crate) const ACCENT: Color = Color::Blue;

fn tone(t: Tone) -> Style {
    let fg = match t {
        Tone::Neutral => return Style::default().add_modifier(Modifier::DIM),
        Tone::Accent => ACCENT,
        Tone::Success => Color::Green,
        Tone::Warning => Color::Yellow,
        Tone::Danger => Color::Red,
    };
    Style::default().fg(fg)
}

/// How a span of markdown, code, or a diff is drawn.
pub(crate) fn role(r: Role) -> Style {
    let s = Style::default();
    match r {
        Role::Plain => s,
        Role::Strong => s.add_modifier(Modifier::BOLD),
        Role::Emphasis => s.add_modifier(Modifier::ITALIC),
        Role::Code => s.fg(Color::Cyan),
        Role::Link => s.fg(ACCENT).add_modifier(Modifier::UNDERLINED),
        Role::Marker => s.fg(ACCENT),
        Role::Keyword => s.fg(Color::Magenta),
        Role::String => s.fg(Color::Green),
        Role::Number => s.fg(Color::Yellow),
        Role::Comment => s.add_modifier(Modifier::DIM | Modifier::ITALIC),
        Role::Type => s.fg(Color::Cyan),
        Role::Added => s.fg(Color::Green),
        Role::Removed => s.fg(Color::Red),
        Role::Hunk => s.fg(Color::Cyan),
        Role::Meta => s.add_modifier(Modifier::BOLD),
    }
}

/// Rich lines as Ratatui lines, `width` columns wide (for rules).
pub(crate) fn rich_lines(lines: Vec<rich::Line>, width: u16) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|l| {
            let spans: Vec<Span<'static>> = l
                .spans
                .iter()
                .map(|s| Span::styled(s.text.clone(), role(s.role)))
                .collect();
            match l.block {
                RichBlock::Heading(level) => {
                    let mut style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
                    if level == 1 {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    Line::from(spans).style(style)
                }
                RichBlock::Quote => {
                    let mut all = vec![Span::styled("│ ", Style::default().fg(ACCENT))];
                    all.extend(spans);
                    Line::from(all).style(Style::default().add_modifier(Modifier::ITALIC))
                }
                RichBlock::Code => {
                    let mut all = vec![Span::raw("  ")];
                    all.extend(spans);
                    Line::from(all)
                }
                RichBlock::Rule => Line::from("─".repeat(usize::from(width)))
                    .style(Style::default().add_modifier(Modifier::DIM)),
                RichBlock::Text => Line::from(spans),
            }
        })
        .collect()
}

/// A `terminal` node's lines in `area`: the pane's screen, as many of its
/// rows as fit with the cursor's in view; or why it is not shown.
fn terminal_lines(pane: u32, refused: &str, embeds: &Embeds, area: Rect) -> Vec<Line<'static>> {
    let note = |s: String, style: Style| vec![Line::from(Span::styled(s, style))];
    if !refused.is_empty() {
        return note(format!("refused: {refused}"), tone(Tone::Danger));
    }
    match embeds.panes.get(&pane) {
        Some(Screen::Lines { lines, cursor_y }) => {
            let rows = usize::from(area.height);
            let cursor = (*cursor_y as usize + 1).min(lines.len());
            let start = cursor.saturating_sub(rows);
            lines
                .iter()
                .skip(start)
                .take(rows)
                .map(|l| Line::from(l.clone()))
                .collect()
        }
        Some(Screen::Refused(why)) => note(format!("refused: {why}"), tone(Tone::Danger)),
        Some(Screen::Unavailable(why)) => note(format!("pane %{pane}: {why}"), tone(Tone::Warning)),
        None => note(format!("pane %{pane}…"), tone(Tone::Neutral)),
    }
}

/// An `image` node in a terminal: a placeholder with its name and size.
fn image_line(src: &str, width: u32, height: u32, refused: &str) -> Line<'static> {
    let name = std::path::Path::new(src)
        .file_name()
        .map_or_else(|| src.to_owned(), |n| n.to_string_lossy().into_owned());
    if !refused.is_empty() {
        return Line::from(Span::styled(
            format!("[image: {name}: {refused}]"),
            tone(Tone::Danger),
        ));
    }
    let size = if width > 0 {
        format!(" {width}x{height}")
    } else {
        String::new()
    };
    Line::from(Span::styled(
        format!("[image: {name}{size}]"),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// The style of a control: reversed while it has the focus.
fn control(focused: bool, base: Style) -> Style {
    if focused {
        base.add_modifier(Modifier::REVERSED)
    } else {
        base
    }
}

fn label_span(label: &str) -> Option<Span<'static>> {
    (!label.is_empty()).then(|| {
        Span::styled(
            format!("{label} "),
            Style::default().add_modifier(Modifier::DIM),
        )
    })
}

fn draw_node(
    n: &Node,
    tree: &Tree,
    embeds: &Embeds,
    state: &mut ViewState,
    f: &mut Frame,
    area: Rect,
) {
    let focused = tree.focus.as_deref() == Some(n.key.as_str());
    let area = match &n.title {
        Some(t) => {
            let mut block = Block::bordered().title(format!(" {t} "));
            if focused {
                block = block.border_style(Style::default().add_modifier(Modifier::BOLD));
            }
            let inner = block.inner(area);
            f.render_widget(block, area);
            inner
        }
        None => area,
    };
    match &n.kind {
        Kind::Column | Kind::Row => {
            let (axis, constraints): (Axis, Vec<Constraint>) = if n.kind == Kind::Column {
                (Axis::Vertical, n.children.iter().map(share).collect())
            } else {
                (Axis::Horizontal, n.children.iter().map(share_row).collect())
            };
            let areas = Layout::default()
                .direction(axis)
                .constraints(constraints)
                .split(area);
            for (c, a) in n.children.iter().zip(areas.iter()) {
                draw_node(c, tree, embeds, state, f, *a);
            }
        }
        Kind::Split { direction, sizes } => {
            let total: u32 = if sizes.len() == n.children.len() {
                sizes.iter().map(|s| u32::from(*s)).sum()
            } else {
                0
            };
            let constraints: Vec<Constraint> = if total == 0 {
                vec![Constraint::Fill(1); n.children.len()]
            } else {
                sizes
                    .iter()
                    .map(|s| Constraint::Ratio(u32::from(*s), total))
                    .collect()
            };
            let axis = match direction {
                Direction::Row => Axis::Horizontal,
                Direction::Column => Axis::Vertical,
            };
            let areas = Layout::default()
                .direction(axis)
                .constraints(constraints)
                .split(area);
            for (c, a) in n.children.iter().zip(areas.iter()) {
                draw_node(c, tree, embeds, state, f, *a);
            }
        }
        Kind::Tabs { active } => {
            let [bar, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
            let titles: Vec<String> = n
                .children
                .iter()
                .map(|c| c.title.clone().unwrap_or_else(|| c.key.clone()))
                .collect();
            f.render_widget(
                Tabs::new(titles)
                    .select(*active)
                    .highlight_style(highlight()),
                bar,
            );
            if let Some(page) = n.shown().first() {
                // The page's own title is its tab's name.
                let mut page = page.clone();
                page.title = None;
                draw_node(&page, tree, embeds, state, f, body);
            }
        }
        Kind::Scroll => {
            // Scrolled by whole children, to keep the focused one in view.
            let heights: Vec<u16> = n.children.iter().map(natural).collect();
            let mut first = state.offsets.get(&n.key).copied().unwrap_or(0);
            first = first.min(n.children.len().saturating_sub(1));
            let holding = tree
                .focus
                .as_deref()
                .and_then(|k| n.children.iter().position(|c| c.find(k).is_some()));
            if let Some(i) = holding {
                if i < first {
                    first = i;
                }
                while first < i
                    && heights[first..=i]
                        .iter()
                        .map(|h| u32::from(*h))
                        .sum::<u32>()
                        > u32::from(area.height)
                {
                    first += 1;
                }
            }
            state.offsets.insert(n.key.clone(), first);
            let mut y = area.y;
            let bottom = area.y.saturating_add(area.height);
            for (c, h) in n.children.iter().zip(&heights).skip(first) {
                if y >= bottom {
                    break;
                }
                let h = (*h).min(bottom - y);
                draw_node(
                    c,
                    tree,
                    embeds,
                    state,
                    f,
                    Rect::new(area.x, y, area.width, h),
                );
                y += h;
            }
        }
        Kind::Table { columns, .. } => {
            let header =
                Row::new(columns.clone()).style(Style::default().add_modifier(Modifier::BOLD));
            let rows = n.children.iter().map(|r| Row::new(r.cells().to_vec()));
            let widths = vec![Constraint::Fill(1); columns.len()];
            let table = Table::new(rows, widths)
                .header(header)
                .row_highlight_style(highlight());
            let mut s = TableState::default()
                .with_offset(state.offsets.get(&n.key).copied().unwrap_or(0))
                .with_selected(n.selected_index());
            f.render_stateful_widget(table, area, &mut s);
            state.offsets.insert(n.key.clone(), s.offset());
        }
        Kind::Log if matches!(n.bind, Some(view_tree::Bind::Tap(_))) => {
            if let Some(view_tree::Bind::Tap(pane)) = &n.bind {
                crate::bound::draw_tap(n, *pane, &state.streams, f, area);
            }
        }
        Kind::Transcript { .. } => crate::bound::draw_transcript(n, &state.streams, f, area),
        Kind::Composer { .. } => {
            crate::bound::draw_composer(n, focused, &state.streams, f, area);
        }
        Kind::List { .. } | Kind::Log => {
            let items: Vec<ListItem> = n
                .children
                .iter()
                .map(|r| ListItem::new(r.cells().first().cloned().unwrap_or_default()))
                .collect();
            let list = List::new(items).highlight_style(highlight());
            let selected = if n.kind == Kind::Log {
                // A log follows its newest line.
                n.children.len().checked_sub(1)
            } else {
                n.selected_index()
            };
            let mut s = ListState::default()
                .with_offset(state.offsets.get(&n.key).copied().unwrap_or(0))
                .with_selected(selected);
            f.render_stateful_widget(list, area, &mut s);
            state.offsets.insert(n.key.clone(), s.offset());
        }
        Kind::Item { .. } => {}
        Kind::Text { text } => {
            f.render_widget(
                Paragraph::new(text.clone()).wrap(Wrap { trim: false }),
                area,
            );
        }
        Kind::Gauge { ratio, label } => {
            f.render_widget(
                Gauge::default()
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(label.clone()),
                area,
            );
        }
        Kind::Chart { values } => {
            let data: Vec<u64> = values.iter().map(|v| v.max(0.0).round() as u64).collect();
            f.render_widget(Sparkline::default().data(&data), area);
        }
        Kind::Markdown { text } => {
            let code_width = area.width;
            f.render_widget(
                Paragraph::new(rich_lines(rich::markdown(text), code_width))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        Kind::Code { text, language } => {
            f.render_widget(
                Paragraph::new(rich_lines(rich::code(text, language), area.width)),
                area,
            );
        }
        Kind::Diff { text } => {
            f.render_widget(
                Paragraph::new(rich_lines(rich::diff(text), area.width)),
                area,
            );
        }
        Kind::Button { label } => {
            let style = control(
                focused,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(format!("[ {label} ]"), style))),
                area,
            );
        }
        Kind::Input {
            label,
            value,
            placeholder,
        } => {
            let mut spans: Vec<Span> = label_span(label).into_iter().collect();
            let used: usize = spans.iter().map(Span::width).sum();
            let room = usize::from(area.width).saturating_sub(used).max(1);
            let (shown, style) = if value.is_empty() && !focused {
                (
                    placeholder.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                )
            } else {
                // The end of a long value stays in view, with the cursor.
                let cursor = if focused { "▏" } else { "" };
                let chars: Vec<char> = value.chars().collect();
                let keep = room.saturating_sub(cursor.chars().count());
                let tail: String = chars[chars.len().saturating_sub(keep)..].iter().collect();
                (format!("{tail}{cursor}"), Style::default())
            };
            let pad = room.saturating_sub(shown.chars().count());
            spans.push(Span::styled(
                format!("{shown}{}", " ".repeat(pad)),
                control(focused, style.add_modifier(Modifier::UNDERLINED)),
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
        }
        Kind::Select {
            label,
            options,
            selected,
        } => {
            let mut spans: Vec<Span> = label_span(label).into_iter().collect();
            for (i, o) in options.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                let style = if i == *selected {
                    control(
                        focused,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                let mark = if i == *selected { "●" } else { "○" };
                spans.push(Span::styled(format!("{mark} {o}"), style));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), area);
        }
        Kind::Toggle { label, on } => {
            let (mark, base) = if *on {
                ("[x]", Style::default().fg(ACCENT))
            } else {
                ("[ ]", Style::default().add_modifier(Modifier::DIM))
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(mark, control(focused, base)),
                    Span::raw(format!(" {label}")),
                ])),
                area,
            );
        }
        Kind::Badge { text, tone: t } => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {text} "),
                    tone(*t).add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ))),
                area,
            );
        }
        Kind::Progress { ratio, label } => {
            let label = if label.is_empty() {
                String::new()
            } else {
                format!(" {label}")
            };
            let room = usize::from(area.width).saturating_sub(label.chars().count());
            let done = ((ratio.clamp(0.0, 1.0) * room as f64).round() as usize).min(room);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("━".repeat(done), Style::default().fg(ACCENT)),
                    Span::styled(
                        "─".repeat(room - done),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(label),
                ])),
                area,
            );
        }
        Kind::Terminal { pane, refused } => {
            f.render_widget(
                Paragraph::new(terminal_lines(*pane, refused, embeds, area)),
                area,
            );
        }
        Kind::Image {
            src,
            width,
            height,
            refused,
            ..
        } => {
            f.render_widget(
                Paragraph::new(image_line(src, *width, *height, refused)),
                area,
            );
        }
        Kind::Status { items } => {
            let mut spans = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(
                        " · ",
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                spans.push(Span::styled(item.text.clone(), tone(item.tone)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), area);
        }
        Kind::Form { fields, focus } => {
            let lines: Vec<Line> = fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let shown = match (field.toggle, field.value.as_str()) {
                        (true, "true") => "[x]".to_owned(),
                        (true, _) => "[ ]".to_owned(),
                        (false, v) => v.to_owned(),
                    };
                    let star = if field.required { "*" } else { "" };
                    let line = Line::from(format!("{}{star}: {shown}", field.label));
                    if focused && i == *focus {
                        line.style(highlight())
                    } else {
                        line
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(lines), area);
        }
    }
}

/// An agentic pane: who it is and what it may do, the transcript (newest
/// at the bottom), and the prompt.
/// An operator's plan and steps, one line each: what it said it would do,
/// then every action or job with its target and its state as last read.
fn operator_lines(app: &App) -> Vec<Line<'static>> {
    use ratatui::style::Color;
    let a = &app.agent;
    let mut out = Vec::new();
    if let Some(plan) = &a.plan {
        out.push(
            Line::from(format!("plan: {plan}"))
                .style(Style::default().add_modifier(Modifier::ITALIC)),
        );
    }
    for t in &a.taken {
        let (mark, style) = match (t.ok, t.open) {
            (false, _) => ("✗", Style::default().fg(Color::Red)),
            (true, true) => ("…", Style::default().fg(Color::Yellow)),
            (true, false) => ("✓", Style::default().fg(Color::Green)),
        };
        let host = t
            .host
            .as_deref()
            .map(|h| format!(" @{h}"))
            .unwrap_or_default();
        out.push(
            Line::from(format!(
                "{mark} {} {}{host} → {}",
                t.tool, t.target, t.state
            ))
            .style(style),
        );
    }
    out
}

fn draw_agent(app: &App, f: &mut Frame, area: ratatui::layout::Rect, block: Block) {
    use app_model::Said;
    use ratatui::style::Color;
    let a = &app.agent;
    let inner = block.inner(area);
    f.render_widget(block, area);
    // An operator shows its plan and what it did above the transcript.
    let steps = operator_lines(app);
    let steps_h = if steps.is_empty() {
        0
    } else {
        u16::try_from(steps.len())
            .unwrap_or(u16::MAX)
            .min(inner.height / 3)
            + 1
    };
    let [head, done, body, prompt] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(steps_h),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    if !steps.is_empty() {
        // The newest steps when they do not all fit.
        let skip = steps
            .len()
            .saturating_sub(usize::from(done.height.saturating_sub(1)));
        f.render_widget(
            Paragraph::new(steps.into_iter().skip(skip).collect::<Vec<_>>())
                .block(Block::default().borders(ratatui::widgets::Borders::BOTTOM)),
            done,
        );
    }
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
    let mut watching = String::new();
    if kind == "filter" {
        watching = if a.filters.is_empty() {
            "  no filters yet".to_owned()
        } else {
            format!("  filters: {}", a.filters.join(", "))
        };
    }
    if !app.manifest.view.source.is_empty() {
        watching = format!(
            "  watching {}: {} rows",
            app.manifest.view.source,
            app.rows.len()
        );
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!("{kind} · {} · {activity}{watching}", a.mode)),
            Line::from(format!("grant: {}", a.grant))
                .style(Style::default().add_modifier(Modifier::DIM)),
        ]),
        head,
    );
    let lines: Vec<Line> = a
        .transcript
        .iter()
        .flat_map(|e| {
            let (mark, style) = match e.who {
                Said::You => ("> ", Style::default().add_modifier(Modifier::BOLD)),
                Said::Agent => ("", Style::default()),
                Said::Tool => ("→ ", Style::default().fg(Color::Cyan)),
                Said::Result { ok: true } => ("← ", Style::default().add_modifier(Modifier::DIM)),
                Said::Result { ok: false } => ("← failed: ", Style::default().fg(Color::Red)),
                Said::Note => ("· ", Style::default().add_modifier(Modifier::DIM)),
                Said::Failed => ("✗ ", Style::default().fg(Color::Red)),
            };
            e.text
                .lines()
                .enumerate()
                .map(move |(i, l)| {
                    let lead = if i == 0 { mark } else { "  " };
                    Line::from(format!("{lead}{l}")).style(style)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // Follow the newest lines, counting wrapped ones.
    let width = body.width.max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(width)).sum();
    let scroll = rows.saturating_sub(body.height as usize);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        body,
    );
    f.render_widget(Paragraph::new(format!("> {}▏", a.input)), prompt);
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_model::Link;

    fn render(m: &str, rows: Vec<serde_json::Value>) -> String {
        let manifest = app_model::manifest::Manifest::parse(m).expect("manifest");
        let mut a = App::new(manifest);
        a.link = Link::Live;
        a.set_rows(rows, 0);
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 6)).expect("terminal");
        t.draw(|f| draw(&a, f)).expect("draw");
        let buf = t.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const HEAD: &str = "schema_version = 1\nid = \"t.g\"\nversion = \"1\"\ntitle = \"G\"\n[data.s]\nmethod = \"daemon.status\"\n";

    #[test]
    fn a_gauge_shows_its_value_and_label() {
        let shown = render(
            &format!(
                "{HEAD}[view]\ntype = \"gauge\"\nsource = \"s\"\nfield = \"done\"\nlabel = \"what\"\n"
            ),
            vec![serde_json::json!({"done": 50, "what": "half way"})],
        );
        assert!(shown.contains("half way"), "{shown}");
        assert!(shown.contains("[live]"), "{shown}");
    }

    #[test]
    fn a_chart_draws_a_bar_per_row() {
        let shown = render(
            &format!("{HEAD}[view]\ntype = \"chart\"\nsource = \"s\"\nfield = \"n\"\n"),
            (0..8).map(|n| serde_json::json!({"n": n})).collect(),
        );
        let bars = shown.chars().filter(|c| "▁▂▃▄▅▆▇█".contains(*c)).count();
        assert!(bars >= 7, "{shown}");
    }

    /// A nested tree: a table beside tabs holding a list and a gauge.
    const TREE: &str = r#"
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
sizes = [60, 40]
children = ["left", "side"]

[nodes.left]
type = "table"
title = "Panes"
source = "panes"
row_key = "id"
columns = ["id", "cmd"]

[nodes.side]
type = "column"
children = ["note", "names"]

[nodes.note]
type = "text"
source = "windows"
field = "name"

[nodes.names]
type = "list"
title = "Windows"
source = "windows"
row_key = "id"
field = "name"
"#;

    fn screen(t: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Where `what` is drawn: (column, row) in characters.
    fn at(lines: &[String], what: &str) -> (usize, usize) {
        lines
            .iter()
            .enumerate()
            .find_map(|(y, l)| l.find(what).map(|b| (l[..b].chars().count(), y)))
            .unwrap_or_else(|| panic!("`{what}` is not drawn:\n{}", lines.join("\n")))
    }

    /// The host's tree reaches a desktop as the desktop reads it (the whole
    /// tree, then patches, through the daemon's copy), and both draw the
    /// same structure: the desktop draws exactly the tree the terminal
    /// draws, and the terminal places each node where the layout says.
    #[test]
    fn a_nested_tree_reaches_the_desktop_as_the_terminal_draws_it() {
        let manifest = app_model::manifest::Manifest::parse(TREE).expect("manifest");
        let mut a = App::new(manifest);
        a.link = Link::Live;
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
        // Host, daemon, desktop.
        let mut host = view_tree::Outgoing::default();
        let mut daemon = view_tree::Kept::default();
        let mut desktop = view_tree::Mirror::default();
        let send = |host: &mut view_tree::Outgoing,
                    daemon: &mut view_tree::Kept,
                    desktop: &mut view_tree::Mirror,
                    frame: &view_tree::Frame| {
            match host.next(frame).expect("an update") {
                view_tree::Update::Full { seq, frame } => {
                    daemon.publish(seq, &serde_json::to_string(&frame).expect("json"), "")
                }
                view_tree::Update::Patch { seq, ops } => {
                    daemon.publish(seq, "", &serde_json::to_string(&ops).expect("json"))
                }
            }
            .expect("published");
            let s = daemon.since(desktop.seq);
            desktop.take(s.seq, &s.tree, &s.patches).expect("taken");
            s
        };
        let tree = a.frame().expect("a tree");
        let first = send(&mut host, &mut daemon, &mut desktop, &tree);
        assert!(!first.tree.is_empty(), "a new reader gets the whole tree");
        assert_eq!(desktop.frame.as_ref(), Some(&tree));
        assert_eq!(
            tree.outline(),
            "split main\n  table left (2 rows)\n  column side\n    text note\n    list names (2 rows)\n"
        );

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).expect("terminal");
        let mut state = ViewState::default();
        term.draw(|f| draw_with(&a, &mut state, f)).expect("draw");
        let lines = screen(&term);
        let (panes_x, panes_y) = at(&lines, " Panes ");
        let (windows_x, windows_y) = at(&lines, " Windows ");
        let (note_x, note_y) = at(&lines, "build");
        let (vim_x, vim_y) = at(&lines, "vim");
        // The split: the table on the left 60%, the column on the right.
        assert!(panes_x < 30 && windows_x >= 30, "{}", lines.join("\n"));
        assert!(vim_x < 30 && vim_y > panes_y, "{}", lines.join("\n"));
        // The column: the text above the titled list.
        assert!(note_x >= 30 && note_y < windows_y, "{}", lines.join("\n"));
        assert!(lines[0].contains(" Tree "), "{}", lines.join("\n"));

        // One row changes: one patch, to that row, reaches the desktop;
        // both draw the change.
        a.set_source(
            "panes",
            vec![
                serde_json::json!({"id": 1, "cmd": "sh"}),
                serde_json::json!({"id": 2, "cmd": "htop"}),
            ],
            1,
        );
        let changed = a.frame().expect("a tree");
        let next = send(&mut host, &mut daemon, &mut desktop, &changed);
        assert!(next.tree.is_empty(), "a reader that keeps up gets patches");
        assert_eq!(next.patches.len(), 1);
        let ops: Vec<view_tree::Op> = serde_json::from_str(&next.patches[0]).expect("ops");
        assert!(
            matches!(&ops[..], [view_tree::Op::Props { path, .. }] if path == &["main", "left", "2"]),
            "{ops:?}"
        );
        assert_eq!(desktop.frame.as_ref(), Some(&changed));
        term.draw(|f| draw_tree(&changed, &Embeds::default(), &mut state, f))
            .expect("draw");
        let lines = screen(&term);
        assert_eq!(at(&lines, "htop").1, vim_y, "{}", lines.join("\n"));
    }

    #[test]
    fn a_table_keeps_its_scroll_across_updates() {
        let manifest = app_model::manifest::Manifest::parse(TREE).expect("manifest");
        let mut a = App::new(manifest);
        let rows = |n: usize| {
            (0..n)
                .map(|i| serde_json::json!({"id": i, "cmd": format!("c{i}")}))
                .collect::<Vec<_>>()
        };
        a.set_source("panes", rows(40), 0);
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).expect("terminal");
        let mut state = ViewState::default();
        a.move_by(30);
        term.draw(|f| draw_with(&a, &mut state, f)).expect("draw");
        let scrolled = state.offsets.get("left").copied().expect("an offset");
        assert!(scrolled > 0);
        // Selection one up, still in view, and new rows at the end: the
        // table does not jump back to the top.
        a.move_by(-1);
        a.set_source("panes", rows(41), 1);
        term.draw(|f| draw_with(&a, &mut state, f)).expect("draw");
        assert_eq!(state.offsets.get("left"), Some(&scrolled));
    }

    /// The reference app: every text, input, and status component.
    const COMPONENTS: &str = include_str!("../../../examples/apps/components/app.toml");

    fn components() -> App {
        let mut a = App::new(app_model::manifest::Manifest::parse(COMPONENTS).expect("manifest"));
        a.link = Link::Live;
        a
    }

    /// The cell where `what` starts.
    fn cell_at<'a>(
        t: &'a ratatui::Terminal<ratatui::backend::TestBackend>,
        what: &str,
    ) -> &'a ratatui::buffer::Cell {
        let (x, y) = at(&screen(t), what);
        &t.backend().buffer()[(x as u16, y as u16)]
    }

    fn draw_app(a: &App, w: u16, h: u16) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("terminal");
        t.draw(|f| draw(a, f)).expect("draw");
        t
    }

    #[test]
    fn the_reference_app_draws_every_component_in_the_terminal() {
        let mut a = components();
        let t = draw_app(&a, 110, 34);
        let lines = screen(&t);
        let shown = lines.join("\n");
        for what in [
            " Components ",
            "Readme",
            "Text with strong and emphasis, inline code, and a link.",
            "• a list item",
            "  • nested",
            "1. numbered",
            "│ A quote.",
            "  fn main() {",
            "Name ▏",
            "● dark ○ light ○ follow the system",
            "[x] Notify when done",
            "[ Save ]",
            " passing ",
            " 1 failing ",
            "build 62%",
            "main · 3 ahead · 1 conflict · utf-8",
            "s save  q quit",
        ] {
            assert!(shown.contains(what), "`{what}` is not drawn:\n{shown}");
        }
        // Styles: a heading in the accent, emphasis, the focused input
        // reversed, a badge in its tone, the progress bar's done part.
        let (x, y) = at(&lines, "│Components");
        let heading = &t.backend().buffer()[(x as u16 + 1, y as u16)];
        assert_eq!(heading.fg, ACCENT);
        assert!(heading.modifier.contains(Modifier::BOLD));
        let strong = cell_at(&t, "strong");
        assert!(strong.modifier.contains(Modifier::BOLD));
        assert!(cell_at(&t, "emphasis").modifier.contains(Modifier::ITALIC));
        assert_eq!(cell_at(&t, "inline code").fg, Color::Cyan);
        assert_eq!(cell_at(&t, "link.").fg, ACCENT);
        assert!(cell_at(&t, "▏").modifier.contains(Modifier::REVERSED));
        let passing = cell_at(&t, "passing");
        assert_eq!(passing.fg, Color::Green);
        assert!(passing.modifier.contains(Modifier::REVERSED));
        assert_eq!(cell_at(&t, "1 failing").fg, Color::Red);
        assert_eq!(cell_at(&t, "━").fg, ACCENT);
        assert_eq!(cell_at(&t, "1 conflict").fg, Color::Red);
        assert_eq!(cell_at(&t, "fn main").fg, Color::Magenta);

        // The code tab: highlighted by language.
        a.switch_tab(1);
        let t = draw_app(&a, 110, 34);
        assert_eq!(cell_at(&t, "/// Sums").fg, Color::Reset);
        assert!(cell_at(&t, "/// Sums").modifier.contains(Modifier::DIM));
        assert_eq!(cell_at(&t, "fn total").fg, Color::Magenta);
        assert_eq!(cell_at(&t, "\"total\"").fg, Color::Green);
        assert_eq!(cell_at(&t, "39").fg, Color::Yellow);
        // The diff tab: added and removed lines.
        a.switch_tab(1);
        let t = draw_app(&a, 110, 34);
        assert_eq!(cell_at(&t, "+    println!").fg, Color::Green);
        assert_eq!(cell_at(&t, "-    println!").fg, Color::Red);
        assert_eq!(cell_at(&t, "@@ -1,4").fg, Color::Cyan);

        // Only the scheme's own palette: no colour by RGB or past the 16
        // ANSI colours, so the terminal draws in the one theme.
        let buf = t.backend().buffer();
        for c in buf.content() {
            for color in [c.fg, c.bg] {
                assert!(
                    !matches!(color, Color::Rgb(..) | Color::Indexed(_)),
                    "{color:?}"
                );
            }
        }
    }

    #[test]
    fn typed_keys_and_focus_show_in_the_terminal() {
        let mut a = components();
        for k in ["h", "i"] {
            a.control_key(k);
        }
        a.focus_next();
        a.control_key("Down");
        let t = draw_app(&a, 110, 34);
        let shown = screen(&t).join("\n");
        assert!(shown.contains("Name hi "), "{shown}");
        assert!(!shown.contains("hi▏"), "the cursor leaves with the focus");
        assert!(cell_at(&t, "● light").modifier.contains(Modifier::REVERSED));
        assert!(!cell_at(&t, "○ dark").modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_terminal_shows_its_panes_screen_and_an_image_its_placeholder() {
        let term = |key: &str, pane: u32, refused: &str| {
            Node::new(
                key,
                Kind::Terminal {
                    pane,
                    refused: refused.into(),
                },
            )
        };
        let mut root = Node::new("main", Kind::Column);
        let mut row = Node::new("panes", Kind::Row);
        row.children = vec![
            term("a", 1, ""),
            term("b", 2, "pane %2 is outside the app's grant"),
        ];
        root.children = vec![
            Node::new(
                "logo",
                Kind::Image {
                    src: "img/logo.png".into(),
                    path: "/pkg/img/logo.png".into(),
                    width: 64,
                    height: 32,
                    refused: String::new(),
                },
            ),
            row,
        ];
        let tree = Tree {
            title: "E".into(),
            status: String::new(),
            focus: None,
            root,
        };
        let mut embeds = Embeds::default();
        // More rows than fit: those up to the cursor's are shown.
        embeds.panes.insert(
            1,
            Screen::Lines {
                lines: (0..20).map(|i| format!("line {i}")).collect(),
                cursor_y: 12,
            },
        );
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 8)).expect("terminal");
        t.draw(|f| draw_tree(&tree, &embeds, &mut ViewState::default(), f))
            .expect("draw");
        let buf = t.backend().buffer().clone();
        let shown = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("[image: logo.png 64x32]"), "{shown}");
        assert!(shown.contains("line 12"), "{shown}");
        assert!(!shown.contains("line 13"), "{shown}");
        assert!(shown.contains("refused: pane %2 is outside"), "{shown}");
    }
}
