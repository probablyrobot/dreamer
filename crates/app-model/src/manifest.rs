//! The app manifest (spec 13.3): a versioned TOML file declaring data
//! sources, one view, and keybound commands. It is data, not code: there
//! is no shell, no eval, and no expression language. Commands fill their
//! parameters only from `${selected.<field>}` placeholders, replaced by the
//! selected row's value with its type kept.

use std::collections::BTreeMap;

use serde::Deserialize;

pub const SCHEMA_VERSION: u32 = 1;

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
    pub view: View,
    #[serde(default)]
    pub commands: BTreeMap<String, Command>,
    /// An agentic pane (spec 13.4): the package bound to a provider and a
    /// grant, with `view.type = "agent"`.
    #[serde(default)]
    pub agent: Option<AgentSpec>,
    /// What the package adds to the desktop when installed (spec 12.2.1).
    #[serde(default)]
    pub contributes: Contributes,
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
    /// An API method that returns the data (a query, not a mutation).
    pub method: String,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
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
                    .is_some_and(path_ok);
                if !ok {
                    errors.push(format!(
                        "{at}: `${{{inner}}}` is not `${{selected.<field>}}` or `${{form.<field>}}`"
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
    pub fn needs(&self) -> Needs {
        let mut methods = self.capabilities.clone();
        let follows = self.data.values().any(|s| !s.events.is_empty())
            || self
                .agent
                .as_ref()
                .is_some_and(|a| a.kind == AgentKind::Filter);
        if follows {
            methods.extend(["events.subscribe".into(), "events.unsubscribe".into()]);
        }
        let (panes, all_panes) = match &self.agent {
            Some(a) => (a.tools.panes.clone(), a.tools.all_panes || self.all_panes),
            None => (Vec::new(), self.all_panes),
        };
        Needs {
            methods,
            panes,
            all_panes,
        }
    }

    /// Parse and validate; every problem, each with where it is.
    pub fn parse(text: &str) -> Result<Manifest, Vec<String>> {
        let m: Manifest = toml::from_str(text).map_err(|e| vec![e.to_string()])?;
        let errors = m.validate();
        if errors.is_empty() {
            Ok(m)
        } else {
            Err(errors)
        }
    }

    fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            errors.push(format!(
                "schema_version: {} is not supported (this runtime reads {SCHEMA_VERSION})",
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
            let service = c.strip_suffix(".*");
            let known = match service {
                Some(s) => api::METHODS
                    .iter()
                    .any(|m| m.name.starts_with(&format!("{s}."))),
                None => method_info(c).is_some() || c == api::MANAGE_FILTERS,
            };
            if !known {
                errors.push(format!(
                    "capabilities: `{c}` is not an API method or service"
                ));
            }
        }
        for (name, s) in &self.data {
            let at = format!("data.{name}");
            match method_info(&s.method) {
                None => errors.push(format!("{at}.method: no method `{}`", s.method)),
                Some(m) if m.mutation => errors.push(format!(
                    "{at}.method: `{}` changes things; data comes from queries",
                    s.method
                )),
                Some(_) => {}
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
        self.validate_agent(&mut errors);
        let sourceless = matches!(self.view.kind, ViewKind::Form)
            || (self.view.kind == ViewKind::Agent && self.view.source.is_empty());
        if !sourceless && !self.data.contains_key(&self.view.source) {
            errors.push(format!(
                "view.source: no data source `{}`",
                self.view.source
            ));
        }
        if self.view.kind == ViewKind::Form {
            if self.view.fields.is_empty() {
                errors.push("view.fields: a form needs fields".into());
            }
            match &self.view.submit {
                None => errors.push("view.submit: a form needs a submit command".into()),
                Some(c) if !self.commands.contains_key(c) => {
                    errors.push(format!("view.submit: no command `{c}`"));
                }
                Some(_) => {}
            }
            let mut names = std::collections::BTreeSet::new();
            for f in &self.view.fields {
                if !path_ok(&f.name) || f.name.contains('.') || !names.insert(f.name.clone()) {
                    errors.push(format!(
                        "view.fields: `{}` is not a unique field name",
                        f.name
                    ));
                }
            }
        }
        if let Some(m) = &self.module
            && (m.is_empty() || m.contains("..") || m.starts_with('/'))
        {
            errors.push(format!("module: `{m}` must be a file beside the manifest"));
        }
        if self.view.kind == ViewKind::Table && self.view.columns.is_empty() {
            errors.push("view.columns: a table needs columns".into());
        }
        for c in self
            .view
            .columns
            .iter()
            .chain(self.view.row_key.iter())
            .chain(self.view.field.iter())
        {
            if !path_ok(c) {
                errors.push(format!("view: `{c}` is not a dotted field name"));
            }
        }
        let mut keys = BTreeMap::new();
        for (name, c) in &self.commands {
            let at = format!("commands.{name}");
            if !key_ok(&c.key) {
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
    type Ctx<'a> = (Option<&'a serde_json::Value>, Option<&'a serde_json::Value>);
    fn lookup(inner: &str, (row, form): Ctx) -> Result<serde_json::Value, String> {
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
        (selected, form),
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
}
