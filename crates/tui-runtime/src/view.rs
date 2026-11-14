//! What the app shows, and drawing it with Ratatui. Pure state: the
//! runtime feeds it data and keys, and draws it when something changed.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Wrap,
};
use serde_json::Value;

use crate::manifest::{FieldKind, Manifest, ViewKind, field};

/// The connection to the daemon, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Live,
    /// Since when (unix ms), and why.
    Stale(u64, String),
}

#[derive(Debug)]
pub struct App {
    pub manifest: Manifest,
    /// The view's rows (or, for a text view, the one value).
    pub rows: Vec<Value>,
    pub selected: usize,
    pub link: Link,
    /// The last command's outcome, or a fetch problem.
    pub status: Option<String>,
    pub updated_unix_ms: Option<u64>,
    /// A form's values, one per field, and the focused field.
    pub form: Vec<String>,
    pub focus: usize,
    /// How many times the package was reloaded.
    pub version: u32,
}

/// A value as one line of text.
pub fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl App {
    pub fn new(manifest: Manifest) -> App {
        App {
            manifest,
            rows: Vec::new(),
            selected: 0,
            link: Link::Connecting,
            status: None,
            updated_unix_ms: None,
            form: Vec::new(),
            focus: 0,
            version: 1,
        }
        .with_form()
    }

    /// Form values for the manifest's fields, kept by name across a
    /// reload.
    fn with_form(mut self) -> App {
        self.reset_form(&[]);
        self
    }

    pub fn reset_form(&mut self, old: &[(String, String)]) {
        self.form = self
            .manifest
            .view
            .fields
            .iter()
            .map(|f| {
                old.iter()
                    .find(|(n, _)| *n == f.name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| {
                        if f.kind == FieldKind::Toggle {
                            "false".into()
                        } else {
                            String::new()
                        }
                    })
            })
            .collect();
        self.focus = self.focus.min(self.form.len().saturating_sub(1));
    }

    /// The form's values by name, to keep across a reload.
    pub fn form_values(&self) -> Vec<(String, String)> {
        self.manifest
            .view
            .fields
            .iter()
            .zip(&self.form)
            .map(|(f, v)| (f.name.clone(), v.clone()))
            .collect()
    }

    /// The form as a record, with typed values; or what is wrong with it.
    pub fn form_json(&self) -> Result<Value, String> {
        let mut out = serde_json::Map::new();
        for (f, v) in self.manifest.view.fields.iter().zip(&self.form) {
            let label = f.label.clone().unwrap_or_else(|| f.name.clone());
            if f.required && v.is_empty() {
                return Err(format!("{label} is required"));
            }
            let value = match f.kind {
                FieldKind::Text => Value::String(v.clone()),
                FieldKind::Toggle => Value::Bool(v == "true"),
                FieldKind::Number if v.is_empty() => Value::Null,
                FieldKind::Number => v
                    .parse::<i64>()
                    .map(Value::from)
                    .or_else(|_| v.parse::<f64>().map(Value::from))
                    .map_err(|_| format!("{label} is not a number"))?,
            };
            out.insert(f.name.clone(), value);
        }
        Ok(Value::Object(out))
    }

    /// A key typed into the focused form field.
    pub fn form_key(&mut self, key: &str) {
        let Some(f) = self.manifest.view.fields.get(self.focus).cloned() else {
            return;
        };
        let Some(v) = self.form.get_mut(self.focus) else {
            return;
        };
        match (f.kind, key) {
            (_, "Tab" | "Down") => self.focus = (self.focus + 1) % self.form.len().max(1),
            (_, "Up") => {
                self.focus = (self.focus + self.form.len().max(1) - 1) % self.form.len().max(1);
            }
            (FieldKind::Toggle, " " | "Space") => {
                *v = if v == "true" {
                    "false".into()
                } else {
                    "true".into()
                };
            }
            (FieldKind::Toggle, _) => {}
            (_, "Backspace") => {
                v.pop();
            }
            (FieldKind::Number, k) if k.chars().count() == 1 => {
                if k.chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                {
                    v.push_str(k);
                }
            }
            (FieldKind::Text, k) if k.chars().count() == 1 => v.push_str(k),
            _ => {}
        }
    }

    /// New data for the view's source: rows, keeping the selection on the
    /// same row (by `row_key`) when it is still there.
    pub fn set_rows(&mut self, rows: Vec<Value>, now_ms: u64) {
        let key = self.manifest.view.row_key.clone();
        let current = key.as_deref().and_then(|k| {
            self.rows
                .get(self.selected)
                .and_then(|r| field(r, k))
                .cloned()
        });
        self.rows = rows;
        self.updated_unix_ms = Some(now_ms);
        if let (Some(k), Some(cur)) = (key.as_deref(), current)
            && let Some(i) = self.rows.iter().position(|r| field(r, k) == Some(&cur))
        {
            self.selected = i;
        }
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    pub fn selected_row(&self) -> Option<&Value> {
        self.rows.get(self.selected)
    }

    pub fn move_by(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, last) as usize;
    }

    fn status_line(&self) -> Line<'static> {
        let link = match &self.link {
            Link::Connecting => "connecting".to_owned(),
            Link::Live => "live".to_owned(),
            Link::Stale(_, why) => format!("stale: {why}"),
        };
        let keys: Vec<String> = self
            .manifest
            .commands
            .iter()
            .map(|(name, c)| format!("{} {}", c.key, c.label.as_deref().unwrap_or(name)))
            .chain(std::iter::once("q quit".to_owned()))
            .collect();
        let mut s = format!("[{link}] {}", keys.join("  "));
        if let Some(st) = &self.status {
            s.push_str("  | ");
            s.push_str(st);
        }
        Line::from(s)
    }

    pub fn draw(&self, f: &mut Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let title = if self.version > 1 {
            format!(" {} (reloaded {}) ", self.manifest.title, self.version - 1)
        } else {
            format!(" {} ", self.manifest.title)
        };
        let block = Block::bordered().title(title);
        let v = &self.manifest.view;
        let highlight = Style::default().add_modifier(Modifier::REVERSED);
        match v.kind {
            ViewKind::Table => {
                let header = Row::new(v.columns.clone())
                    .style(Style::default().add_modifier(Modifier::BOLD));
                let rows = self.rows.iter().map(|r| {
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
                let mut state = TableState::default().with_selected(Some(self.selected));
                f.render_stateful_widget(table, main, &mut state);
            }
            ViewKind::List | ViewKind::Log => {
                let items: Vec<ListItem> = self
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
                    self.rows.len().checked_sub(1)
                } else {
                    Some(self.selected)
                };
                let mut state = ListState::default().with_selected(selected);
                f.render_stateful_widget(list, main, &mut state);
            }
            ViewKind::Gauge => {
                let row = self.rows.first();
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
                let data: Vec<u64> = self
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
                    .zip(&self.form)
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
                        if i == self.focus {
                            line.style(highlight)
                        } else {
                            line
                        }
                    })
                    .collect();
                f.render_widget(Paragraph::new(lines).block(block), main);
            }
            ViewKind::Text => {
                let body = self.rows.first().map(|r| match &v.field {
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
        f.render_widget(Paragraph::new(self.status_line()), status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let m = crate::manifest::Manifest::parse(
            r#"
schema_version = 1
id = "t.app"
version = "1"
title = "T"
[data.s]
method = "tmux.topology"
path = "panes"
[view]
type = "table"
source = "s"
row_key = "id"
columns = ["id"]
"#,
        )
        .expect("manifest");
        App::new(m)
    }

    #[test]
    fn the_selection_follows_its_row_across_updates() {
        let mut a = app();
        a.set_rows(
            vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})],
            0,
        );
        a.move_by(1);
        assert_eq!(a.selected_row(), Some(&serde_json::json!({"id": 2})));
        // A new row arrives in front: the selection stays on id 2.
        a.set_rows(
            vec![
                serde_json::json!({"id": 0}),
                serde_json::json!({"id": 1}),
                serde_json::json!({"id": 2}),
            ],
            1,
        );
        assert_eq!(a.selected, 2);
        // Its row went away: the selection stays in range.
        a.set_rows(vec![serde_json::json!({"id": 0})], 2);
        assert_eq!(a.selected, 0);
        a.move_by(-5);
        assert_eq!(a.selected, 0);
    }

    fn render(m: &str, rows: Vec<serde_json::Value>) -> String {
        let manifest = crate::manifest::Manifest::parse(m).expect("manifest");
        let mut a = App::new(manifest);
        a.link = Link::Live;
        a.set_rows(rows, 0);
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 6)).expect("terminal");
        t.draw(|f| a.draw(f)).expect("draw");
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
