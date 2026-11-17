//! An app's view as a tree of components the hosts know how to draw: the
//! desktop natively, the app host as a terminal UI. Every node has a key,
//! unique among its siblings, so a new version of the tree is compared with
//! the last one key by key: the difference is a list of [`Op`]s that touch
//! only the nodes that changed, and renderers keep what they hold per key
//! (scroll position, focus) across it.
//!
//! The tree is resolved: it holds the text and numbers to draw, never
//! data sources or code. The app host builds it from its model; the
//! daemon keeps the newest ([`Kept`]) and hands each reader the whole tree
//! or the patches since the version it has ([`Mirror`]).
//!
//! The one exception is a bound node ([`Bind`]): it names a daemon stream
//! (an agent session, a pane's tap), and each renderer follows that stream
//! itself, so streamed data reaches it without passing through the app or
//! the tree.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

pub mod rich;
pub mod transcript;

/// The whole view: the chrome around the tree, and the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub title: String,
    /// The status line: the connection, the keys, the last outcome.
    pub status: String,
    /// The key of the node that takes keys, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    pub root: Node,
}

/// One component, its key, and its children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    /// A heading: drawn with a border in a terminal, and as a tab's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
    /// The daemon stream the node draws, for a `transcript` or `composer`
    /// (a session) or a `log` (a tap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<Bind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

/// A daemon stream a node is bound to. On the wire it is
/// `{"session": "<id>"}` or `{"tap": <pane>}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bind {
    /// An agent session's events (`agent.sessions.events`).
    Session(String),
    /// A pane's committed lines (`taps.subscribe`, text level).
    Tap(u32),
}

/// Which way a split divides its space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Side by side.
    #[default]
    Row,
    /// One above the other.
    Column,
}

/// What a node is, with what it shows. Layout nodes (`column`, `row`,
/// `split`, `tabs`, `scroll`) arrange their children; `table`, `list`, and
/// `log` hold their rows as `item` children, keyed by the row; the rest
/// hold their content themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    Column,
    Row,
    /// Children share the space by `sizes` (weights, one per child; equal
    /// when empty).
    Split {
        #[serde(default)]
        direction: Direction,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sizes: Vec<u16>,
    },
    /// One child shown at a time, by index; the others' titles name them.
    Tabs {
        #[serde(default)]
        active: usize,
    },
    /// Children at their natural height, scrolled to keep the focus in view.
    Scroll,
    Table {
        columns: Vec<String>,
        /// The selected item's key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<String>,
    },
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<String>,
    },
    /// Lines, following the newest.
    Log,
    /// A row of a table (one cell per column), list, or log (one cell).
    Item {
        cells: Vec<String>,
    },
    Text {
        text: String,
    },
    /// A ratio from 0 to 1, as a bar.
    Gauge {
        ratio: f64,
        label: String,
    },
    /// Values, as a sparkline.
    Chart {
        values: Vec<f64>,
    },
    Form {
        fields: Vec<Field>,
        /// The field being edited.
        #[serde(default)]
        focus: usize,
    },
    /// Markdown text: headings, emphasis, lists, quotes, code, links as
    /// text ([`rich::markdown`]).
    Markdown {
        text: String,
    },
    /// Source code, highlighted by `language` ([`rich::code`]).
    Code {
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        language: String,
    },
    /// A unified diff, its added and removed lines marked ([`rich::diff`]).
    Diff {
        text: String,
    },
    /// Pressed with Enter or Space when focused, or a click.
    Button {
        label: String,
    },
    /// One line of text being edited; the cursor is at its end.
    Input {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
        value: String,
        /// Shown, dimmed, while the value is empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        placeholder: String,
    },
    /// One of `options`, by index.
    Select {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
        options: Vec<String>,
        #[serde(default)]
        selected: usize,
    },
    /// On or off.
    Toggle {
        label: String,
        on: bool,
    },
    /// A short label in a tone.
    Badge {
        text: String,
        #[serde(default)]
        tone: Tone,
    },
    /// A ratio from 0 to 1, as one thin line with its label.
    Progress {
        ratio: f64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
    /// A status line: short items side by side, each in a tone.
    Status {
        items: Vec<StatusItem>,
    },
    /// An agent session's conversation, from the session it is bound to:
    /// streaming markdown, thinking, tool cards, approval cards, the plan,
    /// and usage ([`transcript`]). The tree holds only how it is shown.
    Transcript {
        /// Thinking shown in full; folded to one line otherwise.
        #[serde(default, skip_serializing_if = "is_false")]
        thinking: bool,
        /// Lines scrolled back from the newest, in a terminal.
        #[serde(default, skip_serializing_if = "is_zero")]
        back: u32,
    },
    /// A prompt for the session it is bound to: several lines of text,
    /// with the cursor at the end, and what is attached to it.
    Composer {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
        value: String,
        /// Shown, dimmed, while the value is empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        placeholder: String,
        /// What goes with the prompt, by label (`selection`, `@%3`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What a badge or a status item means, which picks its colour from the
/// theme: plain, the accent, success, a warning, or danger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    #[default]
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

/// One item of a status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusItem {
    pub text: String,
    #[serde(default)]
    pub tone: Tone,
}

/// A form's field as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub toggle: bool,
    #[serde(default)]
    pub required: bool,
}

impl Kind {
    /// The name used in manifests and on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Column => "column",
            Kind::Row => "row",
            Kind::Split { .. } => "split",
            Kind::Tabs { .. } => "tabs",
            Kind::Scroll => "scroll",
            Kind::Table { .. } => "table",
            Kind::List { .. } => "list",
            Kind::Log => "log",
            Kind::Item { .. } => "item",
            Kind::Text { .. } => "text",
            Kind::Gauge { .. } => "gauge",
            Kind::Chart { .. } => "chart",
            Kind::Form { .. } => "form",
            Kind::Markdown { .. } => "markdown",
            Kind::Code { .. } => "code",
            Kind::Diff { .. } => "diff",
            Kind::Button { .. } => "button",
            Kind::Input { .. } => "input",
            Kind::Select { .. } => "select",
            Kind::Toggle { .. } => "toggle",
            Kind::Badge { .. } => "badge",
            Kind::Progress { .. } => "progress",
            Kind::Status { .. } => "status",
            Kind::Transcript { .. } => "transcript",
            Kind::Composer { .. } => "composer",
        }
    }

    /// Takes keys and clicks of its own: a button, input, select,
    /// toggle, or composer.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Kind::Button { .. }
                | Kind::Input { .. }
                | Kind::Select { .. }
                | Kind::Toggle { .. }
                | Kind::Composer { .. }
        )
    }

    /// The text a text node holds: markdown, code, a diff, or text.
    pub fn text(&self) -> Option<&str> {
        match self {
            Kind::Text { text }
            | Kind::Markdown { text }
            | Kind::Code { text, .. }
            | Kind::Diff { text } => Some(text),
            _ => None,
        }
    }

    /// Arranges children rather than showing data.
    pub fn is_layout(&self) -> bool {
        matches!(
            self,
            Kind::Column | Kind::Row | Kind::Split { .. } | Kind::Tabs { .. } | Kind::Scroll
        )
    }
}

impl Node {
    pub fn new(key: impl Into<String>, kind: Kind) -> Node {
        Node {
            key: key.into(),
            title: None,
            kind,
            bind: None,
            children: Vec::new(),
        }
    }

    /// The node bound to `bind`.
    pub fn bound(mut self, bind: Bind) -> Node {
        self.bind = Some(bind);
        self
    }

    /// Every stream a node in this subtree is bound to, each once, in
    /// tree order (hidden tabs pages too: their streams keep flowing).
    pub fn binds(&self) -> Vec<Bind> {
        let mut out = Vec::new();
        let mut stack = vec![self];
        while let Some(n) = stack.pop() {
            if let Some(b) = &n.bind
                && !out.contains(b)
            {
                out.push(b.clone());
            }
            stack.extend(n.children.iter().rev());
        }
        out
    }

    /// The node with `key` in this subtree.
    pub fn find(&self, key: &str) -> Option<&Node> {
        if self.key == key {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(key))
    }

    /// For a table or list: the index of the selected item.
    pub fn selected_index(&self) -> Option<usize> {
        let (Kind::Table { selected, .. } | Kind::List { selected }) = &self.kind else {
            return None;
        };
        let s = selected.as_deref()?;
        self.children.iter().position(|c| c.key == s)
    }

    /// The children shown: a tabs node's active page, every child
    /// otherwise.
    pub fn shown(&self) -> &[Node] {
        match self.kind {
            Kind::Tabs { active } => {
                let i = active.min(self.children.len().saturating_sub(1));
                self.children.get(i..=i).unwrap_or(&[])
            }
            _ => &self.children,
        }
    }

    /// An item's cells.
    pub fn cells(&self) -> &[String] {
        match &self.kind {
            Kind::Item { cells } => cells,
            _ => &[],
        }
    }

    fn outline_into(&self, depth: usize, out: &mut String) {
        if matches!(self.kind, Kind::Item { .. }) {
            return;
        }
        out.push_str(&"  ".repeat(depth));
        out.push_str(self.kind.name());
        out.push(' ');
        out.push_str(&self.key);
        let items = self
            .children
            .iter()
            .filter(|c| matches!(c.kind, Kind::Item { .. }))
            .count();
        if items > 0 {
            out.push_str(&format!(" ({items} rows)"));
        }
        out.push('\n');
        for c in &self.children {
            c.outline_into(depth + 1, out);
        }
    }
}

impl Frame {
    /// The tree's structure, one node per line, indented by depth: the
    /// kind, the key, and how many rows (items) it holds.
    pub fn outline(&self) -> String {
        let mut out = String::new();
        self.root.outline_into(0, &mut out);
        out
    }

    /// Keys are present and unique among siblings.
    pub fn check(&self) -> Result<(), String> {
        fn go(n: &Node) -> Result<(), String> {
            if n.key.is_empty() {
                return Err("a node without a key".into());
            }
            let mut seen = std::collections::HashSet::new();
            for c in &n.children {
                if !seen.insert(c.key.as_str()) {
                    return Err(format!("`{}` has two children keyed `{}`", n.key, c.key));
                }
                go(c)?;
            }
            Ok(())
        }
        go(&self.root)
    }
}

/// One change from a tree to the next. Nodes are addressed by the keys on
/// the path from the root, the root's own key first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// The chrome changed.
    Frame {
        title: String,
        status: String,
        #[serde(default)]
        focus: Option<String>,
    },
    /// A different root.
    Root { node: Node },
    /// A node's own content changed (not its children).
    Props {
        path: Vec<String>,
        #[serde(default)]
        title: Option<String>,
        kind: Kind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<Bind>,
    },
    /// A node's children changed: their keys in order. Children whose key
    /// was there before are kept as they were (later ops change them);
    /// `added` holds the new ones in full.
    Children {
        path: Vec<String>,
        keys: Vec<String>,
        #[serde(default)]
        added: Vec<Node>,
    },
}

/// What changed from `old` to `new`, by key. Empty when nothing did.
pub fn diff(old: &Frame, new: &Frame) -> Vec<Op> {
    let mut ops = Vec::new();
    if old.title != new.title || old.status != new.status || old.focus != new.focus {
        ops.push(Op::Frame {
            title: new.title.clone(),
            status: new.status.clone(),
            focus: new.focus.clone(),
        });
    }
    if old.root.key != new.root.key {
        ops.push(Op::Root {
            node: new.root.clone(),
        });
        return ops;
    }
    let mut path = vec![new.root.key.clone()];
    diff_node(&old.root, &new.root, &mut path, &mut ops);
    ops
}

fn diff_node(old: &Node, new: &Node, path: &mut Vec<String>, ops: &mut Vec<Op>) {
    if old.title != new.title || old.kind != new.kind || old.bind != new.bind {
        ops.push(Op::Props {
            path: path.clone(),
            title: new.title.clone(),
            kind: new.kind.clone(),
            bind: new.bind.clone(),
        });
    }
    let before: HashMap<&str, &Node> = old.children.iter().map(|c| (c.key.as_str(), c)).collect();
    let same_keys = old.children.len() == new.children.len()
        && old
            .children
            .iter()
            .zip(&new.children)
            .all(|(a, b)| a.key == b.key);
    if !same_keys {
        ops.push(Op::Children {
            path: path.clone(),
            keys: new.children.iter().map(|c| c.key.clone()).collect(),
            added: new
                .children
                .iter()
                .filter(|c| !before.contains_key(c.key.as_str()))
                .cloned()
                .collect(),
        });
    }
    for c in &new.children {
        if let Some(o) = before.get(c.key.as_str()) {
            path.push(c.key.clone());
            diff_node(o, c, path, ops);
            path.pop();
        }
    }
}

fn locate<'a>(root: &'a mut Node, path: &[String]) -> Result<&'a mut Node, String> {
    let (first, rest) = path.split_first().ok_or("an empty path")?;
    if root.key != *first {
        return Err(format!("no root `{first}`"));
    }
    let mut n = root;
    for k in rest {
        n = n
            .children
            .iter_mut()
            .find(|c| c.key == *k)
            .ok_or_else(|| format!("no node `{}`", path.join("/")))?;
    }
    Ok(n)
}

/// Apply `ops` to `frame`, in order. An error means the frame is not the
/// one the ops were made from; the reader then needs the whole tree.
pub fn apply(frame: &mut Frame, ops: &[Op]) -> Result<(), String> {
    for op in ops {
        match op {
            Op::Frame {
                title,
                status,
                focus,
            } => {
                frame.title.clone_from(title);
                frame.status.clone_from(status);
                frame.focus.clone_from(focus);
            }
            Op::Root { node } => frame.root = node.clone(),
            Op::Props {
                path,
                title,
                kind,
                bind,
            } => {
                let n = locate(&mut frame.root, path)?;
                n.title.clone_from(title);
                n.kind = kind.clone();
                n.bind.clone_from(bind);
            }
            Op::Children { path, keys, added } => {
                let n = locate(&mut frame.root, path)?;
                let mut old: HashMap<String, Node> =
                    n.children.drain(..).map(|c| (c.key.clone(), c)).collect();
                let mut new: HashMap<&str, &Node> =
                    added.iter().map(|c| (c.key.as_str(), c)).collect();
                n.children = keys
                    .iter()
                    .map(|k| {
                        old.remove(k)
                            .or_else(|| new.remove(k.as_str()).cloned())
                            .ok_or_else(|| format!("no child `{k}` under `{}`", path.join("/")))
                    })
                    .collect::<Result<_, _>>()?;
            }
        }
    }
    Ok(())
}

/// What a publisher sends next. One is made per publish and turned into
/// a message at once, so its size does not matter.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Update {
    /// The whole tree, as version `seq`.
    Full { seq: u64, frame: Frame },
    /// The changes from version `seq - 1` to `seq`.
    Patch { seq: u64, ops: Vec<Op> },
}

/// The publishing side: the last tree sent and its version.
#[derive(Debug, Default)]
pub struct Outgoing {
    seq: u64,
    sent: Option<Frame>,
}

impl Outgoing {
    /// What to send for `frame`: the whole tree first and after
    /// [`Outgoing::resync`], the changes otherwise; `None` when nothing
    /// changed.
    pub fn next(&mut self, frame: &Frame) -> Option<Update> {
        let update = match &self.sent {
            Some(prev) => {
                let ops = diff(prev, frame);
                if ops.is_empty() {
                    return None;
                }
                self.seq += 1;
                Update::Patch { seq: self.seq, ops }
            }
            None => {
                self.seq += 1;
                Update::Full {
                    seq: self.seq,
                    frame: frame.clone(),
                }
            }
        };
        self.sent = Some(frame.clone());
        Some(update)
    }

    /// The receiver lost track: send the whole tree next.
    pub fn resync(&mut self) {
        self.sent = None;
    }
}

/// Patches the daemon keeps per app pane, so a reader one or a few
/// versions behind gets only the changes.
pub const KEPT_PATCHES: usize = 64;

/// The daemon's copy of a pane's tree: the newest version, and the patches
/// that led to it since the last whole tree.
#[derive(Debug, Default)]
pub struct Kept {
    seq: u64,
    frame: Option<Frame>,
    log: VecDeque<(u64, String)>,
}

/// What a reader at some version needs to reach the newest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Since {
    /// The version the reader is at afterwards.
    pub seq: u64,
    /// The whole tree as JSON, when patches would not do; else empty.
    pub tree: String,
    /// Each version's ops as JSON, in order from the reader's version.
    pub patches: Vec<String>,
}

/// Why a publish was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The patch does not follow the version kept: send the whole tree.
    Resync(String),
    Invalid(String),
}

impl Kept {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    /// Take version `seq`: the whole tree (`tree`, JSON) or the ops from
    /// the version before (`patch`, JSON).
    pub fn publish(&mut self, seq: u64, tree: &str, patch: &str) -> Result<(), Refused> {
        match (tree.is_empty(), patch.is_empty()) {
            (false, true) => {
                let frame: Frame = serde_json::from_str(tree)
                    .map_err(|e| Refused::Invalid(format!("tree: {e}")))?;
                frame.check().map_err(Refused::Invalid)?;
                self.frame = Some(frame);
                self.seq = seq;
                self.log.clear();
                Ok(())
            }
            (true, false) => {
                let Some(frame) = self.frame.as_mut() else {
                    return Err(Refused::Resync("no tree yet".into()));
                };
                if seq != self.seq + 1 {
                    return Err(Refused::Resync(format!(
                        "version {seq} does not follow {}",
                        self.seq
                    )));
                }
                let ops: Vec<Op> = serde_json::from_str(patch)
                    .map_err(|e| Refused::Invalid(format!("patch: {e}")))?;
                if let Err(e) = apply(frame, &ops) {
                    self.frame = None;
                    self.log.clear();
                    return Err(Refused::Resync(e));
                }
                self.seq = seq;
                self.log.push_back((seq, patch.to_owned()));
                if self.log.len() > KEPT_PATCHES {
                    self.log.pop_front();
                }
                Ok(())
            }
            _ => Err(Refused::Invalid("send a tree or a patch".into())),
        }
    }

    /// What a reader at version `have` (0: none) needs.
    pub fn since(&self, have: u64) -> Since {
        let Some(frame) = &self.frame else {
            return Since::default();
        };
        if have == self.seq {
            return Since {
                seq: self.seq,
                ..Since::default()
            };
        }
        let covered = have != 0
            && have < self.seq
            && self
                .log
                .front()
                .is_some_and(|(first, _)| *first <= have + 1);
        if covered {
            return Since {
                seq: self.seq,
                tree: String::new(),
                patches: self
                    .log
                    .iter()
                    .filter(|(s, _)| *s > have)
                    .map(|(_, p)| p.clone())
                    .collect(),
            };
        }
        Since {
            seq: self.seq,
            tree: serde_json::to_string(frame).unwrap_or_default(),
            patches: Vec::new(),
        }
    }
}

/// A reader's copy of a pane's tree.
#[derive(Debug, Default, Clone)]
pub struct Mirror {
    /// The version held; 0 asks for the whole tree.
    pub seq: u64,
    pub frame: Option<Frame>,
}

impl Mirror {
    /// Take what the daemon answered: version `seq`, as a whole `tree` or
    /// the `patches` since [`Mirror::seq`]. Whether the tree changed. On an
    /// error the mirror is reset, so the next read asks for the whole tree.
    pub fn take(&mut self, seq: u64, tree: &str, patches: &[String]) -> Result<bool, String> {
        let r = self.try_take(seq, tree, patches);
        if r.is_err() {
            self.seq = 0;
        }
        r
    }

    fn try_take(&mut self, seq: u64, tree: &str, patches: &[String]) -> Result<bool, String> {
        if !tree.is_empty() {
            let frame: Frame = serde_json::from_str(tree).map_err(|e| format!("tree: {e}"))?;
            self.frame = Some(frame);
            self.seq = seq;
            return Ok(true);
        }
        if patches.is_empty() {
            return Ok(false);
        }
        let Some(frame) = self.frame.as_mut() else {
            return Err("a patch without a tree".into());
        };
        if self.seq + patches.len() as u64 != seq {
            return Err(format!(
                "{} patches do not lead from version {} to {seq}",
                patches.len(),
                self.seq
            ));
        }
        for p in patches {
            let ops: Vec<Op> = serde_json::from_str(p).map_err(|e| format!("patch: {e}"))?;
            apply(frame, &ops)?;
        }
        self.seq = seq;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, cells: &[&str]) -> Node {
        Node::new(
            key,
            Kind::Item {
                cells: cells.iter().map(|s| (*s).to_owned()).collect(),
            },
        )
    }

    fn frame(rows: &[(&str, &str)]) -> Frame {
        let mut table = Node::new(
            "panes",
            Kind::Table {
                columns: vec!["id".into(), "cmd".into()],
                selected: Some("1".into()),
            },
        );
        table.children = rows.iter().map(|(k, c)| item(k, &[k, c])).collect();
        let mut root = Node::new("main", Kind::Column);
        root.children = vec![Node::new("head", Kind::Text { text: "hi".into() }), table];
        Frame {
            title: "T".into(),
            status: "live".into(),
            focus: Some("panes".into()),
            root,
        }
    }

    #[test]
    fn a_tree_reads_back_from_its_json() {
        let f = frame(&[("1", "sh")]);
        let json = serde_json::to_string(&f).expect("json");
        assert!(json.contains(r#""type":"table""#), "{json}");
        assert_eq!(serde_json::from_str::<Frame>(&json).expect("read"), f);
    }

    #[test]
    fn rows_added_moved_and_removed_are_one_children_op() {
        let old = frame(&[("1", "sh"), ("2", "vim")]);
        let new = frame(&[("3", "top"), ("1", "sh")]);
        let ops = diff(&old, &new);
        assert_eq!(ops.len(), 1, "{ops:?}");
        let Op::Children { keys, added, .. } = &ops[0] else {
            panic!("{ops:?}");
        };
        assert_eq!(keys, &["3", "1"]);
        assert_eq!(added.len(), 1);
        let mut applied = old.clone();
        apply(&mut applied, &ops).expect("apply");
        assert_eq!(applied, new);
    }

    #[test]
    fn a_patch_that_does_not_fit_is_refused_and_the_reader_resyncs() {
        let mut kept = Kept::default();
        let mut out = Outgoing::default();
        let send = |kept: &mut Kept, u: Update| match u {
            Update::Full { seq, frame } => {
                kept.publish(seq, &serde_json::to_string(&frame).expect("json"), "")
            }
            Update::Patch { seq, ops } => {
                kept.publish(seq, "", &serde_json::to_string(&ops).expect("json"))
            }
        };
        let a = frame(&[("1", "sh")]);
        let b = frame(&[("1", "zsh")]);
        let c = frame(&[("1", "zsh"), ("2", "vim")]);
        send(&mut kept, out.next(&a).expect("first")).expect("full");
        let mut reader = Mirror::default();
        let s = kept.since(reader.seq);
        assert!(!s.tree.is_empty());
        reader.take(s.seq, &s.tree, &s.patches).expect("take");
        send(&mut kept, out.next(&b).expect("patch")).expect("patch");
        send(&mut kept, out.next(&c).expect("patch")).expect("patch");
        // A reader one version behind gets the patches, not the tree.
        let s = kept.since(reader.seq);
        assert!(s.tree.is_empty());
        assert_eq!(s.patches.len(), 2);
        reader.take(s.seq, &s.tree, &s.patches).expect("take");
        assert_eq!(reader.frame.as_ref(), Some(&c));
        assert_eq!(kept.since(reader.seq).patches.len(), 0);
        // A patch that skips a version is refused; the publisher resyncs.
        assert!(matches!(
            kept.publish(kept.seq() + 2, "", "[]"),
            Err(Refused::Resync(_))
        ));
        // A reader whose patches do not lead to the version resets.
        let mut lost = Mirror {
            seq: 1,
            frame: Some(a.clone()),
        };
        assert!(lost.take(9, "", &["[]".into()]).is_err());
        assert_eq!(lost.seq, 0);
        out.resync();
        assert!(matches!(out.next(&c), Some(Update::Full { .. })));
        assert_eq!(out.next(&c), None);
    }
}
