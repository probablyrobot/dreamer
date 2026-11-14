//! What the app shows, and drawing it with Ratatui. Pure state: the
//! runtime feeds it data and keys, and draws it when something changed.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use serde_json::Value;

use crate::manifest::{Manifest, ViewKind, field};

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
        let title = format!(" {} ", self.manifest.title);
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
}
