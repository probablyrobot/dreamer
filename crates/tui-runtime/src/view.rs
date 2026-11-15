//! Drawing an app's model with Ratatui, as a terminal app in a pane.

use app_model::manifest::{FieldKind, ViewKind, field};
use app_model::{App, text};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Wrap,
};
use serde_json::Value;

pub fn draw(app: &App, f: &mut Frame) {
    let [main, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let title = if app.version > 1 {
        format!(" {} (reloaded {}) ", app.manifest.title, app.version - 1)
    } else {
        format!(" {} ", app.manifest.title)
    };
    let block = Block::bordered().title(title);
    let v = &app.manifest.view;
    let highlight = Style::default().add_modifier(Modifier::REVERSED);
    match v.kind {
        ViewKind::Agent => draw_agent(app, f, main, block),
        ViewKind::Table => {
            let header =
                Row::new(v.columns.clone()).style(Style::default().add_modifier(Modifier::BOLD));
            let rows = app.rows.iter().map(|r| {
                Row::new(
                    v.columns
                        .iter()
                        .map(|c| field(r, c).map(text).unwrap_or_default())
                        .collect::<Vec<_>>(),
                )
            });
            let widths = vec![Constraint::Fill(1); v.columns.len()];
            let table = Table::new(rows, widths)
                .header(header)
                .block(block)
                .row_highlight_style(highlight);
            let mut state = TableState::default().with_selected(Some(app.selected));
            f.render_stateful_widget(table, main, &mut state);
        }
        ViewKind::List | ViewKind::Log => {
            let items: Vec<ListItem> = app
                .rows
                .iter()
                .map(|r| {
                    ListItem::new(match &v.field {
                        Some(p) => field(r, p).map(text).unwrap_or_default(),
                        None => text(r),
                    })
                })
                .collect();
            let list = List::new(items).block(block).highlight_style(highlight);
            let selected = if v.kind == ViewKind::Log {
                // A log follows its newest line.
                app.rows.len().checked_sub(1)
            } else {
                Some(app.selected)
            };
            let mut state = ListState::default().with_selected(selected);
            f.render_stateful_widget(list, main, &mut state);
        }
        ViewKind::Gauge => {
            let row = app.rows.first();
            let value = row
                .and_then(|r| match &v.field {
                    Some(p) => field(r, p).and_then(Value::as_f64),
                    None => r.as_f64(),
                })
                .unwrap_or(0.0);
            let ratio = if value > 1.0 { value / 100.0 } else { value }.clamp(0.0, 1.0);
            let label = row
                .and_then(|r| v.label.as_deref().and_then(|p| field(r, p)))
                .map(text)
                .unwrap_or_else(|| format!("{:.0}%", ratio * 100.0));
            f.render_widget(
                Gauge::default().block(block).ratio(ratio).label(label),
                main,
            );
        }
        ViewKind::Chart => {
            let data: Vec<u64> = app
                .rows
                .iter()
                .map(|r| {
                    match &v.field {
                        Some(p) => field(r, p).and_then(Value::as_f64),
                        None => r.as_f64(),
                    }
                    .unwrap_or(0.0)
                    .max(0.0)
                    .round() as u64
                })
                .collect();
            f.render_widget(Sparkline::default().block(block).data(&data), main);
        }
        ViewKind::Form => {
            let lines: Vec<Line> = v
                .fields
                .iter()
                .zip(&app.form)
                .enumerate()
                .map(|(i, (field, value))| {
                    let label = field.label.clone().unwrap_or_else(|| field.name.clone());
                    let shown = match field.kind {
                        FieldKind::Toggle if value == "true" => "[x]".to_owned(),
                        FieldKind::Toggle => "[ ]".to_owned(),
                        _ => value.clone(),
                    };
                    let star = if field.required { "*" } else { "" };
                    let line = Line::from(format!("{label}{star}: {shown}"));
                    if i == app.focus {
                        line.style(highlight)
                    } else {
                        line
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(lines).block(block), main);
        }
        ViewKind::Text => {
            let body = app.rows.first().map(|r| match &v.field {
                Some(p) => field(r, p).map(text).unwrap_or_default(),
                None => text(r),
            });
            f.render_widget(
                Paragraph::new(body.unwrap_or_default())
                    .block(block)
                    .wrap(Wrap { trim: false }),
                main,
            );
        }
    }
    f.render_widget(Paragraph::new(Line::from(app.status_text())), status);
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
}
