//! What an app shows, as data: its rows, the selection, a form's values,
//! the connection, and the last outcome. Each presentation (the pane app's
//! Ratatui view, the desktop's native panel) draws this same model.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use view_tree::{Frame, Kind, Node};

use crate::manifest::{FieldKind, FormField, Manifest, NodeType, View, ViewKind, field};

/// The connection to the daemon, as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    Connecting,
    Live,
    /// Drawn from the package's fixtures, with no daemon.
    Preview,
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
    /// An agentic pane's prompt and what happened (agent views only).
    pub agent: AgentState,
    /// A tree manifest's state: each data source's rows, each table's or
    /// list's selection (by node key), the focused node, and each tabs
    /// node's page.
    pub sources: BTreeMap<String, Vec<Value>>,
    pub selection: BTreeMap<String, usize>,
    pub focused: Option<String>,
    pub tabs: BTreeMap<String, usize>,
    /// A tree's input components' values by node key: an input's text, a
    /// select's option, a toggle's `true` or `false`.
    pub values: BTreeMap<String, String>,
    /// What the input components did since the host last took them
    /// ([`App::take_events`]), oldest first.
    pub events: Vec<Event>,
    /// A tree's transcripts and composers: their own state, and what they
    /// ask of the daemon ([`App::take_asks`]).
    pub sessions: crate::session::SessionNodes,
    /// What `terminal` and `image` nodes show: the panes' screens, the
    /// grant, and the package directory.
    pub embeds: crate::embeds::Embeds,
}

/// Something an input component did: a button pressed, an input's text
/// changed or submitted (Enter), a select's option or a toggle changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The node's key.
    pub node: String,
    #[serde(rename = "type")]
    pub kind: EventKind,
    /// The value after the event: text for an input or a select, a
    /// boolean for a toggle, null for a button.
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Press,
    Change,
    Submit,
}

/// Who said or did a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Said {
    /// The prompt typed.
    You,
    /// The model's text.
    Agent,
    /// A tool the model called, with its input.
    Tool,
    /// A tool's result.
    Result { ok: bool },
    /// The runtime: usage, a scripted answer, a mode change.
    Note,
    /// A run that ended without an answer, and why.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub who: Said,
    pub text: String,
}

/// Entries kept at most; the oldest go first.
pub const TRANSCRIPT: usize = 500;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    /// The prompt being typed.
    pub input: String,
    pub transcript: Vec<Entry>,
    /// A run is in progress (Esc cancels it).
    pub running: bool,
    /// The provider in use, or why there is none (no-model mode).
    pub provider: Option<String>,
    pub mode: String,
    /// What the agent may do, as shown in the header.
    pub grant: String,
    /// An operator's plan for the current request: what it said before its
    /// first action.
    pub plan: Option<String>,
    /// What an operator did, newest last, with each action's or job's
    /// state as last read.
    pub taken: Vec<Taken>,
    /// Tool calls of the current run awaiting their results: (id, name,
    /// input).
    #[serde(skip)]
    pub calls: Vec<(String, String, Value)>,
    /// A filter pane's filters (those it may manage), by name.
    pub filters: Vec<String>,
}

/// One thing an operator did: an action or job it started, or a tool call
/// that failed, with its target and its state as last read (so
/// errors name the action and its actual target).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Taken {
    /// The tool, as the model called it (`create_pane`).
    pub tool: String,
    /// What it was aimed at, from the call: panes, a window, a program.
    pub target: String,
    /// The host it ran on, if not this one.
    pub host: Option<String>,
    pub record: Record,
    /// The state as last read, with what was observed or why it failed.
    pub state: String,
    /// Still in progress: read again until it settles.
    pub open: bool,
    pub ok: bool,
}

/// What a taken step can be followed by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Record {
    Action(String),
    Job(String),
    None,
}

impl AgentState {
    /// Add an entry; text from the model joins the entry it continues.
    pub fn say(&mut self, who: Said, text: &str) {
        if who == Said::Agent
            && let Some(last) = self.transcript.last_mut()
            && last.who == Said::Agent
        {
            last.text.push_str(text);
            return;
        }
        self.transcript.push(Entry {
            who,
            text: text.to_owned(),
        });
        if self.transcript.len() > TRANSCRIPT {
            let extra = self.transcript.len() - TRANSCRIPT;
            self.transcript.drain(..extra);
        }
    }
}

/// Text as an input takes it: a line break (`\r\n`, `\r`, or `\n`) or a
/// tab becomes one space, and other control characters are dropped.
pub fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
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
        let embeds = crate::embeds::Embeds {
            site: Some(crate::embeds::Site::of(&manifest)),
            ..crate::embeds::Embeds::default()
        };
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
            agent: AgentState::default(),
            sources: BTreeMap::new(),
            selection: BTreeMap::new(),
            focused: None,
            tabs: BTreeMap::new(),
            values: BTreeMap::new(),
            events: Vec::new(),
            sessions: crate::session::SessionNodes::default(),
            embeds,
        }
        .with_form()
        .with_focus()
    }

    /// A tree's focus on its first table, list, or form shown.
    fn with_focus(mut self) -> App {
        self.keep_focus();
        self
    }

    /// The form's fields: a version 1 form's, or a tree's form node's.
    fn form_fields(&self) -> Vec<FormField> {
        self.manifest
            .form_view()
            .map(|v| v.fields)
            .unwrap_or_default()
    }

    /// Form values for the manifest's fields, kept by name across a
    /// reload.
    fn with_form(mut self) -> App {
        self.reset_form(&[]);
        self
    }

    /// Form values for the manifest's fields, kept by name from `old`;
    /// and the input components' values, kept by node key.
    pub fn reset_form(&mut self, old: &[(String, String)]) {
        self.reset_values();
        self.form = self
            .form_fields()
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
        self.form_fields()
            .iter()
            .zip(&self.form)
            .map(|(f, v)| (f.name.clone(), v.clone()))
            .collect()
    }

    /// The form as a record, with typed values; or what is wrong with it.
    pub fn form_json(&self) -> Result<Value, String> {
        let mut out = serde_json::Map::new();
        for (f, v) in self.form_fields().iter().zip(&self.form) {
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
        let Some(f) = self.form_fields().get(self.focus).cloned() else {
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
        if !self.manifest.is_tree() {
            return self.rows.get(self.selected);
        }
        let key = self.focused.as_deref()?;
        let v = self.manifest.nodes.get(key)?.view()?;
        self.sources
            .get(&v.source)?
            .get(self.selection.get(key).copied().unwrap_or(0))
    }

    /// The status line: the connection, the keys, and the last outcome.
    pub fn status_text(&self) -> String {
        let link = match &self.link {
            Link::Connecting => "connecting".to_owned(),
            Link::Live => "live".to_owned(),
            Link::Preview => "preview from fixtures".to_owned(),
            Link::Stale(_, why) => format!("stale: {why}"),
        };
        if self.manifest.view.kind == crate::manifest::ViewKind::Agent {
            let mut s = format!("[{link}] Enter run  Esc cancel  Ctrl-c quit");
            if let Some(st) = &self.status {
                s.push_str("  | ");
                s.push_str(st);
            }
            return s;
        }
        let keys: Vec<String> = self
            .manifest
            .commands
            .iter()
            .filter(|(_, c)| !c.key.is_empty())
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

    /// Whether typed or pasted text has somewhere to go: the focused form
    /// field (a version 1 form's, or a tree's focused form node's) takes
    /// text or a number, or the view is an agent's prompt.
    pub fn takes_text(&self) -> bool {
        if !self.manifest.is_tree() && self.manifest.view.kind == ViewKind::Agent {
            return true;
        }
        if self.in_input() || self.in_composer() {
            return true;
        }
        self.in_form()
            && self
                .form_fields()
                .get(self.focus)
                .is_some_and(|f| f.kind != FieldKind::Toggle)
    }

    /// Text typed (committed by the platform's text input) or pasted into
    /// the focused input, as one edit: line breaks become spaces, other
    /// control characters are dropped, and a number field keeps only what
    /// a number may hold. A view with no input takes none, so pasted text
    /// never runs a command. Returns whether the text was taken.
    pub fn insert_text(&mut self, text: &str) -> bool {
        if !self.takes_text() {
            return false;
        }
        if self.composer_insert(text) {
            return true;
        }
        let clean = clean_text(text);
        if !self.manifest.is_tree() && self.manifest.view.kind == ViewKind::Agent {
            self.agent.input.push_str(&clean);
            return true;
        }
        if self.in_input() {
            self.edit_input(|v| v.push_str(&clean));
            return true;
        }
        let number = self
            .form_fields()
            .get(self.focus)
            .is_some_and(|f| f.kind == FieldKind::Number);
        let Some(v) = self.form.get_mut(self.focus) else {
            return false;
        };
        if number {
            v.extend(
                clean
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-'),
            );
        } else {
            v.push_str(&clean);
        }
        true
    }

    /// In a tree, a click or wheel steps on the node `node` focus it first
    /// when it is a table, list, or form shown; an unknown or hidden node
    /// leaves the focus where it is.
    fn focus_node(&mut self, node: Option<&str>) {
        if let Some(k) = node
            && self.manifest.is_tree()
            && self.focusable().iter().any(|f| f == k)
        {
            self.focused = Some(k.to_owned());
        }
    }

    /// The kind of the data view that takes clicks and the wheel: a
    /// version 1 view's, or a tree's focused node's.
    fn focused_view(&self) -> Option<View> {
        if !self.manifest.is_tree() {
            return Some(self.manifest.view.clone());
        }
        let k = self.focused.as_deref()?;
        self.manifest.nodes.get(k)?.view()
    }

    /// A click on the `index`th row of a table or list selects it; on a
    /// form's field, focuses it. Other views have nothing to select. In a
    /// tree, `node` names the node clicked (else the focused one).
    pub fn select_at(&mut self, node: Option<&str>, index: usize) {
        self.focus_node(node);
        if self.click_control(index) || self.session_click(index) {
            return;
        }
        let Some(v) = self.focused_view() else {
            return;
        };
        match v.kind {
            ViewKind::Table | ViewKind::List => {
                if !self.manifest.is_tree() {
                    if !self.rows.is_empty() {
                        self.selected = index.min(self.rows.len() - 1);
                    }
                    return;
                }
                let n = self.sources.get(&v.source).map_or(0, Vec::len);
                if let (Some(k), true) = (self.focused.clone(), n > 0) {
                    self.selection.insert(k, index.min(n - 1));
                }
            }
            ViewKind::Form if !self.form.is_empty() => {
                self.focus = index.min(self.form.len() - 1);
            }
            _ => {}
        }
    }

    /// Wheel steps move a table's or list's selection (positive down). In
    /// a tree, `node` names the node under the pointer (else the focused
    /// one).
    pub fn scroll(&mut self, node: Option<&str>, steps: i64) {
        self.focus_node(node);
        if self
            .focused_view()
            .is_some_and(|v| matches!(v.kind, ViewKind::Table | ViewKind::List))
        {
            self.move_by(steps);
        }
    }

    pub fn move_by(&mut self, delta: i64) {
        if self.manifest.is_tree() {
            self.move_focused(delta);
            return;
        }
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, last) as usize;
    }
}

/// A tree manifest's state, and the view as a tree.
impl App {
    /// Keys go to a form: a version 1 form, or a tree's focused form node.
    pub fn in_form(&self) -> bool {
        if !self.manifest.is_tree() {
            return self.manifest.view.kind == ViewKind::Form;
        }
        self.focused_type() == Some(NodeType::Form)
    }

    fn focused_type(&self) -> Option<NodeType> {
        let k = self.focused.as_deref()?;
        self.manifest.nodes.get(k).map(|n| n.kind)
    }

    /// The command Enter runs in the form.
    pub fn form_submit(&self) -> Option<String> {
        self.manifest.form_view().and_then(|v| v.submit)
    }

    /// The tables, lists, and forms shown (a tabs node shows its active
    /// page only), in tree order.
    pub fn focusable(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack: Vec<&str> = self.manifest.root.as_deref().into_iter().collect();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(k) = stack.pop() {
            let Some(n) = self.manifest.nodes.get(k) else {
                continue;
            };
            if !seen.insert(k) {
                continue;
            }
            if n.kind.focusable() {
                out.push(k.to_owned());
            }
            if n.kind == NodeType::Tabs {
                let active = self.tabs.get(k).copied().unwrap_or(0);
                stack.extend(n.children.get(active).map(String::as_str));
            } else {
                stack.extend(n.children.iter().rev().map(String::as_str));
            }
        }
        out
    }

    /// Keep the focus on a node that is shown.
    fn keep_focus(&mut self) {
        if !self.manifest.is_tree() {
            return;
        }
        let shown = self.focusable();
        if self.focused.as_ref().is_none_or(|f| !shown.contains(f)) {
            self.focused = shown.into_iter().next();
        }
    }

    /// Tab: the next table, list, or form shown.
    pub fn focus_next(&mut self) {
        let shown = self.focusable();
        let at = self
            .focused
            .as_ref()
            .and_then(|f| shown.iter().position(|k| k == f));
        self.focused = match at {
            Some(i) => shown.get((i + 1) % shown.len()).cloned(),
            None => shown.first().cloned(),
        };
    }

    /// Left or Right: the page of the tabs node holding the focus (or the
    /// first tabs node) moves by `delta`.
    pub fn switch_tab(&mut self, delta: i64) {
        let order = self.manifest.tree_order();
        let holding = |tabs: &str| {
            let Some(f) = self.focused.as_deref() else {
                return false;
            };
            let mut stack = vec![tabs];
            while let Some(k) = stack.pop() {
                if k == f {
                    return true;
                }
                if let Some(n) = self.manifest.nodes.get(k) {
                    stack.extend(n.children.iter().map(String::as_str));
                }
            }
            false
        };
        let tabs: Vec<&String> = order
            .iter()
            .filter(|k| {
                self.manifest
                    .nodes
                    .get(*k)
                    .is_some_and(|n| n.kind == NodeType::Tabs)
            })
            .collect();
        // The innermost tabs node holding the focus comes last in tree
        // order among those that hold it.
        let Some(key) = tabs
            .iter()
            .rev()
            .find(|k| holding(k))
            .or_else(|| tabs.first())
            .map(|k| (*k).clone())
        else {
            return;
        };
        let pages = self.manifest.nodes[&key].children.len() as i64;
        if pages == 0 {
            return;
        }
        let now = self.tabs.get(&key).copied().unwrap_or(0) as i64;
        self.tabs
            .insert(key, (now + delta).rem_euclid(pages) as usize);
        self.focused = None;
        self.keep_focus();
    }

    fn move_focused(&mut self, delta: i64) {
        let Some(key) = self.focused.clone() else {
            return;
        };
        let Some(v) = self.manifest.nodes.get(&key).and_then(|n| n.view()) else {
            return;
        };
        let n = self.sources.get(&v.source).map_or(0, Vec::len);
        if n == 0 || !matches!(v.kind, ViewKind::Table | ViewKind::List) {
            return;
        }
        let now = self.selection.get(&key).copied().unwrap_or(0) as i64;
        self.selection
            .insert(key, (now + delta).clamp(0, n as i64 - 1) as usize);
    }

    /// New rows for the data source `name`: each node showing it keeps its
    /// selection on the same row (by its `row_key`) when it is still there.
    pub fn set_source(&mut self, name: &str, rows: Vec<Value>, now_ms: u64) {
        if !self.manifest.is_tree() {
            if self.manifest.view.source == name {
                self.set_rows(rows, now_ms);
            }
            return;
        }
        let old = self.sources.get(name).cloned().unwrap_or_default();
        for (key, v) in self.manifest.data_views() {
            if v.source != name {
                continue;
            }
            let at = self.selection.get(&key).copied().unwrap_or(0);
            let current = v
                .row_key
                .as_deref()
                .and_then(|k| old.get(at).and_then(|r| field(r, k)).cloned());
            let mut i = at;
            if let (Some(k), Some(cur)) = (v.row_key.as_deref(), current)
                && let Some(found) = rows.iter().position(|r| field(r, k) == Some(&cur))
            {
                i = found;
            }
            self.selection
                .insert(key, i.min(rows.len().saturating_sub(1)));
        }
        self.sources.insert(name.to_owned(), rows);
        self.updated_unix_ms = Some(now_ms);
    }

    /// The view as a tree of components, for the renderers and to publish;
    /// `None` for an agentic pane, which is drawn on its own. A version 1
    /// manifest is one data node keyed `view`.
    pub fn frame(&self) -> Option<Frame> {
        let title = if self.version > 1 {
            format!("{} (reloaded {})", self.manifest.title, self.version - 1)
        } else {
            self.manifest.title.clone()
        };
        let (root, focus) = if self.manifest.is_tree() {
            let root = self.manifest.root.as_deref()?;
            (self.tree_node(root, 0)?, self.focused.clone())
        } else {
            let v = &self.manifest.view;
            if v.kind == ViewKind::Agent {
                return None;
            }
            let focus = matches!(v.kind, ViewKind::Table | ViewKind::List | ViewKind::Form)
                .then(|| "view".to_owned());
            (self.data_node("view", v, &self.rows, self.selected), focus)
        };
        let mut frame = Frame {
            title,
            status: self.status_text(),
            focus,
            root,
        };
        self.embeds.mark(&mut frame);
        Some(frame)
    }

    fn tree_node(&self, key: &str, depth: usize) -> Option<Node> {
        let spec = self.manifest.nodes.get(key)?;
        // A manifest is validated as a tree; this only bounds a bad one.
        if depth > 64 {
            return None;
        }
        let mut node = if spec.kind.is_content() || spec.kind.is_control() {
            Node::new(key, self.component(key, spec))
        } else if spec.kind.is_session() || spec.tap.is_some() {
            self.session_node(key, spec)
        } else if spec.kind == NodeType::Terminal {
            Node::new(
                key,
                Kind::Terminal {
                    pane: spec.pane.unwrap_or_default(),
                    refused: String::new(),
                },
            )
        } else if spec.kind == NodeType::Image {
            Node::new(
                key,
                Kind::Image {
                    src: spec.src.clone().unwrap_or_default(),
                    path: String::new(),
                    width: 0,
                    height: 0,
                    refused: String::new(),
                },
            )
        } else if spec.kind == NodeType::Web {
            Node::new(
                key,
                Kind::Web {
                    src: spec.src.clone().unwrap_or_default(),
                    page: Box::default(),
                    refused: String::new(),
                },
            )
        } else if let Some(v) = spec.view() {
            let rows = self.sources.get(&v.source).map_or(&[][..], Vec::as_slice);
            let selected = self.selection.get(key).copied().unwrap_or(0);
            self.data_node(key, &v, rows, selected)
        } else {
            let kind = match spec.kind {
                NodeType::Row => Kind::Row,
                NodeType::Split => Kind::Split {
                    direction: spec.direction,
                    sizes: spec.sizes.clone(),
                },
                NodeType::Tabs => Kind::Tabs {
                    active: self.tabs.get(key).copied().unwrap_or(0),
                },
                NodeType::Scroll => Kind::Scroll,
                _ => Kind::Column,
            };
            let mut n = Node::new(key, kind);
            n.children = spec
                .children
                .iter()
                .filter_map(|c| self.tree_node(c, depth + 1))
                .collect();
            n
        };
        node.title.clone_from(&spec.title);
        Some(node)
    }

    /// A data view as a node: rows become `item` children keyed by the
    /// view's `row_key` (else their place), so a changed row is one
    /// changed node.
    fn data_node(&self, key: &str, v: &View, rows: &[Value], selected: usize) -> Node {
        let value = |r: &Value| match &v.field {
            Some(p) => field(r, p).map(text).unwrap_or_default(),
            None => text(r),
        };
        let number = |r: &Value| match &v.field {
            Some(p) => field(r, p).and_then(Value::as_f64),
            None => r.as_f64(),
        };
        let items = |cells: &dyn Fn(&Value) -> Vec<String>| -> Vec<Node> {
            let mut seen = std::collections::HashSet::new();
            rows.iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut k = v
                        .row_key
                        .as_deref()
                        .and_then(|p| field(r, p))
                        .map_or_else(|| i.to_string(), text);
                    if !seen.insert(k.clone()) {
                        k = format!("{k}#{i}");
                        seen.insert(k.clone());
                    }
                    Node::new(k, Kind::Item { cells: cells(r) })
                })
                .collect()
        };
        let mut node = Node::new(key, Kind::Column);
        match v.kind {
            ViewKind::Table => {
                node.children = items(&|r| {
                    v.columns
                        .iter()
                        .map(|c| field(r, c).map(text).unwrap_or_default())
                        .collect()
                });
                node.kind = Kind::Table {
                    columns: v.columns.clone(),
                    selected: node.children.get(selected).map(|c| c.key.clone()),
                };
            }
            ViewKind::List => {
                node.children = items(&|r| vec![value(r)]);
                node.kind = Kind::List {
                    selected: node.children.get(selected).map(|c| c.key.clone()),
                };
            }
            ViewKind::Log => {
                node.children = items(&|r| vec![value(r)]);
                node.kind = Kind::Log;
            }
            ViewKind::Text | ViewKind::Agent => {
                node.kind = Kind::Text {
                    text: rows.first().map(value).unwrap_or_default(),
                };
            }
            ViewKind::Gauge => {
                let row = rows.first();
                let n = row.and_then(number).unwrap_or(0.0);
                let ratio = if n > 1.0 { n / 100.0 } else { n }.clamp(0.0, 1.0);
                let label = row
                    .and_then(|r| v.label.as_deref().and_then(|p| field(r, p)))
                    .map(text)
                    .unwrap_or_else(|| format!("{:.0}%", ratio * 100.0));
                node.kind = Kind::Gauge { ratio, label };
            }
            ViewKind::Chart => {
                node.kind = Kind::Chart {
                    values: rows
                        .iter()
                        .map(|r| number(r).unwrap_or(0.0).max(0.0))
                        .collect(),
                };
            }
            ViewKind::Form => {
                node.kind = Kind::Form {
                    fields: v
                        .fields
                        .iter()
                        .zip(&self.form)
                        .map(|(f, val)| view_tree::Field {
                            label: f.label.clone().unwrap_or_else(|| f.name.clone()),
                            value: val.clone(),
                            toggle: f.kind == FieldKind::Toggle,
                            required: f.required,
                        })
                        .collect(),
                    focus: self.focus,
                };
            }
        }
        node
    }
}

/// A tree's text, input, and status components: what they show, and what
/// the input components do with keys, clicks, and text.
impl App {
    /// The input components' values from the manifest, keeping those by
    /// the same node key from before (a select's only while still an
    /// option).
    pub fn reset_values(&mut self) {
        let old = std::mem::take(&mut self.values);
        for (key, n) in &self.manifest.nodes {
            let kept = old.get(key).cloned();
            let value = match n.kind {
                NodeType::Input => kept.or_else(|| n.value.clone()).unwrap_or_default(),
                NodeType::Select => kept
                    .filter(|v| n.options.contains(v))
                    .or_else(|| n.value.clone())
                    .or_else(|| n.options.first().cloned())
                    .unwrap_or_default(),
                NodeType::Toggle => kept.unwrap_or_else(|| n.on.to_string()),
                NodeType::Composer => kept.unwrap_or_default(),
                _ => continue,
            };
            self.values.insert(key.clone(), value);
        }
    }

    /// The input components' values by node key, typed: an input's and a
    /// select's as text, a toggle's as a boolean. Commands read them as
    /// `${value.<node>}`, handlers as `values`.
    pub fn values_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (key, v) in &self.values {
            out.insert(key.clone(), self.typed_value(key, v));
        }
        Value::Object(out)
    }

    fn typed_value(&self, key: &str, v: &str) -> Value {
        match self.manifest.nodes.get(key).map(|n| n.kind) {
            Some(NodeType::Toggle) => Value::Bool(v == "true"),
            _ => Value::String(v.to_owned()),
        }
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn emit(&mut self, node: &str, kind: EventKind) {
        let value = match self.values.get(node) {
            Some(v) => self.typed_value(node, v),
            None => Value::Null,
        };
        self.events.push(Event {
            node: node.to_owned(),
            kind,
            value,
        });
    }

    /// The command an event runs: a button's press, a select's or
    /// toggle's change (`run`), an input's submit (`submit`).
    pub fn command_for(&self, e: &Event) -> Option<String> {
        let n = self.manifest.nodes.get(&e.node)?;
        match (n.kind, e.kind) {
            (NodeType::Button, EventKind::Press)
            | (NodeType::Select | NodeType::Toggle, EventKind::Change) => n.run.clone(),
            (NodeType::Input, EventKind::Submit) => n.submit.clone(),
            _ => None,
        }
    }

    /// The focused node is an input: keys and text type into it.
    pub fn in_input(&self) -> bool {
        self.focused_type() == Some(NodeType::Input)
    }

    /// Change the focused input's text, and record the change.
    fn edit_input(&mut self, f: impl FnOnce(&mut String)) {
        let Some(key) = self.focused.clone() else {
            return;
        };
        let v = self.values.entry(key.clone()).or_default();
        let before = v.clone();
        f(v);
        if *v != before {
            self.emit(&key, EventKind::Change);
        }
    }

    /// A key for the focused input component. An input takes letters,
    /// digits, Space, and Backspace, and Enter submits it; a button is
    /// pressed with Enter or Space; a toggle flips with Enter or Space; a
    /// select moves with Up and Down, and Enter or Space steps to the
    /// next option. Returns whether the key was taken.
    pub fn control_key(&mut self, name: &str) -> bool {
        if self.session_key(name) {
            return true;
        }
        let Some(key) = self.focused.clone() else {
            return false;
        };
        let Some(kind) = self.focused_type() else {
            return false;
        };
        let activate = matches!(name, "Enter" | " " | "Space");
        match kind {
            NodeType::Input => match name {
                "Enter" => self.emit(&key, EventKind::Submit),
                "Backspace" => self.edit_input(|v| {
                    v.pop();
                }),
                " " | "Space" => self.edit_input(|v| v.push(' ')),
                k if k.chars().count() == 1 && !k.chars().all(char::is_control) => {
                    self.edit_input(|v| v.push_str(k));
                }
                _ => return false,
            },
            NodeType::Button if activate => self.emit(&key, EventKind::Press),
            NodeType::Toggle if activate => self.flip(&key),
            NodeType::Select => {
                let delta = match name {
                    "Up" => -1,
                    "Down" => 1,
                    _ if activate => {
                        let n = self.manifest.nodes[&key].options.len().max(1);
                        let at = self.option_index(&key);
                        self.choose(&key, (at + 1) % n);
                        return true;
                    }
                    _ => return false,
                };
                let at = self.option_index(&key) as i64 + delta;
                self.choose(&key, at.max(0) as usize);
            }
            _ => return false,
        }
        true
    }

    /// A click on the focused input component (`index`: the option of a
    /// select): presses a button, flips a toggle, picks an option. An
    /// input only takes the focus. Whether the focused node is one.
    fn click_control(&mut self, index: usize) -> bool {
        let (Some(key), Some(kind)) = (self.focused.clone(), self.focused_type()) else {
            return false;
        };
        match kind {
            NodeType::Button => self.emit(&key, EventKind::Press),
            NodeType::Toggle => self.flip(&key),
            NodeType::Select => self.choose(&key, index),
            NodeType::Input => {}
            _ => return false,
        }
        true
    }

    fn flip(&mut self, key: &str) {
        let v = self.values.entry(key.to_owned()).or_default();
        *v = if v == "true" { "false" } else { "true" }.to_owned();
        self.emit(key, EventKind::Change);
    }

    fn option_index(&self, key: &str) -> usize {
        let options = self
            .manifest
            .nodes
            .get(key)
            .map_or(&[][..], |n| n.options.as_slice());
        self.values
            .get(key)
            .and_then(|v| options.iter().position(|o| o == v))
            .unwrap_or(0)
    }

    /// Pick option `index` (kept in range) of select `key`; a change is
    /// recorded only when the option changed.
    fn choose(&mut self, key: &str, index: usize) {
        let Some(options) = self.manifest.nodes.get(key).map(|n| n.options.clone()) else {
            return;
        };
        let Some(option) = options.get(index.min(options.len().saturating_sub(1))) else {
            return;
        };
        if self.values.get(key) != Some(option) {
            self.values.insert(key.to_owned(), option.clone());
            self.emit(key, EventKind::Change);
        }
    }

    /// A text, input, or status component as drawn: its text, value, or
    /// items from the manifest, or from its source's rows.
    fn component(&self, key: &str, spec: &crate::manifest::NodeSpec) -> Kind {
        let rows = self
            .sources
            .get(&spec.source)
            .map_or(&[][..], Vec::as_slice);
        let value_of = |r: &Value| match &spec.field {
            Some(p) => field(r, p).map(text).unwrap_or_default(),
            None => text(r),
        };
        let shown = || match (&spec.text, spec.source.is_empty()) {
            (Some(t), true) => t.clone(),
            _ => rows.first().map(value_of).unwrap_or_default(),
        };
        let label = spec.label.clone().unwrap_or_default();
        let value = self.values.get(key).cloned().unwrap_or_default();
        match spec.kind {
            NodeType::Markdown => Kind::Markdown { text: shown() },
            NodeType::Code => Kind::Code {
                text: shown(),
                language: spec.language.clone().unwrap_or_default(),
            },
            NodeType::Diff => Kind::Diff { text: shown() },
            NodeType::Badge => Kind::Badge {
                text: shown(),
                tone: spec.tone,
            },
            NodeType::Progress => {
                let row = rows.first();
                let ratio = match (spec.ratio, row) {
                    (Some(r), _) if spec.source.is_empty() => r,
                    (_, Some(r)) => {
                        let n = match &spec.field {
                            Some(p) => field(r, p).and_then(Value::as_f64),
                            None => r.as_f64(),
                        }
                        .unwrap_or(0.0);
                        if n > 1.0 { n / 100.0 } else { n }
                    }
                    _ => 0.0,
                }
                .clamp(0.0, 1.0);
                let label = row
                    .and_then(|r| spec.label.as_deref().and_then(|p| field(r, p)))
                    .map(text)
                    .or_else(|| spec.text.clone())
                    .unwrap_or_else(|| format!("{:.0}%", ratio * 100.0));
                Kind::Progress { ratio, label }
            }
            NodeType::Status => Kind::Status {
                items: if spec.source.is_empty() {
                    spec.items.clone()
                } else {
                    rows.iter()
                        .map(|r| view_tree::StatusItem {
                            text: value_of(r),
                            tone: view_tree::Tone::Neutral,
                        })
                        .collect()
                },
            },
            NodeType::Button => Kind::Button { label },
            NodeType::Input => Kind::Input {
                label,
                value,
                placeholder: spec.placeholder.clone().unwrap_or_default(),
            },
            NodeType::Select => Kind::Select {
                label,
                options: spec.options.clone(),
                selected: self.option_index(key),
            },
            NodeType::Toggle => Kind::Toggle {
                label,
                on: value == "true",
            },
            _ => Kind::Column,
        }
    }
}

/// The model as an app host publishes it (`apps.publish`): the manifest's
/// text and the state, as JSON. Desktops draw what it holds and run none
/// of the package.
#[derive(Serialize)]
struct WireOut<'a> {
    manifest: &'a str,
    rows: &'a [Value],
    selected: usize,
    link: &'a Link,
    status: &'a Option<String>,
    updated_unix_ms: Option<u64>,
    form: &'a [String],
    focus: usize,
    version: u32,
    agent: &'a AgentState,
}

#[derive(Deserialize)]
struct WireIn {
    manifest: String,
    rows: Vec<Value>,
    selected: usize,
    link: Link,
    status: Option<String>,
    updated_unix_ms: Option<u64>,
    form: Vec<String>,
    focus: usize,
    version: u32,
    agent: AgentState,
}

/// The manifest last read from a published view, kept so an unchanged
/// manifest is not parsed again for every view.
pub type ManifestCache = Option<(String, Manifest)>;

impl App {
    /// This model as published, with the manifest's text `source`.
    pub fn to_published(&self, source: &str) -> String {
        serde_json::to_string(&WireOut {
            manifest: source,
            rows: &self.rows,
            selected: self.selected,
            link: &self.link,
            status: &self.status,
            updated_unix_ms: self.updated_unix_ms,
            form: &self.form,
            focus: self.focus,
            version: self.version,
            agent: &self.agent,
        })
        .unwrap_or_default()
    }

    /// A model from a published view.
    pub fn from_published(view: &str, cache: &mut ManifestCache) -> Result<App, String> {
        let w: WireIn = serde_json::from_str(view).map_err(|e| e.to_string())?;
        let manifest = match cache {
            Some((text, m)) if *text == w.manifest => m.clone(),
            _ => {
                let m = Manifest::parse(&w.manifest).map_err(|e| e.join("; "))?;
                *cache = Some((w.manifest, m.clone()));
                m
            }
        };
        let embeds = crate::embeds::Embeds {
            site: Some(crate::embeds::Site::of(&manifest)),
            ..crate::embeds::Embeds::default()
        };
        Ok(App {
            manifest,
            rows: w.rows,
            selected: w.selected,
            link: w.link,
            status: w.status,
            updated_unix_ms: w.updated_unix_ms,
            form: w.form,
            focus: w.focus,
            version: w.version,
            agent: w.agent,
            sources: BTreeMap::new(),
            selection: BTreeMap::new(),
            focused: None,
            tabs: BTreeMap::new(),
            values: BTreeMap::new(),
            events: Vec::new(),
            sessions: crate::session::SessionNodes::default(),
            embeds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_model_reads_back_the_same() {
        let src = r#"
schema_version = 1
id = "t.form"
version = "1"
title = "F"
module = "app.luau"
[view]
type = "form"
source = "none"
submit = "send"
fields = [{ name = "name", kind = "text" }, { name = "loud", kind = "toggle" }]
[commands.send]
key = "Enter"
handler = "send"
"#;
        let mut a = App::new(Manifest::parse(src).expect("manifest"));
        a.form_key("x");
        a.form_key("Tab");
        a.form_key("Space");
        a.link = Link::Stale(5, "gone".into());
        a.agent.say(Said::Agent, "hi");
        let mut cache = None;
        let b = App::from_published(&a.to_published(src), &mut cache).expect("read back");
        assert_eq!(b.form, ["x", "true"]);
        assert_eq!(b.focus, 1);
        assert_eq!(b.link, a.link);
        assert_eq!(b.agent.transcript, a.agent.transcript);
        assert_eq!(b.manifest.title, "F");
        assert!(cache.is_some());
    }

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

    fn form() -> App {
        App::new(
            Manifest::parse(
                r#"
schema_version = 1
id = "t.form"
version = "1"
title = "F"
[view]
type = "form"
source = "none"
submit = "send"
fields = [{ name = "name", kind = "text" }, { name = "n", kind = "number" }, { name = "on", kind = "toggle" }]
[commands.send]
key = "Enter"
method = "actions.submit"
"#,
            )
            .expect("manifest"),
        )
    }

    #[test]
    fn text_goes_into_the_focused_input_as_one_edit() {
        let mut f = form();
        assert!(f.insert_text("two\r\nlines\u{7}"));
        assert_eq!(f.form[0], "two lines");
        f.select_at(None, 1);
        assert!(f.insert_text("-1.5e3x"));
        assert_eq!(f.form[1], "-1.53");
        // A toggle is no text input: nothing is typed into it.
        f.select_at(None, 9);
        assert_eq!(f.focus, 2);
        assert!(!f.takes_text());
        assert!(!f.insert_text("true"));
        assert_eq!(f.form[2], "false");
    }

    #[test]
    fn a_view_with_no_input_ignores_text_so_a_paste_runs_nothing() {
        let mut a = app();
        a.set_rows(vec![serde_json::json!({"id": 1})], 0);
        assert!(!a.insert_text("q"));
        assert_eq!(a.rows.len(), 1);
    }

    #[test]
    fn clicks_select_rows_and_the_wheel_moves_the_selection() {
        let mut a = app();
        a.set_rows((0..5).map(|i| serde_json::json!({"id": i})).collect(), 0);
        a.select_at(None, 3);
        assert_eq!(a.selected, 3);
        a.select_at(None, 40);
        assert_eq!(a.selected, 4);
        a.scroll(None, -2);
        assert_eq!(a.selected, 2);
        a.scroll(None, 9);
        assert_eq!(a.selected, 4);
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

    /// A nested tree: a table beside tabs holding a list and a gauge.
    pub(crate) const TREE: &str = r#"
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

[data.status]
method = "daemon.status"

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
type = "tabs"
children = ["names", "load"]

[nodes.names]
type = "list"
title = "Windows"
source = "windows"
row_key = "id"
field = "name"

[nodes.load]
type = "gauge"
title = "Load"
source = "status"
field = "load"
"#;

    fn tree_app() -> App {
        let mut a = App::new(Manifest::parse(TREE).expect("tree manifest"));
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
            vec![serde_json::json!({"id": 7, "name": "build"})],
            0,
        );
        a.set_source("status", vec![serde_json::json!({"load": 0.5})], 0);
        a
    }

    #[test]
    fn a_tree_manifest_becomes_a_keyed_tree() {
        let a = tree_app();
        let f = a.frame().expect("a tree");
        assert_eq!(
            f.outline(),
            "split main\n  table left (2 rows)\n  tabs side\n    list names (1 rows)\n    gauge load\n"
        );
        assert_eq!(f.focus.as_deref(), Some("left"));
        let left = f.root.find("left").expect("left");
        assert_eq!(left.children[1].key, "2");
        assert_eq!(left.children[1].cells(), ["2", "vim"]);
    }

    #[test]
    fn an_update_to_one_row_is_a_patch_to_that_row_only() {
        let mut a = tree_app();
        let before = a.frame().expect("tree");
        a.set_source(
            "panes",
            vec![
                serde_json::json!({"id": 1, "cmd": "sh"}),
                serde_json::json!({"id": 2, "cmd": "nvim"}),
            ],
            1,
        );
        let after = a.frame().expect("tree");
        let ops = view_tree::diff(&before, &after);
        assert_eq!(
            ops,
            vec![view_tree::Op::Props {
                path: vec!["main".into(), "left".into(), "2".into()],
                title: None,
                kind: view_tree::Kind::Item {
                    cells: vec!["2".into(), "nvim".into()],
                },
                bind: None,
            }]
        );
        let mut applied = before.clone();
        view_tree::apply(&mut applied, &ops).expect("apply");
        assert_eq!(applied, after);
    }

    #[test]
    fn focus_selection_and_tabs_survive_updates() {
        let mut a = tree_app();
        a.move_by(1);
        assert_eq!(
            a.selected_row(),
            Some(&serde_json::json!({"id": 2, "cmd": "vim"}))
        );
        // A row arrives in front: the selection stays on id 2.
        a.set_source(
            "panes",
            vec![
                serde_json::json!({"id": 0, "cmd": "top"}),
                serde_json::json!({"id": 1, "cmd": "sh"}),
                serde_json::json!({"id": 2, "cmd": "vim"}),
            ],
            1,
        );
        let f = a.frame().expect("tree");
        assert!(matches!(
            &f.root.find("left").expect("left").kind,
            view_tree::Kind::Table { selected: Some(k), .. } if k == "2"
        ));
        // Tab moves the focus to the list on the shown tab, and back.
        a.focus_next();
        assert_eq!(a.focused.as_deref(), Some("names"));
        assert_eq!(
            a.selected_row(),
            Some(&serde_json::json!({"id": 7, "name": "build"}))
        );
        // The next tab holds no list: the focus moves to what is shown.
        a.switch_tab(1);
        assert_eq!(a.tabs.get("side"), Some(&1));
        assert_eq!(a.focused.as_deref(), Some("left"));
        a.set_source("status", vec![serde_json::json!({"load": 0.9})], 2);
        assert_eq!(a.focused.as_deref(), Some("left"));
        assert_eq!(a.tabs.get("side"), Some(&1));
    }

    #[test]
    fn clicks_and_the_wheel_reach_the_node_they_name_in_a_tree() {
        let mut a = tree_app();
        // A click on the list's row focuses the list and selects that row.
        a.select_at(Some("names"), 5);
        assert_eq!(a.focused.as_deref(), Some("names"));
        assert_eq!(
            a.selected_row(),
            Some(&serde_json::json!({"id": 7, "name": "build"}))
        );
        // Wheel steps over the table move its selection, not the list's.
        a.scroll(Some("left"), 3);
        assert_eq!(a.focused.as_deref(), Some("left"));
        assert_eq!(a.selection.get("left"), Some(&1));
        assert_eq!(a.selection.get("names"), Some(&0));
        // A node that is not shown or takes no focus changes nothing.
        a.select_at(Some("load"), 0);
        assert_eq!(a.focused.as_deref(), Some("left"));
        assert_eq!(a.selection.get("left"), Some(&0));
        // No form is focused: text has nowhere to go.
        assert!(!a.insert_text("q"));
    }

    #[test]
    fn a_version_1_manifest_is_one_node_keyed_view() {
        let mut a = app();
        a.set_rows(
            vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})],
            0,
        );
        let f = a.frame().expect("tree");
        assert_eq!(f.outline(), "table view (2 rows)\n");
        assert_eq!(f.title, "T");
        assert_eq!(f.status, a.status_text());
    }

    /// The reference app: every text, input, and status component.
    pub(crate) const COMPONENTS: &str = include_str!("../../../examples/apps/components/app.toml");

    fn components() -> App {
        App::new(Manifest::parse(COMPONENTS).expect("the reference app"))
    }

    #[test]
    fn the_reference_app_has_every_component() {
        let a = components();
        let f = a.frame().expect("a tree");
        for kind in [
            "markdown", "code", "diff", "button", "input", "select", "toggle", "badge", "progress",
            "status",
        ] {
            assert!(
                f.outline().lines().any(|l| l.trim().starts_with(kind)),
                "no {kind}:\n{}",
                f.outline()
            );
        }
        // The first control shown takes the focus; the command run only by
        // components has no key in the status line.
        assert_eq!(f.focus.as_deref(), Some("name"));
        assert_eq!(a.status_text(), "[connecting] s save  q quit");
        assert!(matches!(
            f.root.find("skin").map(|n| &n.kind),
            Some(Kind::Select { selected: 0, options, .. }) if options.len() == 3
        ));
        assert!(matches!(
            f.root.find("build").map(|n| &n.kind),
            Some(Kind::Progress { ratio, label }) if (*ratio - 0.62).abs() < 1e-9 && label == "build 62%"
        ));
    }

    #[test]
    fn input_components_record_events_that_run_their_commands() {
        let mut a = components();
        // The input: keys and pasted text type, Enter submits.
        assert!(a.takes_text());
        assert!(a.control_key("h"));
        assert!(a.control_key("Space"));
        assert!(a.insert_text("there\n"));
        assert!(a.control_key("Backspace"));
        assert!(a.control_key("Enter"));
        assert_eq!(a.values["name"], "h there");
        let events = a.take_events();
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].kind, EventKind::Change);
        let submit = events.last().expect("submit");
        assert_eq!(
            (submit.kind, &submit.value),
            (EventKind::Submit, &Value::from("h there"))
        );
        assert_eq!(a.command_for(submit).as_deref(), Some("greet"));
        assert_eq!(a.command_for(&events[0]), None, "a change runs nothing");
        // Up and Down are no input's: they fall through to the runtime.
        assert!(!a.control_key("Up"));

        // A click on a select's option picks it, and focuses the select.
        a.select_at(Some("skin"), 1);
        assert_eq!(a.focused.as_deref(), Some("skin"));
        assert!(!a.takes_text(), "a select takes no text");
        assert!(!a.insert_text("x"));
        assert!(a.control_key("Down"));
        assert!(a.control_key("Down"), "the last option stays");
        let events = a.take_events();
        assert_eq!(
            events.iter().map(|e| e.value.clone()).collect::<Vec<_>>(),
            [Value::from("light"), Value::from("follow the system")]
        );
        assert_eq!(a.command_for(&events[0]).as_deref(), Some("pick"));
        // Picking the option already picked changes nothing.
        a.select_at(Some("skin"), 2);
        assert!(a.take_events().is_empty());

        // A toggle flips by a click, Space, or Enter; a button is pressed.
        a.select_at(Some("notify"), 0);
        assert!(a.control_key("Space"));
        a.focus_next();
        assert_eq!(a.focused.as_deref(), Some("save"));
        assert!(a.control_key("Enter"));
        assert!(!a.control_key("x"), "a button leaves letters to commands");
        let events = a.take_events();
        assert_eq!(
            events
                .iter()
                .map(|e| (e.node.as_str(), e.kind, e.value.clone()))
                .collect::<Vec<_>>(),
            [
                ("notify", EventKind::Change, Value::Bool(false)),
                ("notify", EventKind::Change, Value::Bool(true)),
                ("save", EventKind::Press, Value::Null),
            ]
        );
        assert_eq!(a.command_for(&events[2]).as_deref(), Some("save"));
        assert_eq!(
            a.values_json(),
            serde_json::json!({
                "name": "h there",
                "skin": "follow the system",
                "notify": true,
            })
        );
        let filled = crate::manifest::fill_with(
            &toml::from_str("who = \"${value.name}\"\non = \"${value.notify}\"").expect("toml"),
            None,
            None,
            Some(&a.values_json()),
        )
        .expect("filled");
        assert_eq!(filled, serde_json::json!({"who": "h there", "on": true}));
    }

    #[test]
    fn typing_is_one_patch_and_a_reload_keeps_focus_and_values() {
        let mut a = components();
        let before = a.frame().expect("tree");
        a.control_key("x");
        let after = a.frame().expect("tree");
        // Only the input changed: no new focus, no other node.
        assert_eq!(
            view_tree::diff(&before, &after),
            vec![view_tree::Op::Props {
                path: vec![
                    "main".into(),
                    "side".into(),
                    "controls".into(),
                    "name".into()
                ],
                title: None,
                kind: Kind::Input {
                    label: "Name".into(),
                    value: "x".into(),
                    placeholder: "type a name, Enter greets".into(),
                },
                bind: None,
            }]
        );
        a.select_at(Some("skin"), 1);
        a.focus_next();
        // A reload (a new version of the same package) keeps the focus
        // and each component's value by key.
        let values = a.form_values();
        a.manifest = Manifest::parse(&COMPONENTS.replace("Save", "Store")).expect("v2");
        a.reset_form(&values);
        assert_eq!(a.focused.as_deref(), Some("notify"));
        assert_eq!(a.values["name"], "x");
        assert_eq!(a.values["skin"], "light");
        let f = a.frame().expect("tree");
        assert_eq!(f.focus.as_deref(), Some("notify"));
        assert!(matches!(
            f.root.find("save").map(|n| &n.kind),
            Some(Kind::Button { label }) if label == "Store"
        ));
    }

    #[test]
    fn components_are_validated_with_their_node() {
        let bad = COMPONENTS
            .replace("run = \"save\"", "run = \"gone\"")
            .replace(
                "options = [\"dark\", \"light\", \"follow the system\"]",
                "options = []",
            )
            .replace("ratio = 0.62\n", "")
            + "\n[commands.orphan]\nhandler = \"save\"\n";
        let errors = Manifest::parse(&bad).expect_err("invalid");
        for want in [
            "nodes.save.run: no command `gone`",
            "nodes.skin.options: a select needs options",
            "nodes.build: a progress needs `ratio` or a `source`",
            "commands.orphan.key: a command needs a key, or a node or form that runs it",
        ] {
            assert!(
                errors.iter().any(|e| e.contains(want)),
                "{want}: {errors:?}"
            );
        }
        // `save` keeps its key, so it stays valid without a node.
        assert!(!errors.iter().any(|e| e.starts_with("commands.save.key")));
    }
}
