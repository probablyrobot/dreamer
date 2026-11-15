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
