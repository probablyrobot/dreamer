//! The app manifest (spec 13.3): a versioned TOML file declaring data
//! sources, one view (version 1) or a keyed tree of nodes (version 2), and
//! keybound commands. It is data, not code: there
//! is no shell, no eval, and no expression language. Commands fill their
//! parameters only from `${selected.<field>}` placeholders, replaced by the
//! selected row's value with its type kept.

use std::collections::BTreeMap;

use serde::Deserialize;

pub const SCHEMA_VERSION: u32 = 1;
/// A manifest whose view is a tree of nodes (`root` and `[nodes.*]`).
pub const TREE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub title: String,
    /// Methods the app needs; shown when its grant lacks one.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// The app's grant covers every pane, not only its own: to read other
    /// panes, or to hear events about them (a job's, a filter's).
    #[serde(default)]
    pub all_panes: bool,
    #[serde(default)]
    pub data: BTreeMap<String, Source>,
    /// A Luau module beside the manifest, for logic the manifest cannot
    /// express: `transform(rows)` and command handlers.
    #[serde(default)]
    pub module: Option<String>,
    /// The one view (version 1). In a tree manifest it is unused and
    /// left at its default.
    #[serde(default)]
    pub view: View,
    /// A tree manifest (version 2): the key of the root node, and every
    /// node by key.
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeSpec>,
    #[serde(default)]
    pub commands: BTreeMap<String, Command>,
    /// An agentic pane (spec 13.4): the package bound to a provider and a
    /// grant, with `view.type = "agent"`.
    #[serde(default)]
    pub agent: Option<AgentSpec>,
    /// What the package adds to the desktop when installed (spec 12.2.1).
    #[serde(default)]
    pub contributes: Contributes,
    /// A process app: a program that speaks the app process protocol on
    /// its standard input and output, and draws its own view tree. Needs
    /// the `process` capability.
    #[serde(default)]
    pub process: Option<ProcessSpec>,
    /// A script app: its logic in TypeScript (or JavaScript), run by the
    /// app host, which draws the view tree it returns.
    #[serde(default)]
    pub script: Option<ScriptSpec>,
    /// Its web pages: a local `url` to load them from instead of the
    /// package's `web/` directory, and the origins they may navigate to.
    /// A version 2 manifest with `[web]` and no view of its own (no
    /// `root`, script, or process) is a web app: its view is the page.
    #[serde(default)]
    pub web: Option<WebSpec>,
}

/// An app's web pages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSpec {
    /// A local address (a dev server: `http://localhost:5173`) the pages
    /// load from, instead of the package's `web/` directory. Its origin is
    /// the app's.
    #[serde(default)]
    pub url: Option<String>,
    /// Origins (`https://example.com`) the pages may also navigate to;
    /// they never get the bridge.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// The node key of a web app's one page.
pub const WEB_PAGE: &str = "page";

/// The capability a package needs to run a program of its own.
pub const PROCESS: &str = "process";

/// The capability that will let a package's pages save downloads (see
/// `web_engine::browser`). Planned: every download is refused for now, so
/// a package that asks for it is told so rather than granted nothing.
pub const WEB_DOWNLOAD: &str = "web.download";

/// A process app's program.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    /// The program and its arguments; no shell. A program with a `/` in
    /// it is relative to the package directory; else it is looked up on
    /// `PATH`.
    pub command: Vec<String>,
}

/// A script app's logic.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSpec {
    /// The file beside the manifest: `app.ts`, or plain JavaScript
    /// (`.js`). A TypeScript file runs from the JavaScript `dream app
    /// check` and `dream app install` build from it ([`app_protocol::build`]).
    pub source: String,
}

/// What an agentic pane is for, and so how much it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Answers questions about other terminals from bounded context.
    /// Read-only.
    Inspector,
    /// Watches its data source (no model calls while idle) and explains
    /// on request. Read-only.
    Monitor,
    /// Plans and performs layout, job, and input actions.
    Operator,
    /// Installs and edits filters from a description, and shows the lines
    /// they match. Reads only, plus `filters.manage` (filters that read).
    Filter,
}

impl AgentKind {
    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Inspector => "inspector",
            AgentKind::Monitor => "monitor",
            AgentKind::Operator => "operator",
            AgentKind::Filter => "filter",
        }
    }

    /// Inspectors, monitors, and filter panes only read.
    pub fn read_only(self) -> bool {
        !matches!(self, AgentKind::Operator)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTools {
    /// Dreamer methods the model may call through `dream mcp`; each must be
    /// among the package's capabilities.
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub panes: Vec<u32>,
    #[serde(default)]
    pub all_panes: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub kind: AgentKind,
    /// `claude` or `codex`; without one, the first ready CLI.
    #[serde(default)]
    pub provider: Option<String>,
    /// Added to the provider's instructions.
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: AgentTools,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// A run is stopped after this many seconds (default 300).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// The contribution schema this runtime reads.
pub const CONTRIBUTES_VERSION: u32 = 1;

/// Declarative additions to the desktop: data, placed and drawn by the
/// host. Each is named `<package id>.<id>`, so packages cannot collide;
/// every action is one of the package's own commands, run through the
/// typed API under the package's capabilities.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contributes {
    #[serde(default = "contributes_version")]
    pub version: u32,
    /// Palette commands, optionally with a keybinding.
    #[serde(default)]
    pub commands: Vec<ContributedCommand>,
    /// Sidebar entries; clicking one opens the package as a panel.
    #[serde(default)]
    pub sidebar: Vec<SidebarEntry>,
    /// Compact status items in the tab strip.
    #[serde(default)]
    pub status: Vec<StatusItem>,
}

fn contributes_version() -> u32 {
    CONTRIBUTES_VERSION
}

impl Default for Contributes {
    fn default() -> Contributes {
        Contributes {
            version: CONTRIBUTES_VERSION,
            commands: Vec::new(),
            sidebar: Vec::new(),
            status: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributedCommand {
    pub id: String,
    /// The palette label.
    pub title: String,
    /// One of the package's `[commands]` with a method and no
    /// `${selected...}` or `${form...}` placeholders, or
    #[serde(default)]
    pub run: Option<String>,
    /// `"panel"`: open the package as a panel.
    #[serde(default)]
    pub opens: Option<String>,
    /// A desktop keybinding such as `cmd-alt-r` or `ctrl-alt-r`.
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarEntry {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusItem {
    pub id: String,
    /// Shown before the value.
    #[serde(default)]
    pub label: Option<String>,
    /// One of the package's data sources.
    pub source: String,
    /// The first row's field shown; without one, the number of rows.
    #[serde(default)]
    pub field: Option<String>,
}

/// Whether `id` is a contribution id: lowercase letters, digits, `-`.
fn contribution_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether a desktop keybinding is well formed and holds a modifier that
/// keeps it away from typing: `cmd`, `ctrl`, or `alt`, plus one key.
pub fn binding_ok(k: &str) -> bool {
    let parts: Vec<&str> = k.split('-').collect();
    let Some((key, mods)) = parts.split_last() else {
        return false;
    };
    let mods_ok = mods
        .iter()
        .all(|m| matches!(*m, "cmd" | "ctrl" | "alt" | "shift"));
    let unique = mods.iter().collect::<std::collections::BTreeSet<_>>().len() == mods.len();
    let guarded = mods.iter().any(|m| matches!(*m, "cmd" | "ctrl" | "alt"));
    let key_ok = (key.len() == 1
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        || key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n));
    mods_ok && unique && guarded && key_ok
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// An API method that returns the data (a query, not a mutation), or
    #[serde(default)]
    pub method: String,
    /// a collector's name: its rows, through `collect.query`, each with its
    /// fields as members.
    #[serde(default)]
    pub collector: Option<String>,
    /// The method's parameters; for a collector, `collect.query`'s other
    /// than `name` (`fields`, `panes`, `group_by`, ...).
    #[serde(default)]
    pub params: toml::Table,
    /// Where the rows are in the result, as dotted field names.
    #[serde(default)]
    pub path: Option<String>,
    /// Fetch again this often (at least 200 ms).
    #[serde(default)]
    pub refresh_ms: Option<u64>,
    /// Fetch again when an event of these kinds (prefixes) is recorded.
    #[serde(default)]
    pub events: Vec<String>,
    /// Rows kept at most.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    500
}

/// The method a collector source calls.
pub const COLLECT_QUERY: &str = "collect.query";

impl Source {
    /// The method called: its own, or `collect.query` for a collector.
    pub fn method_name(&self) -> &str {
        if self.collector.is_some() {
            COLLECT_QUERY
        } else {
            &self.method
        }
    }

    /// The parameters sent: for a collector, its name, and as many rows as
    /// the source keeps unless the parameters say otherwise.
    pub fn call_params(&self) -> toml::Table {
        let mut p = self.params.clone();
        if let Some(c) = &self.collector {
            p.insert("name".into(), toml::Value::String(c.clone()));
            if !p.contains_key("limit") {
                p.insert("limit".into(), toml::Value::Integer(self.limit as i64));
            }
        }
        p
    }

    /// Where the rows are in the result: the source's `path`, or for a
    /// collector `rows` (`groups` with `group_by`).
    pub fn rows_path(&self) -> Option<String> {
        if self.path.is_some() || self.collector.is_none() {
            return self.path.clone();
        }
        Some(if self.params.contains_key("group_by") {
            "groups".into()
        } else {
            "rows".into()
        })
    }
}

/// A collector row as an app shows it: its fields become members (a
/// row's own member of the same name wins), and `fields` an object of them.
pub fn collector_row(row: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut o) = row else {
        return row;
    };
    let fields = match o.remove("fields") {
        Some(serde_json::Value::Array(a)) => a,
        _ => Vec::new(),
    };
    let mut by_name = serde_json::Map::new();
    for f in fields {
        if let (Some(n), Some(v)) = (f["name"].as_str(), f.get("value")) {
            by_name.insert(n.to_owned(), v.clone());
        }
    }
    for (k, v) in &by_name {
        if !o.contains_key(k) {
            o.insert(k.clone(), v.clone());
        }
    }
    o.insert("fields".into(), serde_json::Value::Object(by_name));
    serde_json::Value::Object(o)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewKind {
    Table,
    List,
    Log,
    Text,
    /// One value from 0 to 1 (or a percentage above 1) as a bar.
    Gauge,
    /// A numeric field across the rows, as a sparkline.
    Chart,
    /// Typed fields and a submit command.
    Form,
    /// An agentic pane: a prompt, and what the agent did (needs `[agent]`).
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Number,
    Toggle,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormField {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    #[serde(rename = "type")]
    pub kind: ViewKind,
    /// The data source shown (a monitor's watched source); an agent view
    /// may have none.
    #[serde(default)]
    pub source: String,
    /// The field that identifies a row, so the selection survives updates.
    #[serde(default)]
    pub row_key: Option<String>,
    /// Table: the fields shown, as columns.
    #[serde(default)]
    pub columns: Vec<String>,
    /// List, log, gauge, and chart: the field shown per row (default the
    /// whole row).
    #[serde(default)]
    pub field: Option<String>,
    /// Gauge: the field with its label.
    #[serde(default)]
    pub label: Option<String>,
    /// Form: its fields, and the command Enter runs.
    #[serde(default)]
    pub fields: Vec<FormField>,
    #[serde(default)]
    pub submit: Option<String>,
}

impl Default for View {
    fn default() -> View {
        View {
            kind: ViewKind::Text,
            source: String::new(),
            row_key: None,
            columns: Vec::new(),
            field: None,
            label: None,
            fields: Vec::new(),
            submit: None,
        }
    }
}

/// What a node of a tree manifest is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Column,
    Row,
    Split,
    Tabs,
    Scroll,
    Table,
    List,
    Log,
    Text,
    Gauge,
    Chart,
    Form,
    Markdown,
    Code,
    Diff,
    Button,
    Input,
    Select,
    Toggle,
    Badge,
    Progress,
    Status,
    Transcript,
    Composer,
    Terminal,
    Image,
    Web,
}

impl NodeType {
    pub fn is_layout(self) -> bool {
        matches!(
            self,
            NodeType::Column | NodeType::Row | NodeType::Split | NodeType::Tabs | NodeType::Scroll
        )
    }

    /// Takes keys when focused: moves a selection, edits fields, or is a
    /// control.
    pub fn focusable(self) -> bool {
        matches!(self, NodeType::Table | NodeType::List | NodeType::Form)
            || self.is_control()
            || self.is_session()
    }

    /// Bound to an agent session (by `session`, or the first row's
    /// `field` of its `source`): a transcript or a composer.
    pub fn is_session(self) -> bool {
        matches!(self, NodeType::Transcript | NodeType::Composer)
    }

    /// A button, input, select, or toggle: an input component whose
    /// value the model keeps and whose events it records.
    pub fn is_control(self) -> bool {
        matches!(
            self,
            NodeType::Button | NodeType::Input | NodeType::Select | NodeType::Toggle
        )
    }

    /// Shows text or a value given by `text` (or `ratio`, `items`) or
    /// read from its `source`: markdown, code, a diff, a badge, a
    /// progress line, or a status line.
    pub fn is_content(self) -> bool {
        matches!(
            self,
            NodeType::Markdown
                | NodeType::Code
                | NodeType::Diff
                | NodeType::Badge
                | NodeType::Progress
                | NodeType::Status
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            NodeType::Column => "column",
            NodeType::Row => "row",
            NodeType::Split => "split",
            NodeType::Tabs => "tabs",
            NodeType::Scroll => "scroll",
            NodeType::Table => "table",
            NodeType::List => "list",
            NodeType::Log => "log",
            NodeType::Text => "text",
            NodeType::Gauge => "gauge",
            NodeType::Chart => "chart",
            NodeType::Form => "form",
            NodeType::Markdown => "markdown",
            NodeType::Code => "code",
            NodeType::Diff => "diff",
            NodeType::Button => "button",
            NodeType::Input => "input",
            NodeType::Select => "select",
            NodeType::Toggle => "toggle",
            NodeType::Badge => "badge",
            NodeType::Progress => "progress",
            NodeType::Status => "status",
            NodeType::Transcript => "transcript",
            NodeType::Composer => "composer",
            NodeType::Terminal => "terminal",
            NodeType::Image => "image",
            NodeType::Web => "web",
        }
    }
}

/// A node of a tree manifest: a layout node with `children` (keys of other
/// nodes), or a data node with a version 1 view's fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    #[serde(rename = "type")]
    pub kind: NodeType,
    /// A heading; a tab's name.
    #[serde(default)]
    pub title: Option<String>,
    /// Layout nodes: the keys of their children, in order.
    #[serde(default)]
    pub children: Vec<String>,
    /// Split: `row` (side by side, the default) or `column`.
    #[serde(default)]
    pub direction: view_tree::Direction,
    /// Split: a weight per child.
    #[serde(default)]
    pub sizes: Vec<u16>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub row_key: Option<String>,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub fields: Vec<FormField>,
    /// Form: the command Enter runs. Input: the command Enter runs.
    #[serde(default)]
    pub submit: Option<String>,
    /// Markdown, code, diff, and badge: the text shown, when it does not
    /// come from `source` (the first row's `field`). Progress: its label.
    #[serde(default)]
    pub text: Option<String>,
    /// Code: the language highlighted (`rust`, `ts`, `python`, `sh`, ...).
    #[serde(default)]
    pub language: Option<String>,
    /// Badge: what the text means, which picks its colour.
    #[serde(default)]
    pub tone: view_tree::Tone,
    /// Progress: the ratio (0 to 1) when it does not come from `source`.
    #[serde(default)]
    pub ratio: Option<f64>,
    /// Status: its items, when they do not come from `source` (one per
    /// row, its `field`).
    #[serde(default)]
    pub items: Vec<view_tree::StatusItem>,
    /// Select: the options.
    #[serde(default)]
    pub options: Vec<String>,
    /// Input: the starting text; select: the starting option.
    #[serde(default)]
    pub value: Option<String>,
    /// Input: shown while the value is empty.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Toggle: on at the start.
    #[serde(default)]
    pub on: bool,
    /// Button: the command a press runs. Select and toggle: the command a
    /// change runs.
    #[serde(default)]
    pub run: Option<String>,
    /// Transcript and composer: the agent session they are bound to, when
    /// it does not come from `source` (the first row's `field`).
    #[serde(default)]
    pub session: Option<String>,
    /// Log: the pane whose committed lines it shows, as a tap, instead of
    /// a `source`.
    #[serde(default)]
    pub tap: Option<u32>,
    /// Terminal: the tmux pane shown, by number (`3` for `%3`).
    #[serde(default)]
    pub pane: Option<u32>,
    /// Image: a PNG or JPEG file in the package, by its path there. Web:
    /// a page in the package's `web/` directory, or a URL on the app's
    /// origin or its allow-list.
    #[serde(default)]
    pub src: Option<String>,
}

impl NodeSpec {
    /// A data node as a version 1 view; `None` for a layout node.
    pub fn view(&self) -> Option<View> {
        if self.tap.is_some() {
            // A log bound to a tap is drawn from the stream, not rows.
            return None;
        }
        let kind = match self.kind {
            NodeType::Table => ViewKind::Table,
            NodeType::List => ViewKind::List,
            NodeType::Log => ViewKind::Log,
            NodeType::Text => ViewKind::Text,
            NodeType::Gauge => ViewKind::Gauge,
            NodeType::Chart => ViewKind::Chart,
            NodeType::Form => ViewKind::Form,
            _ => return None,
        };
        Some(View {
            kind,
            source: self.source.clone(),
            row_key: self.row_key.clone(),
            columns: self.columns.clone(),
            field: self.field.clone(),
            label: self.label.clone(),
            fields: self.fields.clone(),
            submit: self.submit.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// The key that runs it. A command that a button, input, select, or
    /// toggle runs (or a form submits) may have none.
    #[serde(default)]
    pub key: String,
    /// An API method to call, or
    #[serde(default)]
    pub method: Option<String>,
    /// a function of the module, called with the selected row (and the
    /// form) to do the work.
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub params: toml::Table,
    #[serde(default)]
    pub label: Option<String>,
}

/// Keys the runtime keeps for itself.
pub const RESERVED_KEYS: [&str; 8] = ["q", "Up", "Down", "k", "j", "PageUp", "PageDown", "Ctrl-c"];

/// Keys a tree's runtime also keeps: Tab moves the focus, Left and Right
/// switch tabs.
pub const TREE_RESERVED_KEYS: [&str; 3] = ["Tab", "Left", "Right"];

/// What a transcript calls: following its session, and answering its
/// approval requests.
pub const TRANSCRIPT_METHODS: [&str; 4] = [
    "agent.sessions.events",
    "agent.sessions.approve",
    "agent.sessions.deny",
    "terminal.unsubscribe",
];

/// What a composer calls: sending a prompt, stopping a turn, and
/// following its session (to know whether a turn runs).
pub const COMPOSER_METHODS: [&str; 4] = [
    "agent.sessions.send",
    "agent.sessions.cancel",
    "agent.sessions.events",
    "terminal.unsubscribe",
];

/// Whether `k` names a key the runtime understands.
pub fn key_ok(k: &str) -> bool {
    let named = [
        "Enter",
        "Esc",
        "Tab",
        "Backspace",
        "Delete",
        "Left",
        "Right",
        "Home",
        "End",
    ];
    named.contains(&k)
        || (k.chars().count() == 1 && k.chars().all(|c| c.is_ascii_graphic()))
        || k.strip_prefix('F')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n))
        || k.strip_prefix("Ctrl-")
            .is_some_and(|c| c.len() == 1 && c.chars().all(|c| c.is_ascii_lowercase()))
}

fn path_ok(p: &str) -> bool {
    !p.is_empty()
        && p.split('.')
            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Every `${...}` placeholder in `v` must be `${selected.<field path>}`.
fn check_templates(v: &toml::Value, at: &str, errors: &mut Vec<String>) {
    match v {
        toml::Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find("${") {
                let after = &rest[i + 2..];
                let Some(end) = after.find('}') else {
                    errors.push(format!("{at}: an unclosed `${{`"));
                    return;
                };
                let inner = &after[..end];
                let ok = inner
                    .strip_prefix("selected.")
                    .or_else(|| inner.strip_prefix("form."))
                    .is_some_and(path_ok)
                    || inner
                        .strip_prefix("value.")
                        .is_some_and(|k| path_ok(k) && !k.contains('.'));
                if !ok {
                    errors.push(format!(
                        "{at}: `${{{inner}}}` is not `${{selected.<field>}}`, `${{form.<field>}}`, or `${{value.<node>}}`"
                    ));
                }
                rest = &after[end + 1..];
            }
        }
        toml::Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                check_templates(x, &format!("{at}[{i}]"), errors);
            }
        }
        toml::Value::Table(t) => {
            for (k, x) in t {
                check_templates(x, &format!("{at}.{k}"), errors);
            }
        }
        _ => {}
    }
}

fn method_info(name: &str) -> Option<&'static api::MethodInfo> {
    api::METHODS.iter().find(|m| m.name == name)
}

/// What a launch must grant a package for it to work: its capabilities,
/// event subscription when it follows events, and its agent's panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needs {
    pub methods: Vec<String>,
    pub panes: Vec<u32>,
    pub all_panes: bool,
}

impl Manifest {
    /// What a launch must grant this package.
    /// The methods the package calls: its capabilities, and `collect.query`
    /// when a data source is a collector.
    pub fn methods(&self) -> Vec<String> {
        let mut methods: Vec<String> = self
            .capabilities
            .iter()
            .filter(|c| *c != PROCESS)
            .cloned()
            .collect();
        let declared = methods
            .iter()
            .any(|c| c == COLLECT_QUERY || c == "collect.*");
        if !declared && self.data.values().any(|s| s.collector.is_some()) {
            methods.push(COLLECT_QUERY.into());
        }
        // Bound nodes: what following their stream, and acting on it,
        // calls.
        let mut bound: Vec<&str> = Vec::new();
        for n in self.nodes.values() {
            match n.kind {
                NodeType::Transcript => bound.extend(TRANSCRIPT_METHODS),
                NodeType::Composer => bound.extend(COMPOSER_METHODS),
                NodeType::Log if n.tap.is_some() => {
                    bound.extend(["taps.subscribe", "taps.unsubscribe"]);
                }
                _ => {}
            }
        }
        for m in bound {
            let service = format!("{}.*", m.rsplit_once('.').map_or(m, |(s, _)| s));
            if !methods.iter().any(|c| c == m || *c == service) {
                methods.push(m.to_owned());
            }
        }
        methods
    }

    /// The panes bound logs tap.
    pub fn tapped_panes(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.nodes.values().filter_map(|n| n.tap).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether a data source is a collector, whose rows come from any pane.
    fn reads_collectors(&self) -> bool {
        self.data.values().any(|s| s.collector.is_some())
    }

    pub fn needs(&self) -> Needs {
        let mut methods = self.methods();
        let follows = self.data.values().any(|s| !s.events.is_empty())
            || self
                .agent
                .as_ref()
                .is_some_and(|a| a.kind == AgentKind::Filter);
        if follows {
            methods.extend(["events.subscribe".into(), "events.unsubscribe".into()]);
        }
        let (mut panes, all_panes) = match &self.agent {
            Some(a) => (a.tools.panes.clone(), a.tools.all_panes || self.all_panes),
            None => (Vec::new(), self.all_panes),
        };
        for p in self.tapped_panes() {
            if !panes.contains(&p) {
                panes.push(p);
            }
        }
        // The panes its `terminal` nodes show.
        if self.is_tree() {
            for p in self.nodes.values().filter_map(|n| n.pane) {
                if !panes.contains(&p) {
                    panes.push(p);
                }
            }
        }
        Needs {
            methods,
            panes,
            all_panes: all_panes || self.reads_collectors(),
        }
    }

    /// Whether a node (a button's or control's `run`, an input's or a
    /// form's `submit`), the version 1 form, or a contributed command runs
    /// the command `name`.
    pub fn runs_command(&self, name: &str) -> bool {
        let named = |c: &Option<String>| c.as_deref() == Some(name);
        named(&self.view.submit)
            || self
                .nodes
                .values()
                .any(|n| named(&n.run) || named(&n.submit))
            || self.contributes.commands.iter().any(|c| named(&c.run))
    }

    /// A process app: its program draws its view.
    pub fn is_process(&self) -> bool {
        self.process.is_some()
    }

    /// A script app: the host runs its `app.ts` and draws its view.
    pub fn is_script(&self) -> bool {
        self.script.is_some()
    }

    /// Its own code draws its view: a process app or a script app.
    pub fn draws_own_view(&self) -> bool {
        self.is_process() || self.is_script()
    }

    /// A web app: version 2, with `[web]` and no view of its own; its
    /// view is its page (`web/index.html`, or the declared `url`).
    pub fn is_web_app(&self) -> bool {
        self.is_tree()
            && self.web.is_some()
            && !self.draws_own_view()
            && (self.root.is_none() || self.root.as_deref() == Some(WEB_PAGE))
            && self.nodes.keys().all(|k| k == WEB_PAGE)
    }

    /// Its view is a tree of nodes (version 2).
    pub fn is_tree(&self) -> bool {
        self.schema_version == TREE_SCHEMA_VERSION
    }

    /// The keys of the nodes reached from a tree's root, parents first.
    pub fn tree_order(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut stack: Vec<&str> = self.root.as_deref().into_iter().collect();
        while let Some(k) = stack.pop() {
            if out.iter().any(|o| o == k) {
                continue;
            }
            let Some(n) = self.nodes.get(k) else { continue };
            out.push(k.to_owned());
            stack.extend(n.children.iter().rev().map(String::as_str));
        }
        out
    }

    /// The data views by key, in tree order: a tree's data nodes, or a
    /// version 1 manifest's one view, keyed `view`.
    pub fn data_views(&self) -> Vec<(String, View)> {
        if !self.is_tree() {
            return vec![("view".into(), self.view.clone())];
        }
        self.tree_order()
            .into_iter()
            .filter_map(|k| {
                let v = self.nodes.get(&k)?.view()?;
                Some((k, v))
            })
            .collect()
    }

    /// The form's view: the version 1 view, or a tree's form node.
    pub fn form_view(&self) -> Option<View> {
        if !self.is_tree() {
            return Some(self.view.clone());
        }
        self.data_views()
            .into_iter()
            .map(|(_, v)| v)
            .find(|v| v.kind == ViewKind::Form)
    }

    /// The data sources the view shows.
    pub fn shown_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let content = self.tree_order().into_iter().filter_map(|k| {
            let n = self.nodes.get(&k)?;
            (n.kind.is_content() || n.kind.is_session()).then(|| n.source.clone())
        });
        let views: Vec<String> = self
            .data_views()
            .into_iter()
            .map(|(_, v)| v.source)
            .collect();
        for source in views.into_iter().chain(content) {
            if self.data.contains_key(&source) && !out.contains(&source) {
                out.push(source);
            }
        }
        out
    }

    /// Parse and validate; every problem, each with where it is.
    pub fn parse(text: &str) -> Result<Manifest, Vec<String>> {
        let raw: toml::Table = toml::from_str(text).map_err(|e| vec![e.to_string()])?;
        let has_view = raw.contains_key("view");
        let mut m: Manifest = toml::from_str(text).map_err(|e| vec![e.to_string()])?;
        let mut errors = Vec::new();
        if m.is_web_app() {
            // The view is the page: one `web` node.
            let page: NodeSpec =
                toml::from_str("type = \"web\"").map_err(|e| vec![e.to_string()])?;
            m.root = Some(WEB_PAGE.into());
            m.nodes.insert(WEB_PAGE.into(), page);
        }
        if m.draws_own_view() {
            if has_view {
                errors.push(
                    "view: a process or script app draws its own view; it has no [view]".into(),
                );
            }
        } else if m.is_tree() {
            if has_view {
                errors.push("view: a tree manifest has `root` and [nodes], not [view]".into());
            }
        } else if !has_view {
            errors.push("view: missing (or use schema_version = 2 with a tree)".into());
        }
        errors.extend(m.validate());
        if errors.is_empty() {
            Ok(m)
        } else {
            Err(errors)
        }
    }

    fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.schema_version != SCHEMA_VERSION && self.schema_version != TREE_SCHEMA_VERSION {
            errors.push(format!(
                "schema_version: {} is not supported (this runtime reads {SCHEMA_VERSION} and {TREE_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        let id_ok = !self.id.is_empty()
            && self.id.split('.').all(|p| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if !id_ok {
            errors.push(format!("id: `{}` is not dotted lowercase", self.id));
        }
        for c in &self.capabilities {
            if c == WEB_DOWNLOAD {
                errors.push(format!(
                    "capabilities: `{c}` is not available yet: downloads in web pages are refused"
                ));
                continue;
            }
            let service = c.strip_suffix(".*");
            let known = match service {
                Some(s) => api::METHODS
                    .iter()
                    .any(|m| m.name.starts_with(&format!("{s}."))),
                None => method_info(c).is_some() || c == api::MANAGE_FILTERS || c == PROCESS,
            };
            if !known {
                errors.push(format!(
                    "capabilities: `{c}` is not an API method or service"
                ));
            }
        }
        for (name, s) in &self.data {
            let at = format!("data.{name}");
            match (&s.collector, s.method.is_empty()) {
                (Some(_), false) => {
                    errors.push(format!("{at}: give a method or a collector, not both"));
                }
                (Some(c), true) => {
                    let label = !c.is_empty()
                        && c.len() <= 64
                        && c.chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch));
                    if !label {
                        errors.push(format!(
                            "{at}.collector: `{c}` is not a collector name (letters, digits, `-`, `_`, `.`)"
                        ));
                    }
                    if s.params.contains_key("name") {
                        errors.push(format!("{at}.params.name: the collector names it"));
                    }
                }
                (None, true) => errors.push(format!("{at}: give a method or a collector")),
                (None, false) => match method_info(&s.method) {
                    None => errors.push(format!("{at}.method: no method `{}`", s.method)),
                    Some(m) if m.mutation => errors.push(format!(
                        "{at}.method: `{}` changes things; data comes from queries",
                        s.method
                    )),
                    Some(_) => {}
                },
            }
            if s.path.as_deref().is_some_and(|p| !path_ok(p)) {
                errors.push(format!("{at}.path: not dotted field names"));
            }
            if s.refresh_ms.is_some_and(|ms| ms < 200) {
                errors.push(format!("{at}.refresh_ms: at least 200"));
            }
            if s.limit == 0 || s.limit > 10_000 {
                errors.push(format!("{at}.limit: 1 to 10000"));
            }
            check_templates(
                &toml::Value::Table(s.params.clone()),
                &format!("{at}.params"),
                &mut errors,
            );
            if s.params
                .values()
                .any(|v| matches!(v, toml::Value::String(x) if x.contains("${")))
            {
                errors.push(format!("{at}.params: placeholders belong to commands"));
            }
        }
        if self.is_process() && self.is_script() {
            errors.push("script: a package is a process app or a script app, not both".into());
        } else if self.is_script() {
            self.validate_script(&mut errors);
        } else if self.is_process() {
            self.validate_process(&mut errors);
        } else if self.capabilities.iter().any(|c| c == PROCESS) {
            errors.push("capabilities: `process` is for a package with a [process] command".into());
        } else if self.is_tree() {
            self.validate_tree(&mut errors);
        } else {
            self.validate_agent(&mut errors);
            if self.root.is_some() || !self.nodes.is_empty() {
                errors.push("root, nodes: a tree needs schema_version = 2".into());
            }
            self.validate_view(&self.view, "view", &mut errors);
        }
        if let Some(w) = &self.web {
            self.validate_web(w, &mut errors);
        }
        if let Some(m) = &self.module
            && (m.is_empty() || m.contains("..") || m.starts_with('/'))
        {
            errors.push(format!("module: `{m}` must be a file beside the manifest"));
        }
        let mut keys = BTreeMap::new();
        for (name, c) in &self.commands {
            let at = format!("commands.{name}");
            if c.key.is_empty() {
                if !self.runs_command(name) {
                    errors.push(format!(
                        "{at}.key: a command needs a key, or a node or form that runs it"
                    ));
                }
            } else if !key_ok(&c.key) {
                errors.push(format!("{at}.key: `{}` is not a key", c.key));
            } else if RESERVED_KEYS.contains(&c.key.as_str()) {
                errors.push(format!("{at}.key: `{}` is the runtime's", c.key));
            } else if let Some(other) = keys.insert(c.key.clone(), name.clone()) {
                errors.push(format!("{at}.key: `{}` is also {other}'s", c.key));
            }
            match (&c.method, &c.handler) {
                (Some(m), None) => {
                    if method_info(m).is_none() {
                        errors.push(format!("{at}.method: no method `{m}`"));
                    }
                }
                (None, Some(h)) => {
                    if self.module.is_none() {
                        errors.push(format!("{at}.handler: `{h}` needs a module"));
                    }
                    if !path_ok(h) || h.contains('.') {
                        errors.push(format!("{at}.handler: `{h}` is not a function name"));
                    }
                }
                _ => errors.push(format!("{at}: give a method or a handler, not both")),
            }
            check_templates(
                &toml::Value::Table(c.params.clone()),
                &format!("{at}.params"),
                &mut errors,
            );
        }
        self.validate_contributions(&mut errors);
        errors
    }

    /// A view's own checks: a version 1 `[view]`, or a tree's data node
    /// at `at`.
    fn validate_view(&self, v: &View, at: &str, errors: &mut Vec<String>) {
        let sourceless =
            matches!(v.kind, ViewKind::Form) || (v.kind == ViewKind::Agent && v.source.is_empty());
        if !sourceless && !self.data.contains_key(&v.source) {
            errors.push(format!("{at}.source: no data source `{}`", v.source));
        }
        if v.kind == ViewKind::Form {
            if v.fields.is_empty() {
                errors.push(format!("{at}.fields: a form needs fields"));
            }
            match &v.submit {
                None => errors.push(format!("{at}.submit: a form needs a submit command")),
                Some(c) if !self.commands.contains_key(c) => {
                    errors.push(format!("{at}.submit: no command `{c}`"));
                }
                Some(_) => {}
            }
            let mut names = std::collections::BTreeSet::new();
            for f in &v.fields {
                if !path_ok(&f.name) || f.name.contains('.') || !names.insert(f.name.clone()) {
                    errors.push(format!(
                        "{at}.fields: `{}` is not a unique field name",
                        f.name
                    ));
                }
            }
        }
        if v.kind == ViewKind::Table && v.columns.is_empty() {
            errors.push(format!("{at}.columns: a table needs columns"));
        }
        for c in v
            .columns
            .iter()
            .chain(v.row_key.iter())
            .chain(v.field.iter())
        {
            if !path_ok(c) {
                errors.push(format!("{at}: `{c}` is not a dotted field name"));
            }
        }
    }

    /// A process app has a program, the `process` capability, and none of
    /// the parts its program replaces.
    fn validate_web(&self, w: &WebSpec, errors: &mut Vec<String>) {
        if !self.is_tree() {
            errors.push("web: web pages need schema_version = 2".into());
        }
        if let Some(u) = &w.url
            && !view_tree::web::is_local(u)
        {
            errors.push(format!(
                "web.url: `{u}` is not a local address (http or https on localhost, 127.0.0.1, or [::1])"
            ));
        }
        for a in &w.allow {
            let origin = view_tree::web::origin(a);
            let web = a.starts_with("https://") || a.starts_with("http://");
            if !web || origin.as_deref() != Some(a.trim_end_matches('/')) {
                errors.push(format!(
                    "web.allow: `{a}` is not an origin (`https://example.com`, no path)"
                ));
            }
        }
    }

    fn validate_process(&self, errors: &mut Vec<String>) {
        let Some(p) = &self.process else { return };
        if !self.capabilities.iter().any(|c| c == PROCESS) {
            errors.push(
                "process: running a program needs the `process` capability; add \"process\" to capabilities"
                    .into(),
            );
        }
        if self.schema_version != TREE_SCHEMA_VERSION {
            errors.push(format!(
                "schema_version: a process app draws a view tree; use {TREE_SCHEMA_VERSION}"
            ));
        }
        match p.command.first() {
            None => errors.push("process.command: give the program, then its arguments".into()),
            Some(prog) if prog.is_empty() || prog.split('/').any(|s| s == "..") => {
                errors.push(format!(
                    "process.command: `{prog}` is not a program in the package or on PATH"
                ));
            }
            Some(_) => {}
        }
        if p.command.iter().any(|a| a.contains('\0')) {
            errors.push("process.command: an argument holds a NUL".into());
        }
        let parts = [
            ("data", !self.data.is_empty()),
            ("root", self.root.is_some()),
            ("nodes", !self.nodes.is_empty()),
            ("commands", !self.commands.is_empty()),
            ("module", self.module.is_some()),
            ("agent", self.agent.is_some()),
            (
                "contributes",
                !(self.contributes.commands.is_empty()
                    && self.contributes.sidebar.is_empty()
                    && self.contributes.status.is_empty()),
            ),
        ];
        for (name, there) in parts {
            if there {
                errors.push(format!(
                    "{name}: a process app's program does this; remove it"
                ));
            }
        }
    }

    /// A script app names its source beside the manifest, and has none of
    /// the parts its code replaces.
    fn validate_script(&self, errors: &mut Vec<String>) {
        let Some(s) = &self.script else { return };
        if self.schema_version != TREE_SCHEMA_VERSION {
            errors.push(format!(
                "schema_version: a script app draws a view tree; use {TREE_SCHEMA_VERSION}"
            ));
        }
        if app_protocol::build::kind(&s.source).is_none()
            || s.source.starts_with('/')
            || s.source.split('/').any(|p| p == ".." || p.is_empty())
        {
            errors.push(format!(
                "script.source: `{}` must be a .ts or .js file in the package",
                s.source
            ));
        }
        if self.capabilities.iter().any(|c| c == PROCESS) {
            errors.push("capabilities: `process` is for a package with a [process] command".into());
        }
        let parts = [
            ("data", !self.data.is_empty()),
            ("root", self.root.is_some()),
            ("nodes", !self.nodes.is_empty()),
            ("commands", !self.commands.is_empty()),
            ("module", self.module.is_some()),
            ("agent", self.agent.is_some()),
            (
                "contributes",
                !(self.contributes.commands.is_empty()
                    && self.contributes.sidebar.is_empty()
                    && self.contributes.status.is_empty()),
            ),
        ];
        for (name, there) in parts {
            if there {
                errors.push(format!("{name}: a script app's code does this; remove it"));
            }
        }
    }

    fn validate_tree(&self, errors: &mut Vec<String>) {
        if self.agent.is_some() {
            errors.push("agent: an agentic pane has a version 1 `[view]`".into());
        }
        let Some(root) = &self.root else {
            errors.push("root: a tree manifest names its root node".into());
            return;
        };
        if !self.nodes.contains_key(root) {
            errors.push(format!("root: no node `{root}`"));
        }
        let mut parents: BTreeMap<&str, usize> = BTreeMap::new();
        let mut forms = 0;
        for (key, n) in &self.nodes {
            let at = format!("nodes.{key}");
            let key_ok = !key.is_empty()
                && key.len() <= 64
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !key_ok {
                errors.push(format!(
                    "{at}: a node key is lowercase letters, digits, `-`, and `_`"
                ));
            }
            for c in &n.children {
                *parents.entry(c.as_str()).or_default() += 1;
                if !self.nodes.contains_key(c) {
                    errors.push(format!("{at}.children: no node `{c}`"));
                }
            }
            if n.kind.is_layout() {
                if n.children.is_empty() {
                    errors.push(format!("{at}.children: a {} needs children", n.kind.name()));
                }
            } else {
                if !n.children.is_empty() {
                    errors.push(format!("{at}.children: a {} has none", n.kind.name()));
                }
                if n.kind == NodeType::Form {
                    forms += 1;
                }
                if let Some(v) = n.view() {
                    self.validate_view(&v, &at, errors);
                }
                self.validate_component(n, &at, errors);
                self.validate_bound(n, &at, errors);
            }
            if !n.sizes.is_empty()
                && (n.kind != NodeType::Split
                    || n.sizes.len() != n.children.len()
                    || n.sizes.contains(&0))
            {
                errors.push(format!(
                    "{at}.sizes: a split has one size above 0 per child"
                ));
            }
        }
        if forms > 1 {
            errors.push("nodes: one form per app".into());
        }
        for (key, count) in &parents {
            if *count > 1 {
                errors.push(format!("nodes.{key}: a node has one parent"));
            }
            if *key == root.as_str() {
                errors.push(format!("nodes.{key}: the root has no parent"));
            }
        }
        // Every node hangs from the root, once: the nodes form a tree.
        let reached = self.tree_order();
        for key in self.nodes.keys() {
            if !reached.contains(key) {
                errors.push(format!("nodes.{key}: not reached from the root"));
            }
        }
        for (name, c) in &self.commands {
            if TREE_RESERVED_KEYS.contains(&c.key.as_str()) {
                errors.push(format!(
                    "commands.{name}.key: `{}` moves the focus or the tab in a tree",
                    c.key
                ));
            }
        }
    }

    /// A transcript's, composer's, or tapped log's own checks: where its
    /// stream comes from.
    fn validate_bound(&self, n: &NodeSpec, at: &str, errors: &mut Vec<String>) {
        let kind = n.kind.name();
        if n.kind.is_session() {
            match (&n.session, n.source.is_empty()) {
                (Some(_), false) => {
                    errors.push(format!(
                        "{at}: a {kind} has `session` or a `source`, not both"
                    ));
                }
                (None, true) => {
                    errors.push(format!(
                        "{at}: a {kind} needs `session` or a `source` (its `field` is the session id)"
                    ));
                }
                (Some(s), true) if s.is_empty() => {
                    errors.push(format!("{at}.session: an agent session id"));
                }
                (None, false) if !self.data.contains_key(&n.source) => {
                    errors.push(format!("{at}.source: no data source `{}`", n.source));
                }
                _ => {}
            }
            if let Some(f) = &n.field
                && !path_ok(f)
            {
                errors.push(format!("{at}: `{f}` is not a dotted field name"));
            }
        } else if n.session.is_some() {
            errors.push(format!("{at}.session: a {kind} is not bound to a session"));
        }
        if n.tap.is_some() {
            if n.kind != NodeType::Log {
                errors.push(format!("{at}.tap: only a log is bound to a tap"));
            } else if !n.source.is_empty() {
                errors.push(format!("{at}: a log has `tap` or a `source`, not both"));
            }
        }
    }

    /// A text, input, or status component's own checks.
    fn validate_component(&self, n: &NodeSpec, at: &str, errors: &mut Vec<String>) {
        let kind = n.kind.name();
        if n.kind.is_content() {
            let given = match n.kind {
                NodeType::Progress => n.ratio.is_some(),
                NodeType::Status => !n.items.is_empty(),
                _ => n.text.is_some(),
            };
            if n.source.is_empty() && !given {
                let what = match n.kind {
                    NodeType::Progress => "ratio",
                    NodeType::Status => "items",
                    _ => "text",
                };
                errors.push(format!("{at}: a {kind} needs `{what}` or a `source`"));
            }
            if !n.source.is_empty() && !self.data.contains_key(&n.source) {
                errors.push(format!("{at}.source: no data source `{}`", n.source));
            }
            if n.ratio.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
                errors.push(format!("{at}.ratio: 0 to 1"));
            }
            if let Some(f) = &n.field
                && !path_ok(f)
            {
                errors.push(format!("{at}: `{f}` is not a dotted field name"));
            }
        }
        if n.kind.is_control() && !n.source.is_empty() {
            errors.push(format!("{at}.source: a {kind} holds its own value"));
        }
        let embed = matches!(n.kind, NodeType::Terminal | NodeType::Image | NodeType::Web);
        if embed && !n.source.is_empty() {
            errors.push(format!("{at}.source: a {kind} shows no data source"));
        }
        if n.pane.is_some() && n.kind != NodeType::Terminal {
            errors.push(format!("{at}.pane: only a terminal shows a pane"));
        }
        if n.src.is_some() && !matches!(n.kind, NodeType::Image | NodeType::Web) {
            errors.push(format!("{at}.src: only an image or a web page has a `src`"));
        }
        match n.kind {
            NodeType::Terminal => {
                if n.pane.is_none() {
                    errors.push(format!("{at}.pane: a terminal names the pane it shows"));
                }
                let reads = self
                    .methods()
                    .iter()
                    .any(|c| c == api::method::TERMINAL_READ || c == "terminal.*");
                if !reads {
                    errors.push(format!(
                        "{at}: a terminal reads its pane; add `{}` to `capabilities`",
                        api::method::TERMINAL_READ
                    ));
                }
            }
            NodeType::Image => {
                if let Some(why) = view_tree::image::src_problem(n.src.as_deref().unwrap_or("")) {
                    errors.push(format!("{at}.src: {why}"));
                }
            }
            NodeType::Web => {
                if let Err(why) = crate::embeds::web_page(
                    &crate::embeds::Site::of(self),
                    n.src.as_deref().unwrap_or(""),
                ) {
                    errors.push(format!("{at}.src: {why}"));
                }
            }
            _ => {}
        }
        match n.kind {
            NodeType::Button if n.label.as_deref().is_none_or(str::is_empty) => {
                errors.push(format!("{at}.label: a button needs a label"));
            }
            NodeType::Toggle if n.label.as_deref().is_none_or(str::is_empty) => {
                errors.push(format!("{at}.label: a toggle needs a label"));
            }
            NodeType::Select => {
                if n.options.is_empty() {
                    errors.push(format!("{at}.options: a select needs options"));
                }
                if let Some(v) = &n.value
                    && !n.options.contains(v)
                {
                    errors.push(format!("{at}.value: `{v}` is not one of the options"));
                }
            }
            _ => {}
        }
        let mut runs = Vec::new();
        if let Some(c) = &n.run {
            if !matches!(
                n.kind,
                NodeType::Button | NodeType::Select | NodeType::Toggle
            ) {
                errors.push(format!("{at}.run: a {kind} runs nothing"));
            }
            runs.push(("run", c));
        }
        if let Some(c) = &n.submit
            && n.kind == NodeType::Input
        {
            runs.push(("submit", c));
        }
        for (what, c) in runs {
            if !self.commands.contains_key(c) {
                errors.push(format!("{at}.{what}: no command `{c}`"));
            }
        }
    }

    fn validate_contributions(&self, errors: &mut Vec<String>) {
        let c = &self.contributes;
        if c.version != CONTRIBUTES_VERSION {
            errors.push(format!(
                "contributes.version: {} is not supported (this runtime reads {CONTRIBUTES_VERSION})",
                c.version
            ));
        }
        let ids = |kind: &str, list: Vec<&str>, errors: &mut Vec<String>| {
            let mut seen = std::collections::BTreeSet::new();
            for id in list {
                if !contribution_id_ok(id) {
                    errors.push(format!(
                        "contributes.{kind}: `{id}` is not lowercase letters, digits, and -"
                    ));
                } else if !seen.insert(id) {
                    errors.push(format!("contributes.{kind}: `{id}` is given twice"));
                }
            }
        };
        ids(
            "commands",
            c.commands.iter().map(|x| x.id.as_str()).collect(),
            errors,
        );
        ids(
            "sidebar",
            c.sidebar.iter().map(|x| x.id.as_str()).collect(),
            errors,
        );
        ids(
            "status",
            c.status.iter().map(|x| x.id.as_str()).collect(),
            errors,
        );
        let mut keys = BTreeMap::new();
        for x in &c.commands {
            let at = format!("contributes.commands.{}", x.id);
            match (&x.run, x.opens.as_deref()) {
                (Some(name), None) => match self.commands.get(name) {
                    None => errors.push(format!("{at}.run: no command `{name}`")),
                    Some(cmd) if cmd.method.is_none() => errors.push(format!(
                        "{at}.run: `{name}` has a handler; a contributed command runs a method"
                    )),
                    Some(cmd) => {
                        if toml::Value::Table(cmd.params.clone())
                            .to_string()
                            .contains("${")
                        {
                            errors.push(format!(
                                "{at}.run: `{name}` uses placeholders, and the desktop has no selection or form to fill them"
                            ));
                        }
                    }
                },
                (None, Some("panel")) => {}
                (None, Some(other)) => {
                    errors.push(format!("{at}.opens: `{other}` is not `panel`"));
                }
                _ => errors.push(format!("{at}: give `run` or `opens`, not both")),
            }
            if let Some(k) = &x.key {
                if !binding_ok(k) {
                    errors.push(format!(
                        "{at}.key: `{k}` is not a binding with cmd, ctrl, or alt and one key"
                    ));
                } else if let Some(other) = keys.insert(k.clone(), x.id.clone()) {
                    errors.push(format!("{at}.key: `{k}` is also {other}'s"));
                }
            }
        }
        for x in &c.status {
            let at = format!("contributes.status.{}", x.id);
            if !self.data.contains_key(&x.source) {
                errors.push(format!("{at}.source: no data source `{}`", x.source));
            }
            if let Some(f) = &x.field
                && !path_ok(f)
            {
                errors.push(format!("{at}.field: `{f}` is not a dotted field name"));
            }
        }
    }
}

impl Manifest {
    fn validate_agent(&self, errors: &mut Vec<String>) {
        let Some(a) = &self.agent else {
            if self.view.kind == ViewKind::Agent {
                errors.push("view: an agent view needs an [agent] section".into());
            }
            return;
        };
        if self.view.kind != ViewKind::Agent {
            errors.push("agent: needs `view.type = \"agent\"`".into());
        }
        if let Some(p) = &a.provider
            && !matches!(p.as_str(), "claude" | "codex")
        {
            errors.push(format!("agent.provider: `{p}` is not claude or codex"));
        }
        if a.kind == AgentKind::Monitor && self.view.source.is_empty() {
            errors.push("view.source: a monitor watches a data source".into());
        }
        let has = |m: &str| a.tools.methods.iter().any(|x| x == m);
        if a.kind == AgentKind::Filter && !(has(api::MANAGE_FILTERS) && has("terminal.query")) {
            errors.push(format!(
                "agent.tools.methods: a filter pane needs `terminal.query` and `{}`",
                api::MANAGE_FILTERS
            ));
        }
        for m in &a.tools.methods {
            if m == api::MANAGE_FILTERS {
                if a.kind != AgentKind::Filter {
                    errors.push(format!("agent.tools.methods: only a filter pane has `{m}`"));
                }
                if !self.capabilities.iter().any(|c| c == m) {
                    errors.push(format!(
                        "agent.tools.methods: `{m}` is not among the package's capabilities"
                    ));
                }
                continue;
            }
            let Some(info) = method_info(m) else {
                errors.push(format!("agent.tools.methods: no method `{m}`"));
                continue;
            };
            let declared = self.capabilities.iter().any(|c| {
                c == m
                    || c.strip_suffix(".*")
                        .is_some_and(|svc| m.starts_with(&format!("{svc}.")))
            });
            if !declared {
                errors.push(format!(
                    "agent.tools.methods: `{m}` is not among the package's capabilities"
                ));
            }
            if a.kind.read_only() && (info.mutation || m == "terminal.input") {
                errors.push(format!(
                    "agent.tools.methods: `{m}` changes things, and {} panes only read",
                    a.kind.name()
                ));
            }
        }
        if a.timeout_s == Some(0) || a.max_turns == Some(0) {
            errors.push("agent: timeout_s and max_turns are at least 1".into());
        }
    }
}

/// TOML to JSON, for API params.
pub fn to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => serde_json::Value::Array(a.iter().map(to_json).collect()),
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
    }
}

/// The value at a dotted `path` in `v`.
pub fn field<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(v, |v, k| v.get(k))
}

/// Fill a command's params: a string that is exactly one placeholder
/// becomes the field's value with its type; placeholders inside longer
/// strings become the value's text. A missing field is an error, never an
/// empty substitution.
pub fn fill(
    params: &toml::Table,
    selected: Option<&serde_json::Value>,
    form: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    fill_with(params, selected, form, None)
}

/// [`fill`], with the values of a tree's input components by node key
/// for `${value.<node>}`.
pub fn fill_with(
    params: &toml::Table,
    selected: Option<&serde_json::Value>,
    form: Option<&serde_json::Value>,
    values: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    type Ctx<'a> = (
        Option<&'a serde_json::Value>,
        Option<&'a serde_json::Value>,
        Option<&'a serde_json::Value>,
    );
    fn lookup(inner: &str, (row, form, values): Ctx) -> Result<serde_json::Value, String> {
        if let Some(key) = inner.strip_prefix("value.") {
            return values
                .and_then(|v| v.get(key))
                .cloned()
                .ok_or_else(|| format!("no input component `{key}`"));
        }
        if let Some(path) = inner.strip_prefix("form.") {
            let form = form.ok_or("there is no form")?;
            return field(form, path)
                .cloned()
                .ok_or_else(|| format!("the form has no `{path}`"));
        }
        let row = row.ok_or("nothing is selected")?;
        let path = inner.strip_prefix("selected.").unwrap_or(inner);
        field(row, path)
            .cloned()
            .ok_or_else(|| format!("the selected row has no `{path}`"))
    }
    fn go(v: &serde_json::Value, ctx: Ctx) -> Result<serde_json::Value, String> {
        Ok(match v {
            serde_json::Value::String(s) if s.contains("${") => {
                if let Some(inner) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}'))
                    && !inner.contains("${")
                {
                    return lookup(inner, ctx);
                }
                let mut out = String::new();
                let mut rest = s.as_str();
                while let Some(i) = rest.find("${") {
                    out.push_str(&rest[..i]);
                    let after = &rest[i + 2..];
                    let end = after.find('}').ok_or("an unclosed placeholder")?;
                    match lookup(&after[..end], ctx)? {
                        serde_json::Value::String(x) => out.push_str(&x),
                        other => out.push_str(&other.to_string()),
                    }
                    rest = &after[end + 1..];
                }
                out.push_str(rest);
                serde_json::Value::String(out)
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(|x| go(x, ctx)).collect::<Result<_, _>>()?)
            }
            serde_json::Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, x)| go(x, ctx).map(|x| (k.clone(), x)))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }
    go(
        &to_json(&toml::Value::Table(params.clone())),
        (selected, form, values),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
schema_version = 1
id = "local.panes"
version = "0.1.0"
title = "Panes"
capabilities = ["tmux.topology", "actions.*"]

[data.panes]
method = "tmux.topology"
path = "panes"
events = ["tmux.topology"]

[view]
type = "table"
source = "panes"
row_key = "id"
columns = ["id", "title"]

[commands.rename]
key = "r"
method = "actions.submit"
params = { action = { rename_window = { window = "${selected.window}", name = "w-${selected.id}" } } }
"#;

    const PROCESS_APP: &str = r#"
schema_version = 2
id = "local.counter"
version = "0.1.0"
title = "Counter"
capabilities = ["process", "tmux.topology"]

[process]
command = ["./bin/counter", "--fast"]
"#;

    #[test]
    fn a_script_app_names_its_source_and_nothing_its_code_replaces() {
        let script = r#"
schema_version = 2
id = "local.panes"
version = "0.1.0"
title = "Panes"
capabilities = ["tmux.topology"]

[script]
source = "app.ts"
"#;
        let m = Manifest::parse(script).expect("valid");
        assert!(m.is_script() && m.draws_own_view() && !m.is_process());
        assert_eq!(m.methods(), ["tmux.topology"]);
        for (bad, why) in [
            (
                script.replace("app.ts", "../app.ts"),
                "must be a .ts or .js file",
            ),
            (
                script.replace("app.ts", "app.py"),
                "must be a .ts or .js file",
            ),
            (
                script.replace("\"tmux.topology\"", "\"process\""),
                "`process` is for a package with a [process] command",
            ),
            (
                format!("{script}[process]\ncommand = [\"./x\"]\n"),
                "not both",
            ),
        ] {
            let errors = Manifest::parse(&bad).expect_err("invalid");
            assert!(errors.iter().any(|e| e.contains(why)), "{errors:?}");
        }
        let with_data = script.replace(
            "[script]",
            "[data.p]\nmethod = \"tmux.topology\"\n\n[script]",
        );
        let errors = Manifest::parse(&with_data).expect_err("invalid");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("data: a script app's code does this")),
            "{errors:?}"
        );
    }

    #[test]
    fn downloads_are_not_a_capability_yet() {
        let m = PROCESS_APP.replace("\"process\", ", "\"process\", \"web.download\", ");
        let errors = Manifest::parse(&m).expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("`web.download` is not available yet")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_process_app_needs_the_process_capability() {
        let m = Manifest::parse(PROCESS_APP).expect("valid");
        assert!(m.is_process());
        // `process` is not an API method: the grant does not carry it.
        assert_eq!(m.needs().methods, ["tmux.topology"]);

        let without = PROCESS_APP.replace("\"process\", ", "");
        let errors = Manifest::parse(&without).expect_err("refused");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("needs the `process` capability")),
            "{errors:?}"
        );

        let mixed = format!("{PROCESS_APP}\n[view]\ntype = \"text\"\nsource = \"x\"\n");
        let errors = Manifest::parse(&mixed).expect_err("refused");
        assert!(errors.iter().any(|e| e.starts_with("view:")), "{errors:?}");

        let escape = PROCESS_APP.replace("./bin/counter", "../counter");
        assert!(Manifest::parse(&escape).is_err());

        let stray = GOOD.replace("\"tmux.topology\", \"actions.*\"", "\"process\"");
        let errors = Manifest::parse(&stray).expect_err("refused");
        assert!(
            errors.iter().any(|e| e.contains("[process] command")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_manifest_parses_and_fills_typed_placeholders() {
        let m = Manifest::parse(GOOD).expect("valid");
        let row = serde_json::json!({ "id": 3, "window": 7, "title": "x" });
        let p = fill(&m.commands["rename"].params, Some(&row), None).expect("fill");
        assert_eq!(
            p,
            serde_json::json!({ "action": { "rename_window": { "window": 7, "name": "w-3" } } })
        );
        assert!(fill(&m.commands["rename"].params, None, None).is_err());
        let missing = serde_json::json!({ "id": 3 });
        assert!(
            fill(&m.commands["rename"].params, Some(&missing), None)
                .expect_err("missing field")
                .contains("window")
        );
    }

    #[test]
    fn mistakes_are_reported_with_where_they_are() {
        let bad = GOOD
            .replace("schema_version = 1", "schema_version = 9")
            .replace("method = \"tmux.topology\"", "method = \"daemon.shutdown\"")
            .replace("key = \"r\"", "key = \"q\"")
            .replace("${selected.window}", "${os.execute}");
        let errors = Manifest::parse(&bad).expect_err("invalid");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(has("schema_version: 9"), "{errors:?}");
        assert!(
            has("data.panes.method: `daemon.shutdown` changes things"),
            "{errors:?}"
        );
        assert!(
            has("commands.rename.key: `q` is the runtime's"),
            "{errors:?}"
        );
        assert!(has("is not `${selected.<field>}`"), "{errors:?}");
        let typo = GOOD.replace("row_key", "rowkey");
        assert!(Manifest::parse(&typo).is_err(), "unknown keys are errors");
    }

    #[test]
    fn an_inspector_or_monitor_only_reads() {
        let agent = |kind: &str, methods: &str| {
            format!(
                "schema_version = 1\nid = \"t.a\"\nversion = \"1\"\ntitle = \"A\"\n\
                 capabilities = [\"tmux.*\", \"actions.submit\", \"terminal.input\"]\n\
                 [view]\ntype = \"agent\"\n[agent]\nkind = \"{kind}\"\ntools = {{ methods = [{methods}] }}\n"
            )
        };
        assert!(Manifest::parse(&agent("inspector", "\"tmux.topology\"")).is_ok());
        assert!(Manifest::parse(&agent("operator", "\"actions.submit\"")).is_ok());
        let errors = Manifest::parse(&agent(
            "inspector",
            "\"actions.submit\", \"terminal.input\"",
        ))
        .expect_err("an inspector that acts");
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.contains("changes things, and inspector panes only read"))
                .count(),
            2,
            "{errors:?}"
        );
        let errors = Manifest::parse(&agent("operator", "\"jobs.start\"")).expect_err("undeclared");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("not among the package's capabilities")),
            "{errors:?}"
        );
        let errors =
            Manifest::parse(&agent("monitor", "")).expect_err("a monitor without a source");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("a monitor watches a data source")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_filter_pane_reads_and_manages_filters_and_nothing_else_does() {
        let pkg = |kind: &str, methods: &str| {
            format!(
                "schema_version = 1\nid = \"t.f\"\nversion = \"1\"\ntitle = \"F\"\n\
                 capabilities = [\"terminal.query\", \"filters.manage\", \"terminal.input\"]\n\
                 [view]\ntype = \"agent\"\n[agent]\nkind = \"{kind}\"\n\
                 tools = {{ methods = [{methods}], all_panes = true }}\n"
            )
        };
        let both = "\"terminal.query\", \"filters.manage\"";
        assert!(Manifest::parse(&pkg("filter", both)).is_ok());
        let errors = Manifest::parse(&pkg("filter", "\"terminal.query\""))
            .expect_err("a filter pane without filters.manage");
        assert!(
            errors.iter().any(|e| e.contains("a filter pane needs")),
            "{errors:?}"
        );
        let errors = Manifest::parse(&pkg("filter", &format!("{both}, \"terminal.input\"")))
            .expect_err("a filter pane that types");
        assert!(
            errors.iter().any(|e| e.contains("filter panes only read")),
            "{errors:?}"
        );
        let errors =
            Manifest::parse(&pkg("inspector", both)).expect_err("an inspector with filters");
        assert!(
            errors.iter().any(|e| e.contains("only a filter pane has")),
            "{errors:?}"
        );
    }

    const CONTRIBUTING: &str = r#"
[commands.first]
key = "f"
method = "actions.submit"
params = { action = { kind = { rename_window = { window = 1, name = "first" } } } }

[[contributes.commands]]
id = "rename-first"
title = "Rename the first window"
run = "first"
key = "cmd-alt-r"

[[contributes.commands]]
id = "open"
title = "Open panes"
opens = "panel"

[[contributes.sidebar]]
id = "panes"
title = "Panes"

[[contributes.status]]
id = "count"
label = "panes"
source = "panes"
"#;

    #[test]
    fn contributions_name_the_package_s_own_commands_and_sources() {
        let m = Manifest::parse(&format!("{GOOD}{CONTRIBUTING}")).expect("valid");
        assert_eq!(m.contributes.commands.len(), 2);
        assert_eq!(m.contributes.status[0].source, "panes");

        let bad = format!("{GOOD}{CONTRIBUTING}")
            .replace("run = \"first\"", "run = \"rename\"")
            .replace("key = \"cmd-alt-r\"", "key = \"r\"")
            .replace("opens = \"panel\"", "opens = \"window\"")
            .replace("source = \"panes\"\n\"#", "source = \"nothing\"\n\"#")
            .replace("id = \"panes\"", "id = \"Panes\"");
        let errors = Manifest::parse(&bad).expect_err("invalid");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(
            has("contributes.commands.rename-first.run: `rename` uses placeholders"),
            "{errors:?}"
        );
        assert!(
            has("contributes.commands.rename-first.key: `r` is not a binding"),
            "{errors:?}"
        );
        assert!(
            has("contributes.commands.open.opens: `window`"),
            "{errors:?}"
        );
        assert!(has("contributes.sidebar: `Panes`"), "{errors:?}");
    }

    #[test]
    fn bindings_need_a_modifier_that_keeps_them_from_typing() {
        assert!(binding_ok("cmd-alt-r"));
        assert!(binding_ok("ctrl-shift-f5"));
        assert!(!binding_ok("shift-r"));
        assert!(!binding_ok("r"));
        assert!(!binding_ok("cmd-cmd-r"));
        assert!(!binding_ok("hyper-r"));
    }

    #[test]
    fn a_collector_is_a_data_source() {
        let head = "schema_version = 1\nid = \"t.c\"\nversion = \"1\"\ntitle = \"T\"\n";
        let view = "[view]\ntype = \"table\"\nsource = \"f\"\ncolumns = [\"test\"]\n";
        let m = Manifest::parse(&format!(
            "{head}[data.f]\ncollector = \"test-failures\"\nevents = [\"collect.row\"]\n{view}"
        ))
        .expect("valid");
        let n = m.needs();
        assert!(n.methods.iter().any(|x| x == COLLECT_QUERY), "{n:?}");
        assert!(n.all_panes);
        let s = &m.data["f"];
        assert_eq!(s.method_name(), COLLECT_QUERY);
        assert_eq!(s.call_params()["name"].as_str(), Some("test-failures"));
        assert_eq!(s.call_params()["limit"].as_integer(), Some(500));
        assert_eq!(s.rows_path().as_deref(), Some("rows"));
        let m = Manifest::parse(&format!(
            "{head}[data.f]\ncollector = \"errors\"\nparams = {{ group_by = \"severity\" }}\n{view}"
        ))
        .expect("valid");
        assert_eq!(m.data["f"].rows_path().as_deref(), Some("groups"));
        let errors = |src: &str| {
            Manifest::parse(&format!("{head}[data.f]\n{src}\n{view}")).expect_err("invalid")
        };
        assert!(errors("collector = \"a\"\nmethod = \"tmux.topology\"")[0].contains("not both"));
        assert!(errors("params = {}")[0].contains("give a method or a collector"));
        assert!(errors("collector = \"a b\"")[0].contains("not a collector name"));
        assert!(
            errors("collector = \"a\"\nparams = { name = \"b\" }")[0]
                .contains("the collector names it")
        );
        let row = collector_row(serde_json::json!({
            "id": 3,
            "pane": 1,
            "fields": [{ "name": "test", "value": "a::b" }, { "name": "pane", "value": "x" }],
        }));
        assert_eq!(row["test"], "a::b");
        assert_eq!(row["pane"], 1);
        assert_eq!(row["fields"]["pane"], "x");
    }

    const TREE: &str = r#"
schema_version = 2
id = "local.tree"
version = "0.1.0"
title = "Tree"
root = "main"

[data.panes]
method = "tmux.topology"
path = "panes"

[nodes.main]
type = "column"
children = ["head", "panes"]

[nodes.head]
type = "text"
source = "panes"
field = "id"

[nodes.panes]
type = "table"
source = "panes"
row_key = "id"
columns = ["id"]
"#;

    #[test]
    fn a_tree_manifest_parses_and_names_its_mistakes() {
        let m = Manifest::parse(TREE).expect("valid");
        assert!(m.is_tree());
        assert_eq!(m.tree_order(), ["main", "head", "panes"]);
        assert_eq!(m.shown_sources(), ["panes"]);

        let bad = TREE
            .replace(
                r#"children = ["head", "panes"]"#,
                r#"children = ["head", "gone"]"#,
            )
            .replace("columns = [\"id\"]", "columns = []")
            + "\n[nodes.extra]\ntype = \"row\"\n\n[view]\ntype = \"text\"\nsource = \"panes\"\n\n[commands.next]\nkey = \"Tab\"\nmethod = \"tmux.topology\"\n";
        let errors = Manifest::parse(&bad).expect_err("invalid");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(has("nodes.main.children: no node `gone`"), "{errors:?}");
        assert!(
            has("nodes.panes.columns: a table needs columns"),
            "{errors:?}"
        );
        assert!(
            has("nodes.extra.children: a row needs children"),
            "{errors:?}"
        );
        assert!(has("nodes.extra: not reached from the root"), "{errors:?}");
        assert!(has("view: a tree manifest has `root`"), "{errors:?}");
        assert!(
            has("commands.next.key: `Tab` moves the focus"),
            "{errors:?}"
        );

        let shared = TREE.replace(
            r#"children = ["head", "panes"]"#,
            r#"children = ["head", "panes", "head"]"#,
        );
        let errors = Manifest::parse(&shared).expect_err("a node with two parents");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("nodes.head: a node has one parent")),
            "{errors:?}"
        );

        // A version 1 manifest has no tree, and a tree needs version 2.
        let v1 = GOOD.replace("title = \"Panes\"", "title = \"Panes\"\nroot = \"x\"");
        let errors = Manifest::parse(&v1).expect_err("a tree in version 1");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("a tree needs schema_version = 2")),
            "{errors:?}"
        );
    }

    const EMBEDS: &str = r#"
schema_version = 2
id = "local.embeds"
version = "0.1.0"
title = "Embeds"
capabilities = ["terminal.read"]
root = "main"

[nodes.main]
type = "split"
children = ["left", "right", "logo"]

[nodes.left]
type = "terminal"
pane = 3

[nodes.right]
type = "terminal"
pane = 7

[nodes.logo]
type = "image"
src = "img/logo.png"
"#;

    #[test]
    fn terminal_and_image_nodes_are_checked_and_their_panes_granted() {
        let m = Manifest::parse(EMBEDS).expect("valid");
        let needs = m.needs();
        assert_eq!(needs.panes, [3, 7]);
        assert!(!needs.all_panes);
        assert_eq!(needs.methods, ["terminal.read"]);

        let bad = EMBEDS
            .replace("capabilities = [\"terminal.read\"]", "capabilities = []")
            .replace("pane = 7\n", "")
            .replace("img/logo.png", "../secret.png");
        let errors = Manifest::parse(&bad).expect_err("invalid");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(
            has("nodes.left: a terminal reads its pane; add `terminal.read`"),
            "{errors:?}"
        );
        assert!(
            has("nodes.right.pane: a terminal names the pane it shows"),
            "{errors:?}"
        );
        assert!(
            has("nodes.logo.src: an image's `src` may not leave the app's package"),
            "{errors:?}"
        );
    }

    #[test]
    fn a_web_app_is_its_page_and_its_web_fields_are_checked() {
        let base = "schema_version = 2\nid = \"local.web\"\nversion = \"0.1.0\"\ntitle = \"Web\"\n";
        let m = Manifest::parse(&format!("{base}[web]\n")).expect("a web app");
        assert!(m.is_web_app());
        assert_eq!(m.root.as_deref(), Some(WEB_PAGE));
        assert_eq!(m.nodes[WEB_PAGE].kind, NodeType::Web);

        let bad = format!(
            "{base}[web]\nurl = \"https://example.com\"\nallow = [\"https://a.example/x\", \"ftp://b.example\"]\n"
        );
        let errors = Manifest::parse(&bad).expect_err("invalid");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(
            has("web.url: `https://example.com` is not a local address"),
            "{errors:?}"
        );
        assert!(
            has("web.allow: `https://a.example/x` is not an origin"),
            "{errors:?}"
        );
        assert!(
            has("web.allow: `ftp://b.example` is not an origin"),
            "{errors:?}"
        );

        // In a tree: a page in the package, on the app's origin or its
        // allow-list, never another site.
        let tree = format!(
            "{base}root = \"main\"\n[web]\nallow = [\"https://docs.example\"]\n\
             [nodes.main]\ntype = \"row\"\nchildren = [\"a\", \"b\", \"c\", \"d\"]\n\
             [nodes.a]\ntype = \"web\"\nsrc = \"pages/a.html\"\n\
             [nodes.b]\ntype = \"web\"\nsrc = \"https://docs.example/x\"\n\
             [nodes.c]\ntype = \"web\"\nsrc = \"https://evil.example/\"\n\
             [nodes.d]\ntype = \"web\"\nsrc = \"../secret.html\"\n"
        );
        let errors = Manifest::parse(&tree).expect_err("invalid");
        assert_eq!(errors.len(), 2, "{errors:?}");
        let has = |s: &str| errors.iter().any(|e| e.contains(s));
        assert!(
            has("nodes.c.src: `https://evil.example/` is not on the app's origin"),
            "{errors:?}"
        );
        assert!(
            has("nodes.d.src: `../secret.html` is neither an address nor a page"),
            "{errors:?}"
        );
    }
}
