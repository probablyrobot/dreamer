//! The schema file's model and its validation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub version: Version,
    pub package: String,
    pub types: Vec<TypeDef>,
    pub methods: Vec<MethodDef>,
    /// What the daemon pushes, by topic.
    #[serde(default)]
    pub notifications: Vec<NotificationDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationDef {
    pub topic: String,
    pub payload: String,
    pub doc: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeDef {
    pub message: Option<String>,
    #[serde(rename = "enum")]
    pub enumeration: Option<String>,
    pub doc: Option<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    pub oneof: Option<Oneof>,
    #[serde(default)]
    pub values: Vec<EnumValue>,
    #[serde(default)]
    pub reserved: Vec<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub n: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub repeated: bool,
    #[serde(default)]
    pub optional: bool,
    pub doc: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oneof {
    pub name: String,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub n: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub doc: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumValue {
    pub n: i32,
    pub name: String,
    pub doc: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodDef {
    pub method: String,
    pub params: String,
    pub result: String,
    #[serde(default)]
    pub mutation: bool,
    pub doc: Option<String>,
}

pub const SCALARS: [&str; 7] = ["string", "bool", "u32", "u64", "i32", "i64", "bytes"];

/// Names generated code uses as identifiers in Rust or Luau.
const RESERVED_WORDS: [&str; 24] = [
    "type", "match", "fn", "mod", "use", "impl", "self", "super", "crate", "move", "ref", "loop",
    "where", "async", "await", "dyn", "end", "local", "function", "then", "until", "nil", "not",
    "repeat",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Scalar,
    Enum,
    Message,
}

impl Schema {
    pub fn kind(&self, ty: &str) -> Option<Kind> {
        if SCALARS.contains(&ty) {
            return Some(Kind::Scalar);
        }
        self.types.iter().find_map(|t| {
            if t.message.as_deref() == Some(ty) {
                Some(Kind::Message)
            } else if t.enumeration.as_deref() == Some(ty) {
                Some(Kind::Enum)
            } else {
                None
            }
        })
    }

    pub fn messages(&self) -> impl Iterator<Item = (&str, &TypeDef)> {
        self.types
            .iter()
            .filter_map(|t| t.message.as_deref().map(|n| (n, t)))
    }

    pub fn enums(&self) -> impl Iterator<Item = (&str, &TypeDef)> {
        self.types
            .iter()
            .filter_map(|t| t.enumeration.as_deref().map(|n| (n, t)))
    }

    /// Every problem, so one run reports them all.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.package.split('.').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }) {
            errors.push(format!(
                "package `{}` must be dotted lowercase",
                self.package
            ));
        }
        let mut names = BTreeSet::new();
        for t in &self.types {
            let name = match (&t.message, &t.enumeration) {
                (Some(n), None) | (None, Some(n)) => n,
                _ => {
                    errors.push("each type is exactly one of `message` or `enum`".into());
                    continue;
                }
            };
            if !is_camel(name) {
                errors.push(format!("type `{name}` must be CamelCase"));
            }
            if SCALARS.contains(&name.as_str()) || !names.insert(name.clone()) {
                errors.push(format!(
                    "type `{name}` is defined twice or shadows a scalar"
                ));
            }
        }
        for (name, t) in self.messages() {
            if !t.values.is_empty() {
                errors.push(format!("message {name}: `values` belongs to enums"));
            }
            let mut numbers = BTreeMap::new();
            let mut field_names = BTreeSet::new();
            let mut claim = |n: u32, what: &str, errors: &mut Vec<String>| {
                if n == 0 || n > 536_870_911 || (19_000..=19_999).contains(&n) {
                    errors.push(format!("{name}.{what}: field number {n} is not usable"));
                }
                if t.reserved.contains(&n) {
                    errors.push(format!("{name}.{what}: field number {n} is reserved"));
                }
                if let Some(prev) = numbers.insert(n, what.to_owned()) {
                    errors.push(format!(
                        "{name}: field number {n} used by {prev} and {what}"
                    ));
                }
            };
            for f in &t.fields {
                claim(f.n, &f.name, &mut errors);
                check_ident(name, &f.name, &mut field_names, &mut errors);
                match self.kind(&f.ty) {
                    None => errors.push(format!("{name}.{}: unknown type `{}`", f.name, f.ty)),
                    Some(Kind::Message) if f.optional => errors.push(format!(
                        "{name}.{}: message fields are always optional; drop `optional`",
                        f.name
                    )),
                    Some(Kind::Enum) if f.optional || f.repeated => errors.push(format!(
                        "{name}.{}: enum fields cannot be optional or repeated",
                        f.name
                    )),
                    _ => {}
                }
                if f.optional && f.repeated {
                    errors.push(format!("{name}.{}: optional and repeated together", f.name));
                }
            }
            if let Some(o) = &t.oneof {
                check_ident(name, &o.name, &mut field_names, &mut errors);
                if o.variants.is_empty() {
                    errors.push(format!("{name}.{}: a oneof needs variants", o.name));
                }
                for v in &o.variants {
                    claim(v.n, &v.name, &mut errors);
                    check_ident(name, &v.name, &mut field_names, &mut errors);
                    if self.kind(&v.ty) != Some(Kind::Message) {
                        errors.push(format!(
                            "{name}.{}: oneof variants must be messages, not `{}`",
                            v.name, v.ty
                        ));
                    }
                }
            }
        }
        for (name, t) in self.enums() {
            if !t.fields.is_empty() || t.oneof.is_some() {
                errors.push(format!("enum {name}: has message parts"));
            }
            if !t.values.iter().any(|v| v.n == 0) {
                errors.push(format!(
                    "enum {name}: needs a value numbered 0 (the default)"
                ));
            }
            let mut seen_n = BTreeSet::new();
            let mut seen_name = BTreeSet::new();
            for v in &t.values {
                if !seen_n.insert(v.n) || !seen_name.insert(v.name.clone()) {
                    errors.push(format!("enum {name}: duplicate value {} {}", v.n, v.name));
                }
                if !is_snake(&v.name) {
                    errors.push(format!(
                        "enum {name}: value `{}` must be snake_case",
                        v.name
                    ));
                }
            }
        }
        let mut methods = BTreeSet::new();
        for m in &self.methods {
            let parts: Vec<&str> = m.method.split('.').collect();
            if !(2..=3).contains(&parts.len()) || !parts.iter().all(|p| is_snake(p)) {
                errors.push(format!(
                    "method `{}` must be `service.name` (or `service.group.name`) in snake_case",
                    m.method
                ));
            }
            if !methods.insert(m.method.clone()) {
                errors.push(format!("method `{}` defined twice", m.method));
            }
            for ty in [&m.params, &m.result] {
                if self.kind(ty) != Some(Kind::Message) {
                    errors.push(format!("method `{}`: `{ty}` is not a message", m.method));
                }
            }
            // A method is never also the group of other methods.
            let group = format!("{}.", m.method);
            if self.methods.iter().any(|o| o.method.starts_with(&group)) {
                errors.push(format!(
                    "method `{}` is also a group of other methods",
                    m.method
                ));
            }
        }
        let mut topics = BTreeSet::new();
        for n in &self.notifications {
            let parts: Vec<&str> = n.topic.split('.').collect();
            if !(2..=3).contains(&parts.len()) || !parts.iter().all(|p| is_snake(p)) {
                errors.push(format!(
                    "notification `{}` must be `service.name` (or `service.group.name`) in snake_case",
                    n.topic
                ));
            }
            if !topics.insert(n.topic.clone()) {
                errors.push(format!("notification `{}` defined twice", n.topic));
            }
            if self.kind(&n.payload) != Some(Kind::Message) {
                errors.push(format!(
                    "notification `{}`: `{}` is not a message",
                    n.topic, n.payload
                ));
            }
        }
        errors
    }
}

fn check_ident(message: &str, name: &str, seen: &mut BTreeSet<String>, errors: &mut Vec<String>) {
    if !is_snake(name) {
        errors.push(format!("{message}.{name}: names must be snake_case"));
    }
    if RESERVED_WORDS.contains(&name) {
        errors.push(format!(
            "{message}.{name}: `{name}` is a Rust or Luau keyword"
        ));
    }
    if !seen.insert(name.to_owned()) {
        errors.push(format!("{message}.{name}: name used twice"));
    }
}

fn is_snake(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_camel(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `outcome_unknown` -> `OutcomeUnknown`.
pub fn camel(snake: &str) -> String {
    snake
        .split(['_', '.'])
        .map(|p| {
            let mut c = p.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

/// `SplitPane` -> `split_pane`.
pub fn snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
