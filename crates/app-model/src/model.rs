//! What an app shows, as data: its rows, the selection, a form's values,
//! the connection, and the last outcome. Each presentation (the pane app's
//! Ratatui view, the desktop's native panel) draws this same model.

use serde_json::Value;

use crate::manifest::{FieldKind, Manifest, field};

/// The connection to the daemon, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Live,
    /// Since when (unix ms), and why.
    Stale(u64, String),
}

#[derive(Debug, Clone)]
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

    /// The status line: the connection, the keys, and the last outcome.
    pub fn status_text(&self) -> String {
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
        s
    }

    pub fn move_by(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, last) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let m = Manifest::parse(
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
