//! What the host keeps for an app's view between its renders: the focus,
//! and each control's value (an input's text, a select's option, a
//! toggle, a table's or list's selected row), by node key.
//!
//! An app gives a value when it draws a node; the user changes it with
//! keys and clicks. The value shown is the user's until the app gives a
//! different one than it gave last time (to clear an input after a
//! submit, say), which then wins. So an app that only listens still has
//! working controls, and an app that sets values stays in charge.

use std::collections::HashMap;

use app_protocol::Event;
use serde_json::{Value, json};
use view_tree::{Frame, Kind, Node};

/// A control's value.
#[derive(Debug, Clone, PartialEq)]
enum Held {
    Text(String),
    Index(usize),
    On(bool),
    Row(Option<String>),
}

/// What an input means for the app.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// A component's event: `press`, `change`, `submit` (an input),
    /// `select` and `enter` (a table's or list's row, by key).
    Component {
        node: String,
        action: &'static str,
        value: Value,
    },
    /// A key no component took.
    Key(String),
    /// The user asked to quit (`q` outside an input).
    Quit,
}

#[derive(Debug, Default)]
pub struct Controls {
    focus: Option<String>,
    /// The focus the app gave last.
    given_focus: Option<String>,
    held: HashMap<String, Held>,
    /// The values the app gave last.
    given: HashMap<String, Held>,
}

fn focusable(k: &Kind) -> bool {
    k.is_control() || matches!(k, Kind::Table { .. } | Kind::List { .. })
}

/// The value a node gives.
fn given(n: &Node) -> Option<Held> {
    Some(match &n.kind {
        Kind::Input { value, .. } => Held::Text(value.clone()),
        Kind::Select { selected, .. } => Held::Index(*selected),
        Kind::Toggle { on, .. } => Held::On(*on),
        Kind::Table { selected, .. } | Kind::List { selected } => Held::Row(selected.clone()),
        _ => return None,
    })
}

/// The focusable nodes shown, in order.
fn focusables(n: &Node, out: &mut Vec<String>) {
    if focusable(&n.kind) && !out.contains(&n.key) {
        out.push(n.key.clone());
    }
    for c in n.shown() {
        focusables(c, out);
    }
}

impl Controls {
    /// The app's frame as shown: the held values and the focus put in.
    pub fn shape(&mut self, app: &Frame) -> Frame {
        let mut f = app.clone();
        let mut seen = HashMap::new();
        self.walk(&mut f.root, &mut seen);
        self.held.retain(|k, _| seen.contains_key(k));
        self.given = seen;
        if app.focus != self.given_focus {
            self.given_focus = app.focus.clone();
            self.focus = app.focus.clone();
        }
        let mut order = Vec::new();
        focusables(&f.root, &mut order);
        if !self.focus.as_ref().is_some_and(|k| order.contains(k)) {
            self.focus = order.first().cloned();
        }
        f.focus = self.focus.clone();
        f
    }

    fn walk(&mut self, n: &mut Node, seen: &mut HashMap<String, Held>) {
        if let Some(g) = given(n)
            && !seen.contains_key(&n.key)
        {
            if self.given.get(&n.key) != Some(&g) || !self.held.contains_key(&n.key) {
                self.held.insert(n.key.clone(), g.clone());
            }
            seen.insert(n.key.clone(), g);
            let held = self.held.get(&n.key).cloned();
            let first = n.children.first().map(|c| c.key.clone());
            let keys: Vec<String> = n.children.iter().map(|c| c.key.clone()).collect();
            match (&mut n.kind, held) {
                (Kind::Input { value, .. }, Some(Held::Text(t))) => *value = t,
                (
                    Kind::Select {
                        selected, options, ..
                    },
                    Some(Held::Index(i)),
                ) => {
                    *selected = i.min(options.len().saturating_sub(1));
                }
                (Kind::Toggle { on, .. }, Some(Held::On(b))) => *on = b,
                (Kind::Table { selected, .. } | Kind::List { selected }, Some(Held::Row(r))) => {
                    *selected = r.filter(|r| keys.contains(r)).or(first);
                }
                _ => {}
            }
        }
        for c in &mut n.children {
            self.walk(c, seen);
        }
    }

    /// What `event` does on the frame `shown`; the held values change
    /// with it.
    pub fn on(&mut self, shown: &Frame, event: Event) -> Vec<Act> {
        match event {
            Event::Key { key } => self.key(shown, key),
            Event::Text { text } => {
                let Some(n) = self.focused(shown) else {
                    return Vec::new();
                };
                if !matches!(n.kind, Kind::Input { .. }) {
                    return Vec::new();
                }
                let text: String = text.chars().filter(|c| !c.is_control()).collect();
                let key = n.key.clone();
                self.edit(&key, |v| v.push_str(&text))
            }
            Event::Select { node, index } => {
                let Some(key) = node.or_else(|| self.focus.clone()) else {
                    return Vec::new();
                };
                let Some(n) = shown.root.find(&key) else {
                    return Vec::new();
                };
                if focusable(&n.kind) {
                    self.focus = Some(key.clone());
                }
                let index = usize::try_from(index).unwrap_or(usize::MAX);
                match &n.kind {
                    Kind::Table { .. } | Kind::List { .. } => {
                        let Some(item) = n.children.get(index) else {
                            return Vec::new();
                        };
                        let row = item.key.clone();
                        self.held.insert(key.clone(), Held::Row(Some(row.clone())));
                        vec![component(&key, "select", json!(row))]
                    }
                    Kind::Select { options, .. } if index < options.len() => {
                        self.held.insert(key.clone(), Held::Index(index));
                        vec![component(&key, "change", json!(index))]
                    }
                    Kind::Toggle { on, .. } => {
                        self.held.insert(key.clone(), Held::On(!on));
                        vec![component(&key, "change", json!(!on))]
                    }
                    Kind::Button { .. } => vec![component(&key, "press", Value::Null)],
                    _ => Vec::new(),
                }
            }
            Event::Scroll { node, steps } => {
                let Some(key) = node.or_else(|| self.focus.clone()) else {
                    return Vec::new();
                };
                match shown.root.find(&key) {
                    Some(n) if matches!(n.kind, Kind::Table { .. } | Kind::List { .. }) => {
                        let n = n.clone();
                        self.move_row(&n, steps)
                    }
                    _ => Vec::new(),
                }
            }
            Event::Component {
                node,
                action,
                value,
            } => {
                let held = match (&value, shown.root.find(&node).map(|n| &n.kind)) {
                    (Value::String(s), Some(Kind::Input { .. })) => Some(Held::Text(s.clone())),
                    (Value::Number(i), Some(Kind::Select { .. })) => {
                        i.as_u64().map(|i| Held::Index(i as usize))
                    }
                    (Value::Bool(b), Some(Kind::Toggle { .. })) => Some(Held::On(*b)),
                    _ => None,
                };
                if let Some(h) = held {
                    self.held.insert(node.clone(), h);
                }
                let action = match action.as_str() {
                    "press" => "press",
                    "change" => "change",
                    "submit" => "submit",
                    "select" => "select",
                    "enter" => "enter",
                    _ => return Vec::new(),
                };
                vec![Act::Component {
                    node,
                    action,
                    value,
                }]
            }
        }
    }

    fn focused<'a>(&self, shown: &'a Frame) -> Option<&'a Node> {
        shown.root.find(self.focus.as_deref()?)
    }

    fn key(&mut self, shown: &Frame, key: String) -> Vec<Act> {
        if key == "Tab" {
            let mut order = Vec::new();
            focusables(&shown.root, &mut order);
            if !order.is_empty() {
                let at = self
                    .focus
                    .as_ref()
                    .and_then(|f| order.iter().position(|k| k == f));
                let next = at.map_or(0, |i| (i + 1) % order.len());
                self.focus = Some(order[next].clone());
            }
            return Vec::new();
        }
        let Some(n) = self.focused(shown).cloned() else {
            return unclaimed(key);
        };
        let k = n.key.clone();
        let press = key == "Enter" || key == " ";
        match &n.kind {
            Kind::Input { value, .. } => {
                let typed = key.chars().count() == 1 && !key.chars().any(char::is_control);
                if typed {
                    return self.edit(&k, |v| v.push_str(&key));
                }
                match key.as_str() {
                    "Backspace" => self.edit(&k, |v| {
                        v.pop();
                    }),
                    "Enter" => {
                        let v = match self.held.get(&k) {
                            Some(Held::Text(t)) => t.clone(),
                            _ => value.clone(),
                        };
                        vec![component(&k, "submit", json!(v))]
                    }
                    _ => unclaimed(key),
                }
            }
            Kind::Button { .. } if press => vec![component(&k, "press", Value::Null)],
            Kind::Toggle { on, .. } if press => {
                self.held.insert(k.clone(), Held::On(!on));
                vec![component(&k, "change", json!(!on))]
            }
            Kind::Select {
                options, selected, ..
            } if !options.is_empty() => {
                let last = options.len() - 1;
                let to = match key.as_str() {
                    "Up" => selected.saturating_sub(1),
                    "Down" => (*selected + 1).min(last),
                    "Enter" | " " => (*selected + 1) % options.len(),
                    _ => return unclaimed(key),
                };
                if to == *selected {
                    return Vec::new();
                }
                self.held.insert(k.clone(), Held::Index(to));
                vec![component(&k, "change", json!(to))]
            }
            Kind::Table { selected, .. } | Kind::List { selected } => {
                let steps = match key.as_str() {
                    "Up" | "k" => -1,
                    "Down" | "j" => 1,
                    "PageUp" => -10,
                    "PageDown" => 10,
                    "Home" => i64::MIN / 2,
                    "End" => i64::MAX / 2,
                    "Enter" => {
                        return selected
                            .iter()
                            .map(|row| component(&k, "enter", json!(row)))
                            .collect();
                    }
                    _ => return unclaimed(key),
                };
                self.move_row(&n, steps)
            }
            _ => unclaimed(key),
        }
    }

    fn edit(&mut self, key: &str, f: impl FnOnce(&mut String)) -> Vec<Act> {
        let mut v = match self.held.get(key) {
            Some(Held::Text(t)) => t.clone(),
            _ => String::new(),
        };
        let before = v.clone();
        f(&mut v);
        if v == before {
            return Vec::new();
        }
        self.held.insert(key.to_owned(), Held::Text(v.clone()));
        vec![component(key, "change", json!(v))]
    }

    fn move_row(&mut self, n: &Node, steps: i64) -> Vec<Act> {
        if n.children.is_empty() {
            return Vec::new();
        }
        let at = n.selected_index().unwrap_or(0) as i64;
        let last = n.children.len() as i64 - 1;
        let to = (at.saturating_add(steps)).clamp(0, last) as usize;
        let row = n.children[to].key.clone();
        if Some(to) == n.selected_index() {
            return Vec::new();
        }
        self.held
            .insert(n.key.clone(), Held::Row(Some(row.clone())));
        vec![component(&n.key, "select", json!(row))]
    }
}

fn component(node: &str, action: &'static str, value: Value) -> Act {
    Act::Component {
        node: node.to_owned(),
        action,
        value,
    }
}

/// A key the focused component did not take: `q` quits; the app gets the
/// rest.
fn unclaimed(key: String) -> Vec<Act> {
    if key == "q" {
        vec![Act::Quit]
    } else {
        vec![Act::Key(key)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(root: Node) -> Frame {
        Frame {
            title: "t".into(),
            status: String::new(),
            focus: None,
            root,
        }
    }

    fn input(key: &str, value: &str) -> Node {
        Node::new(
            key,
            Kind::Input {
                label: String::new(),
                value: value.into(),
                placeholder: String::new(),
            },
        )
    }

    fn key(k: &str) -> Event {
        Event::Key { key: k.into() }
    }

    #[test]
    fn typing_edits_the_input_until_the_app_gives_a_new_value() {
        let mut c = Controls::default();
        let mut root = Node::new("main", Kind::Column);
        root.children = vec![
            input("name", ""),
            Node::new("go", Kind::Button { label: "Go".into() }),
        ];
        let app = frame(root);
        let shown = c.shape(&app);
        assert_eq!(shown.focus.as_deref(), Some("name"));
        assert_eq!(
            c.on(&shown, key("q")),
            vec![component("name", "change", json!("q"))]
        );
        let shown = c.shape(&app);
        assert_eq!(
            shown.root.find("name").map(|n| n.kind.clone()),
            Some(input("x", "q").kind)
        );
        assert_eq!(
            c.on(&shown, key("Enter")),
            vec![component("name", "submit", json!("q"))]
        );
        // The app clears it: its new value wins.
        let mut cleared = app.clone();
        cleared.root.children[0] = input("name", "-");
        let shown = c.shape(&cleared);
        assert_eq!(
            shown.root.find("name").map(|n| n.kind.clone()),
            Some(input("x", "-").kind)
        );
        // Tab to the button; Enter presses it; `q` quits outside an input.
        assert!(c.on(&shown, key("Tab")).is_empty());
        let shown = c.shape(&cleared);
        assert_eq!(shown.focus.as_deref(), Some("go"));
        assert_eq!(
            c.on(&shown, key("Enter")),
            vec![component("go", "press", Value::Null)]
        );
        assert_eq!(c.on(&shown, key("q")), vec![Act::Quit]);
        assert_eq!(c.on(&shown, key("x")), vec![Act::Key("x".into())]);
    }

    #[test]
    fn a_table_selects_its_first_row_and_moves() {
        let mut c = Controls::default();
        let mut t = Node::new(
            "t",
            Kind::Table {
                columns: vec!["id".into()],
                selected: None,
            },
        );
        t.children = ["a", "b", "c"]
            .iter()
            .map(|k| {
                Node::new(
                    *k,
                    Kind::Item {
                        cells: vec![(*k).into()],
                    },
                )
            })
            .collect();
        let app = frame(t);
        let shown = c.shape(&app);
        assert_eq!(shown.root.selected_index(), Some(0));
        assert_eq!(
            c.on(&shown, key("Down")),
            vec![component("t", "select", json!("b"))]
        );
        let shown = c.shape(&app);
        assert_eq!(
            c.on(&shown, key("Enter")),
            vec![component("t", "enter", json!("b"))]
        );
        assert_eq!(
            c.on(
                &shown,
                Event::Select {
                    node: Some("t".into()),
                    index: 2
                }
            ),
            vec![component("t", "select", json!("c"))]
        );
    }
}
