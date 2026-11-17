//! Drawing an app with Ratatui, as a terminal app in a pane: its view
//! tree (every app but an agentic pane), or an agentic pane's prompt and
//! transcript.

use std::collections::HashMap;

use app_model::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction as Axis, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Tabs,
    Wrap,
};
use view_tree::{Direction, Frame as Tree, Kind, Node};

/// What the terminal keeps per node key between draws, so an update to
/// the tree does not lose it: how far each table, list, and scroll node
/// is scrolled.
#[derive(Debug, Default)]
pub struct ViewState {
    offsets: HashMap<String, usize>,
}

/// Draw `app` once, with nothing kept from an earlier draw.
pub fn draw(app: &App, f: &mut Frame) {
    draw_with(app, &mut ViewState::default(), f);
}

/// Draw `app`, keeping scroll positions in `state` across draws.
pub fn draw_with(app: &App, state: &mut ViewState, f: &mut Frame) {
    match app.frame() {
        Some(tree) => draw_tree(&tree, state, f),
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
/// its status line below.
pub fn draw_tree(tree: &Tree, state: &mut ViewState, f: &mut Frame) {
    let [main, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let block = Block::bordered().title(format!(" {} ", tree.title));
    let inner = block.inner(main);
    f.render_widget(block, main);
    draw_node(&tree.root, tree, state, f, inner);
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
    };
    own.saturating_add(border)
}

/// How a child shares a column: text and gauges take what they need, the
/// rest share what is left.
fn share(n: &Node) -> Constraint {
    match n.kind {
        Kind::Text { .. } | Kind::Gauge { .. } => Constraint::Length(natural(n)),
        _ => Constraint::Fill(1),
    }
}

fn draw_node(n: &Node, tree: &Tree, state: &mut ViewState, f: &mut Frame, area: Rect) {
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
                (
                    Axis::Horizontal,
                    vec![Constraint::Fill(1); n.children.len()],
                )
            };
            let areas = Layout::default()
                .direction(axis)
                .constraints(constraints)
                .split(area);
            for (c, a) in n.children.iter().zip(areas.iter()) {
                draw_node(c, tree, state, f, *a);
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
                draw_node(c, tree, state, f, *a);
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
                draw_node(&page, tree, state, f, body);
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
                draw_node(c, tree, state, f, Rect::new(area.x, y, area.width, h));
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
        term.draw(|f| draw_tree(&changed, &mut state, f))
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
}
