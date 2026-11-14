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
    #[serde(default)]
    pub data: BTreeMap<String, Source>,
    pub view: View,
    #[serde(default)]
    pub commands: BTreeMap<String, Command>,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    #[serde(rename = "type")]
    pub kind: ViewKind,
    pub source: String,
    /// The field that identifies a row, so the selection survives updates.
    #[serde(default)]
    pub row_key: Option<String>,
    /// Table: the fields shown, as columns.
    #[serde(default)]
    pub columns: Vec<String>,
    /// List and log: the field shown per row (default the whole row).
    #[serde(default)]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub key: String,
    pub method: String,
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
                if !inner.strip_prefix("selected.").is_some_and(path_ok) {
                    errors.push(format!(
                        "{at}: `${{{inner}}}` is not `${{selected.<field>}}`"
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

impl Manifest {
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
                None => method_info(c).is_some(),
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
        if !self.data.contains_key(&self.view.source) {
            errors.push(format!(
                "view.source: no data source `{}`",
                self.view.source
            ));
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
            if method_info(&c.method).is_none() {
                errors.push(format!("{at}.method: no method `{}`", c.method));
            }
            check_templates(
                &toml::Value::Table(c.params.clone()),
                &format!("{at}.params"),
                &mut errors,
            );
        }
        errors
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
) -> Result<serde_json::Value, String> {
    fn go(
        v: &serde_json::Value,
        row: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        Ok(match v {
            serde_json::Value::String(s) if s.contains("${") => {
                let row = row.ok_or("nothing is selected")?;
                let lookup = |inner: &str| -> Result<serde_json::Value, String> {
                    let path = inner.strip_prefix("selected.").unwrap_or(inner);
                    field(row, path)
                        .cloned()
                        .ok_or_else(|| format!("the selected row has no `{path}`"))
                };
                if let Some(inner) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}'))
                    && !inner.contains("${")
                {
                    return lookup(inner);
                }
                let mut out = String::new();
                let mut rest = s.as_str();
                while let Some(i) = rest.find("${") {
                    out.push_str(&rest[..i]);
                    let after = &rest[i + 2..];
                    let end = after.find('}').ok_or("an unclosed placeholder")?;
                    match lookup(&after[..end])? {
                        serde_json::Value::String(x) => out.push_str(&x),
                        other => out.push_str(&other.to_string()),
                    }
                    rest = &after[end + 1..];
                }
                out.push_str(rest);
                serde_json::Value::String(out)
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(|x| go(x, row)).collect::<Result<_, _>>()?)
            }
            serde_json::Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, x)| go(x, row).map(|x| (k.clone(), x)))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }
    go(&to_json(&toml::Value::Table(params.clone())), selected)
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
        let p = fill(&m.commands["rename"].params, Some(&row)).expect("fill");
        assert_eq!(
            p,
            serde_json::json!({ "action": { "rename_window": { "window": 7, "name": "w-3" } } })
        );
        assert!(fill(&m.commands["rename"].params, None).is_err());
        let missing = serde_json::json!({ "id": 3 });
        assert!(
            fill(&m.commands["rename"].params, Some(&missing))
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
}
