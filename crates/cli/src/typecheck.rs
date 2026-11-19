//! Type-level mistakes in an app.ts, found without a TypeScript compiler:
//! the code is parsed with swc and every use of `dreamer`, `ui`, and `app`
//! is held to what `dreamer.d.ts` declares, read as data from
//! [`api::APP_TYPES`] (generated from the API schema and `ui.yaml`).
//!
//! What it finds: a `dreamer.*` name that is not an API method (or a
//! `dreamer.call` of one), a `ui.*` builder that does not exist, a
//! property a method's parameters, a builder, or `app()` does not have,
//! and a value whose kind the code makes plain (a string, number, boolean,
//! array, object, or function written in place) that is not what the
//! declaration takes, at any depth of object and array literals. A value
//! held in a variable, returned by a call, or cast with `as` is not
//! followed: that is a type checker's work, and this is not one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use serde::Deserialize;
use swc_common::sync::Lrc;
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, Span, Spanned, SyntaxContext};
use swc_ecma_ast::{
    ArrayLit, CallExpr, Callee, EsVersion, Expr, ExprOrSpread, ImportSpecifier, Lit, MemberExpr,
    MemberProp, ModuleDecl, ModuleExportName, ModuleItem, ObjectLit, Prop, PropName, PropOrSpread,
    UnaryOp,
};
use swc_ecma_parser::{EsSyntax, Syntax, TsSyntax, parse_file_as_module};
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

/// One mistake, where it is (1-based line and column).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Problem {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.file, self.line, self.column, self.message
        )
    }
}

// ---- The declarations, as data -------------------------------------------

#[derive(Deserialize)]
struct Types {
    methods: BTreeMap<String, String>,
    messages: HashMap<String, Message>,
    enums: HashMap<String, Vec<String>>,
    ui: Ui,
}

#[derive(Deserialize)]
struct Message {
    fields: BTreeMap<String, FieldType>,
    oneof: Option<Oneof>,
}

#[derive(Deserialize)]
struct FieldType {
    #[serde(rename = "type")]
    ty: String,
    repeated: bool,
}

#[derive(Deserialize)]
struct Oneof {
    name: String,
    variants: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Ui {
    types: BTreeMap<String, UiType>,
    common: BTreeMap<String, String>,
    components: Vec<Component>,
    screen: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct UiType {
    values: Option<Vec<String>>,
    fields: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct Component {
    name: String,
    form: String,
    first: Option<String>,
    props: BTreeMap<String, String>,
}

struct Known {
    types: Types,
    /// Every proper prefix of a method name: `agent`, `agent.sessions`.
    groups: BTreeSet<String>,
    /// Each builder's properties, the common ones included.
    props: HashMap<String, BTreeMap<String, String>>,
    /// What `app()` takes.
    app: BTreeMap<String, String>,
}

fn known() -> &'static Known {
    static K: OnceLock<Known> = OnceLock::new();
    K.get_or_init(|| {
        let types: Types =
            serde_json::from_str(api::APP_TYPES).expect("app-types.json is generated valid");
        let mut groups = BTreeSet::new();
        for m in types.methods.keys() {
            for (end, _) in m.match_indices('.') {
                groups.insert(m[..end].to_owned());
            }
        }
        let props = types
            .ui
            .components
            .iter()
            .map(|c| {
                let mut p = types.ui.common.clone();
                p.extend(c.props.clone());
                (c.name.clone(), p)
            })
            .collect();
        let app = [
            ("state", "unknown"),
            ("load", "() => unknown"),
            ("refresh", "number"),
            ("view", "() => unknown"),
            ("onKey", "() => unknown"),
            ("onEvent", "() => unknown"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        Known {
            types,
            groups,
            props,
            app,
        }
    })
}

/// What a place takes.
#[derive(Clone, Copy)]
enum Shape<'a> {
    Any,
    Str,
    Bool,
    Num {
        integer: bool,
        unsigned: bool,
    },
    Function,
    Enum(&'a [String]),
    /// A schema field: its type, a list of it when repeated.
    Field(&'a str, bool),
    /// A `ui.yaml` type: `A | B`, `T[]`, or a name.
    Ui(&'a str),
    Message(&'a str),
    Oneof(&'a Oneof),
    Fields(&'a BTreeMap<String, String>, &'a str),
}

impl<'a> Shape<'a> {
    /// The field's type when it is not a list.
    fn api(k: &'a Known, ty: &'a str) -> Shape<'a> {
        match ty {
            "string" | "bytes" => Shape::Str,
            "bool" => Shape::Bool,
            "u32" | "u64" => Shape::Num {
                integer: true,
                unsigned: true,
            },
            "i32" | "i64" => Shape::Num {
                integer: true,
                unsigned: false,
            },
            t => match k.types.enums.get(t) {
                Some(values) => Shape::Enum(values),
                None if k.types.messages.contains_key(t) => Shape::Message(t),
                None => Shape::Any,
            },
        }
    }

    /// A `ui.yaml` name, or `unknown`.
    fn ui_name(k: &'a Known, name: &'a str) -> Shape<'a> {
        match name {
            "string" => Shape::Str,
            "number" => Shape::Num {
                integer: false,
                unsigned: false,
            },
            "boolean" => Shape::Bool,
            n => match k.types.ui.types.get(n) {
                Some(UiType {
                    values: Some(v), ..
                }) => Shape::Enum(v),
                Some(UiType {
                    fields: Some(f), ..
                }) => Shape::Fields(f, n),
                _ => Shape::Any,
            },
        }
    }

    fn describe(self) -> String {
        match self {
            Shape::Any => "anything".into(),
            Shape::Str => "a string".into(),
            Shape::Bool => "a boolean".into(),
            Shape::Num { integer: true, .. } => "a whole number".into(),
            Shape::Num { .. } => "a number".into(),
            Shape::Function => "a function".into(),
            Shape::Enum(v) => {
                let v: Vec<String> = v.iter().map(|v| format!("\"{v}\"")).collect();
                format!("one of {}", v.join(", "))
            }
            Shape::Field(_, true) => "an array".into(),
            Shape::Field(ty, false) => Shape::api(known(), ty).describe(),
            Shape::Ui(ty) if ty.starts_with('(') => "a function".into(),
            Shape::Ui(ty) => ty
                .split('|')
                .map(|p| {
                    let p = p.trim();
                    if p.ends_with("[]") {
                        "an array".to_owned()
                    } else {
                        Shape::ui_name(known(), p).describe()
                    }
                })
                .collect::<Vec<_>>()
                .join(" or "),
            Shape::Message(m) => format!("an object ({m})"),
            Shape::Oneof(_) => "an object with one of its variants".into(),
            Shape::Fields(_, n) => format!("an object ({n})"),
        }
    }
}

/// What the code makes plain about a value.
enum Seen<'e> {
    Str(Option<String>),
    Num(f64),
    Bool,
    Nothing,
    Array(&'e ArrayLit),
    Object(&'e ObjectLit),
    Function,
    Unknown,
}

impl Seen<'_> {
    fn of(e: &Expr) -> Seen<'_> {
        match e {
            Expr::Paren(p) => Seen::of(&p.expr),
            Expr::TsConstAssertion(c) => Seen::of(&c.expr),
            Expr::TsSatisfies(s) => Seen::of(&s.expr),
            Expr::Lit(Lit::Str(s)) => Seen::Str(s.value.as_atom().map(|a| a.as_str().to_owned())),
            Expr::Lit(Lit::Num(n)) => Seen::Num(n.value),
            Expr::Lit(Lit::Bool(_)) => Seen::Bool,
            Expr::Lit(Lit::Null(_)) => Seen::Nothing,
            Expr::Ident(i) if &*i.sym == "undefined" => Seen::Nothing,
            Expr::Tpl(_) => Seen::Str(None),
            Expr::Unary(u) if u.op == UnaryOp::Minus => match Seen::of(&u.arg) {
                Seen::Num(v) => Seen::Num(-v),
                _ => Seen::Unknown,
            },
            Expr::Array(a) => Seen::Array(a),
            Expr::Object(o) => Seen::Object(o),
            Expr::Arrow(_) | Expr::Fn(_) => Seen::Function,
            _ => Seen::Unknown,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Seen::Str(_) => "a string",
            Seen::Num(_) => "a number",
            Seen::Bool => "a boolean",
            Seen::Array(_) => "an array",
            Seen::Object(_) => "an object",
            Seen::Function => "a function",
            Seen::Nothing | Seen::Unknown => "a value",
        }
    }
}

/// The closest of `names` to `wrong`, when it is close.
fn closest<'n>(wrong: &str, names: impl Iterator<Item = &'n str>) -> Option<&'n str> {
    let limit = (wrong.chars().count() / 3).max(1);
    names
        .map(|n| (distance(wrong, n), n))
        .filter(|(d, _)| *d <= limit)
        .min()
        .map(|(_, n)| n)
}

/// Edit distance, a swap of two neighbours counting one.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (
        a.to_lowercase().chars().collect(),
        b.to_lowercase().chars().collect(),
    );
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

fn hint<'n>(wrong: &str, names: impl Iterator<Item = &'n str> + Clone) -> String {
    match closest(wrong, names.clone()) {
        Some(n) => format!("; did you mean `{n}`?"),
        None => {
            let all: Vec<&str> = names.collect();
            if all.is_empty() {
                String::new()
            } else if all.len() <= 12 {
                format!(" (it has {})", all.join(", "))
            } else {
                String::new()
            }
        }
    }
}

// ---- The walk ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    App,
    Ui,
    Dreamer,
}

struct Checker<'k> {
    k: &'k Known,
    cm: Lrc<SourceMap>,
    file: String,
    unresolved: Mark,
    /// Imports from "dreamer", by local binding.
    imports: HashMap<(String, SyntaxContext), Role>,
    problems: Vec<Problem>,
}

impl Checker<'_> {
    fn report(&mut self, span: Span, message: String) {
        let at = self.cm.lookup_char_pos(span.lo);
        self.problems.push(Problem {
            file: self.file.clone(),
            line: at.line,
            column: at.col.0 + 1,
            message,
        });
    }

    fn role(&self, e: &Expr) -> Option<Role> {
        let Expr::Ident(i) = e else { return None };
        if let Some(r) = self.imports.get(&(i.sym.to_string(), i.ctxt)) {
            return Some(*r);
        }
        if i.ctxt.outer() != self.unresolved {
            return None;
        }
        match &*i.sym {
            "app" => Some(Role::App),
            "ui" => Some(Role::Ui),
            "dreamer" => Some(Role::Dreamer),
            _ => None,
        }
    }

    /// `dreamer.a.b`: the global and the names after it, as far as they
    /// are written out.
    fn chain(&self, m: &MemberExpr) -> Option<(Role, Vec<(String, Span)>)> {
        let mut names = Vec::new();
        let mut cur = m;
        loop {
            match &cur.prop {
                MemberProp::Ident(i) => names.push(Some((i.sym.to_string(), i.span))),
                MemberProp::Computed(c) => match &*c.expr {
                    Expr::Lit(Lit::Str(s)) => {
                        names.push(s.value.as_atom().map(|a| (a.as_str().to_owned(), s.span)))
                    }
                    _ => names.push(None),
                },
                MemberProp::PrivateName(_) => return None,
            }
            let mut obj: &Expr = &cur.obj;
            while let Expr::Paren(p) = obj {
                obj = &p.expr;
            }
            match obj {
                Expr::Member(inner) => cur = inner,
                other => {
                    let role = self.role(other)?;
                    names.reverse();
                    // Stop at the first name that is not written out.
                    let names = names.into_iter().map_while(|n| n).collect();
                    return Some((role, names));
                }
            }
        }
    }

    /// Report a name after `dreamer.` or `ui.` that does not exist; the
    /// method or builder named, if one is.
    fn resolve(&mut self, role: Role, names: &[(String, Span)]) -> Option<String> {
        match role {
            Role::App => None,
            Role::Ui => {
                let (name, span) = names.first()?;
                if name == "screen" || self.k.props.contains_key(name) {
                    return Some(name.clone());
                }
                let builders = self
                    .k
                    .types
                    .ui
                    .components
                    .iter()
                    .map(|c| c.name.as_str())
                    .chain(["screen"]);
                let h = hint(name, builders);
                self.report(*span, format!("`ui.{name}` is not a ui builder{h}"));
                None
            }
            Role::Dreamer => {
                let mut path = String::new();
                for (i, (name, span)) in names.iter().enumerate() {
                    if i == 0 && (name == "call" || name == "DreamerError") {
                        return None;
                    }
                    let parent = path.clone();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(name);
                    if self.k.types.methods.contains_key(&path) {
                        return Some(path);
                    }
                    if self.k.groups.contains(&path) {
                        continue;
                    }
                    let prefix = if parent.is_empty() {
                        String::new()
                    } else {
                        format!("{parent}.")
                    };
                    let mut siblings: BTreeSet<&str> = self
                        .k
                        .types
                        .methods
                        .keys()
                        .chain(self.k.groups.iter())
                        .filter_map(|m| m.strip_prefix(&prefix))
                        .filter(|rest| !rest.contains('.'))
                        .collect();
                    if parent.is_empty() {
                        siblings.extend(["call", "DreamerError"]);
                    }
                    let h = hint(name, siblings.iter().copied());
                    let what = if self.k.groups.contains(&parent) || parent.is_empty() {
                        "is not an API method"
                    } else {
                        "is not an API method or group"
                    };
                    self.report(*span, format!("`dreamer.{path}` {what}{h}"));
                    return None;
                }
                None
            }
        }
    }

    /// Hold `e` to `shape`; `what` names the place in messages.
    fn value(&mut self, e: &Expr, shape: Shape<'_>, what: &str) {
        self.seen(Seen::of(e), e.span(), shape, what);
    }

    fn seen(&mut self, seen: Seen<'_>, span: Span, shape: Shape<'_>, what: &str) {
        if matches!(seen, Seen::Unknown | Seen::Nothing) {
            return;
        }
        let k = self.k;
        match shape {
            Shape::Any => {}
            Shape::Field(ty, true) => match seen {
                Seen::Array(a) => self.elements(a, Shape::Field(ty, false), what),
                _ => self.mismatch(span, &seen, "an array", what),
            },
            Shape::Field(ty, false) => self.seen(seen, span, Shape::api(k, ty), what),
            Shape::Ui(ty) => {
                if ty.starts_with('(') {
                    return self.seen(seen, span, Shape::Function, what);
                }
                let parts: Vec<&str> = ty.split('|').map(str::trim).collect();
                if parts.len() > 1 {
                    // A union: fine when its kind fits one part.
                    let fits = parts.iter().any(|p| match (&seen, p.strip_suffix("[]")) {
                        (Seen::Array(_), Some(_)) => true,
                        (_, Some(_)) => false,
                        (_, None) => fits(&seen, Shape::ui_name(k, p)),
                    });
                    if !fits {
                        self.mismatch(span, &seen, &shape.describe(), what);
                    }
                    return;
                }
                match ty.strip_suffix("[]") {
                    Some(inner) => match seen {
                        Seen::Array(a) => self.elements(a, Shape::Ui(inner), what),
                        _ => self.mismatch(span, &seen, "an array", what),
                    },
                    None => self.seen(seen, span, Shape::ui_name(k, ty), what),
                }
            }
            Shape::Message(m) => match seen {
                Seen::Object(o) => self.object(o, Shape::Message(m), what),
                _ => self.mismatch(span, &seen, &shape.describe(), what),
            },
            Shape::Oneof(_) | Shape::Fields(..) => match seen {
                Seen::Object(o) => self.object(o, shape, what),
                _ => self.mismatch(span, &seen, &shape.describe(), what),
            },
            Shape::Enum(values) => match &seen {
                Seen::Str(Some(v)) if !values.contains(v) => {
                    let h = hint(v, values.iter().map(String::as_str));
                    let all: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
                    let msg = if h.starts_with(';') {
                        format!("{what} is not \"{v}\"{h}")
                    } else {
                        format!("{what} is one of {}, not \"{v}\"", all.join(", "))
                    };
                    self.report(span, msg);
                }
                Seen::Str(_) | Seen::Num(_) => {}
                _ => self.mismatch(span, &seen, &shape.describe(), what),
            },
            Shape::Num { integer, unsigned } => match seen {
                Seen::Num(v) if integer && v.fract() != 0.0 => {
                    self.report(span, format!("{what} takes a whole number, not {v}"));
                }
                Seen::Num(v) if unsigned && v < 0.0 => {
                    self.report(span, format!("{what} takes a number 0 or more, not {v}"));
                }
                Seen::Num(_) => {}
                _ => self.mismatch(span, &seen, &shape.describe(), what),
            },
            Shape::Str | Shape::Bool | Shape::Function => {
                if !fits(&seen, shape) {
                    self.mismatch(span, &seen, &shape.describe(), what);
                }
            }
        }
    }

    fn elements(&mut self, a: &ArrayLit, shape: Shape<'_>, what: &str) {
        for el in a.elems.iter().flatten() {
            if el.spread.is_none() {
                self.value(&el.expr, shape, &format!("an item of {what}"));
            }
        }
    }

    fn mismatch(&mut self, span: Span, seen: &Seen<'_>, want: &str, what: &str) {
        let found = seen.describe();
        self.report(span, format!("{what} takes {want}, not {found}"));
    }

    /// Hold an object literal's properties to a record.
    fn object(&mut self, o: &ObjectLit, record: Shape<'_>, what: &str) {
        let k = self.k;
        for p in &o.props {
            let PropOrSpread::Prop(p) = p else { continue };
            // The key, where it is, and what its value is seen to be.
            let (key, span, seen) = match &**p {
                Prop::Shorthand(i) => (Some(i.sym.to_string()), i.span, None),
                Prop::KeyValue(kv) => (
                    prop_name(&kv.key),
                    kv.key.span(),
                    Some((Seen::of(&kv.value), kv.value.span())),
                ),
                Prop::Method(m) => (
                    prop_name(&m.key),
                    m.key.span(),
                    Some((Seen::Function, m.function.span)),
                ),
                Prop::Getter(g) => (prop_name(&g.key), g.key.span(), None),
                _ => continue,
            };
            let Some(key) = key else { continue };
            let (shape, place) = match record {
                Shape::Message(m) => {
                    let Some(msg) = k.types.messages.get(m) else {
                        return;
                    };
                    if let Some(f) = msg.fields.get(&key) {
                        (
                            Shape::Field(&f.ty, f.repeated),
                            format!("`{key}` ({m}.{key})"),
                        )
                    } else if let Some(of) = msg.oneof.as_ref().filter(|of| of.name == key) {
                        (Shape::Oneof(of), format!("`{key}` ({m}.{key})"))
                    } else {
                        let names = msg
                            .fields
                            .keys()
                            .map(String::as_str)
                            .chain(msg.oneof.as_ref().map(|o| o.name.as_str()));
                        let h = hint(&key, names);
                        self.report(span, format!("`{key}` is not a field of {what}{h}"));
                        continue;
                    }
                }
                Shape::Oneof(of) => match of.variants.get(&key) {
                    Some(ty) => (Shape::Message(ty), format!("`{key}` ({ty})")),
                    None => {
                        let h = hint(&key, of.variants.keys().map(String::as_str));
                        self.report(
                            span,
                            format!("`{key}` is not one of the variants of {what}{h}"),
                        );
                        continue;
                    }
                },
                Shape::Fields(fields, name) => match fields.get(&key) {
                    Some(ty) => (Shape::Ui(ty), format!("`{key}` of {name}")),
                    None => {
                        let h = hint(&key, fields.keys().map(String::as_str));
                        self.report(span, format!("`{key}` is not a property of {name}{h}"));
                        continue;
                    }
                },
                _ => return,
            };
            if let Some((seen, at)) = seen {
                self.seen(seen, at, shape, &place);
            }
        }
    }

    fn object_arg(&mut self, arg: Option<&ExprOrSpread>, record: Shape<'_>, what: &str) {
        if let Some(a) = arg.filter(|a| a.spread.is_none()) {
            self.value(&a.expr, record, what);
        }
    }

    fn call(&mut self, c: &CallExpr, role: Role, names: &[(String, Span)], target: Option<&str>) {
        let k = self.k;
        let args = &c.args;
        match role {
            Role::App if names.is_empty() => {
                self.object_arg(args.first(), Shape::Fields(&k.app, "app()"), "app()");
            }
            Role::Dreamer => {
                if let Some(method) = target {
                    let Some(params) = k.types.methods.get(method) else {
                        return;
                    };
                    let what = format!("the parameters of dreamer.{method} ({params})");
                    self.object_arg(args.first(), Shape::Message(params), &what);
                } else if names.len() == 1 && names[0].0 == "call" {
                    let Some(first) = args.first().filter(|a| a.spread.is_none()) else {
                        return;
                    };
                    if let Expr::Lit(Lit::Str(s)) = &*first.expr {
                        let Some(m) = s.value.as_atom().map(|a| a.as_str()) else {
                            return;
                        };
                        match k.types.methods.get(m) {
                            Some(params) => {
                                let what = format!("the parameters of {m} ({params})");
                                self.object_arg(args.get(1), Shape::Message(params), &what);
                            }
                            None => {
                                let h = hint(m, k.types.methods.keys().map(String::as_str));
                                self.report(s.span, format!("\"{m}\" is not an API method{h}"));
                            }
                        }
                    }
                }
            }
            Role::Ui => {
                let Some(name) = target.filter(|_| names.len() == 1) else {
                    return;
                };
                if name == "screen" {
                    let what = "ui.screen's options".to_owned();
                    self.object_arg(
                        args.get(1),
                        Shape::Fields(&k.types.ui.screen, "ui.screen"),
                        &what,
                    );
                    return;
                }
                let Some(c) = k.types.ui.components.iter().find(|c| c.name == name) else {
                    return;
                };
                let Some(props) = k.props.get(name) else {
                    return;
                };
                let what = format!("ui.{name}");
                let record = Shape::Fields(props, &what);
                let first = args.first().filter(|a| a.spread.is_none());
                match (c.form.as_str(), first.map(|a| Seen::of(&a.expr))) {
                    (_, Some(Seen::Object(_))) | ("props", _) => {
                        self.object_arg(first, record, &what);
                    }
                    ("layout", _) => {
                        if let Some(a) = first {
                            let place = format!("the children of ui.{name}");
                            self.value(&a.expr, Shape::Ui("Child[]"), &place);
                        }
                        self.object_arg(args.get(1), record, &what);
                    }
                    ("content", _) => {
                        if let (Some(a), Some(f)) = (first, c.first.as_deref())
                            && let Some(ty) = props.get(f)
                        {
                            let place = format!("`{f}` of ui.{name}");
                            self.value(&a.expr, Shape::Ui(ty), &place);
                        }
                        self.object_arg(args.get(1), record, &what);
                    }
                    _ => {}
                }
            }
            Role::App => {}
        }
    }
}

/// Whether what is seen can be `shape` (for the simple shapes).
fn fits(seen: &Seen<'_>, shape: Shape<'_>) -> bool {
    matches!(
        (seen, shape),
        (Seen::Unknown | Seen::Nothing, _)
            | (_, Shape::Any)
            | (Seen::Str(_), Shape::Str | Shape::Enum(_))
            | (Seen::Num(_), Shape::Num { .. } | Shape::Enum(_))
            | (Seen::Bool, Shape::Bool)
            | (Seen::Function, Shape::Function)
            | (
                Seen::Object(_),
                Shape::Message(_) | Shape::Fields(..) | Shape::Oneof(_)
            )
    )
}

fn prop_name(p: &PropName) -> Option<String> {
    match p {
        PropName::Ident(i) => Some(i.sym.to_string()),
        PropName::Str(s) => s.value.as_atom().map(|a| a.as_str().to_owned()),
        _ => None,
    }
}

impl Visit for Checker<'_> {
    fn visit_call_expr(&mut self, c: &CallExpr) {
        if let Callee::Expr(callee) = &c.callee {
            let mut e: &Expr = callee;
            while let Expr::Paren(p) = e {
                e = &p.expr;
            }
            match e {
                Expr::Member(m) => {
                    if let Some((role, names)) = self.chain(m) {
                        // Resolving here reports a wrong name once; the
                        // member visit below does not report it again.
                        let target = self.quiet_resolve(role, &names);
                        self.call(c, role, &names, target.as_deref());
                    }
                }
                other => {
                    if let Some(role) = self.role(other) {
                        self.call(c, role, &[], None);
                    }
                }
            }
        }
        c.visit_children_with(self);
    }

    fn visit_member_expr(&mut self, m: &MemberExpr) {
        match self.chain(m) {
            Some((role, names)) => {
                self.resolve(role, &names);
                // Only computed names inside the chain hold more code.
                let mut e: &MemberExpr = m;
                loop {
                    if let MemberProp::Computed(c) = &e.prop {
                        c.visit_with(self);
                    }
                    match &*e.obj {
                        Expr::Member(inner) => e = inner,
                        _ => break,
                    }
                }
            }
            None => m.visit_children_with(self),
        }
    }
}

impl Checker<'_> {
    /// [`Checker::resolve`] without reporting.
    fn quiet_resolve(&mut self, role: Role, names: &[(String, Span)]) -> Option<String> {
        let before = self.problems.len();
        let r = self.resolve(role, names);
        self.problems.truncate(before);
        r
    }
}

/// The mistakes in `source` (named `name`; TypeScript unless it ends in
/// `.js`). Code that does not parse has none: stripping reports its
/// syntax errors.
pub fn check(name: &str, source: &str) -> Vec<Problem> {
    GLOBALS.set(&Globals::new(), || {
        let cm: Lrc<SourceMap> = Lrc::default();
        let fm = cm.new_source_file(
            Lrc::new(FileName::Custom(name.to_owned())),
            source.to_owned(),
        );
        let syntax = if name.ends_with(".js") {
            Syntax::Es(EsSyntax::default())
        } else {
            Syntax::Typescript(TsSyntax::default())
        };
        let mut recovered = Vec::new();
        let Ok(mut module) =
            parse_file_as_module(&fm, syntax, EsVersion::latest(), None, &mut recovered)
        else {
            return Vec::new();
        };
        let unresolved = Mark::new();
        let top = Mark::new();
        module.visit_mut_with(&mut swc_ecma_transforms_base::resolver(
            unresolved, top, true,
        ));
        let mut imports = HashMap::new();
        for item in &module.body {
            let ModuleItem::ModuleDecl(ModuleDecl::Import(i)) = item else {
                continue;
            };
            if i.src.value.as_atom().map(|a| a.as_str()) != Some("dreamer") {
                continue;
            }
            for s in &i.specifiers {
                let ImportSpecifier::Named(n) = s else {
                    continue;
                };
                let imported = match &n.imported {
                    Some(ModuleExportName::Ident(id)) => id.sym.to_string(),
                    Some(ModuleExportName::Str(s)) => s
                        .value
                        .as_atom()
                        .map(|a| a.as_str().to_owned())
                        .unwrap_or_default(),
                    None => n.local.sym.to_string(),
                };
                let role = match imported.as_str() {
                    "app" => Role::App,
                    "ui" => Role::Ui,
                    "dreamer" => Role::Dreamer,
                    _ => continue,
                };
                imports.insert((n.local.sym.to_string(), n.local.ctxt), role);
            }
        }
        let mut c = Checker {
            k: known(),
            cm,
            file: name.to_owned(),
            unresolved,
            imports,
            problems: Vec::new(),
        };
        module.visit_with(&mut c);
        c.problems
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(src: &str) -> Vec<String> {
        check("app.ts", src)
            .iter()
            .map(Problem::to_string)
            .collect()
    }

    #[test]
    fn a_misspelled_method_is_named_with_the_one_meant() {
        let p = found("const t = await dreamer.tmux.topolgy();\n");
        assert_eq!(
            p,
            ["app.ts:1:30: `dreamer.tmux.topolgy` is not an API method; did you mean `topology`?"]
        );
        let p = found("dreamer.tmx.topology();\ndreamer.agent.sessions.create({});\n");
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("app.ts:1:9: `dreamer.tmx`"), "{p:?}");
        assert!(p[0].contains("did you mean `tmux`"), "{p:?}");
        let p =
            found("dreamer.call(\"tmux.topolgy\", {});\ndreamer.call(\"tmux.topology\", {});\n");
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("app.ts:1:14:"), "{p:?}");
    }

    #[test]
    fn parameters_are_held_to_the_methods_record() {
        let p = found(
            "dreamer.tmux.topology({ endpont: \"a\" });\n\
             dreamer.terminal.read({ pane: \"3\", history: -1 });\n\
             dreamer.actions.submit({ action: { kind: { select_pane: { pane: true } } } });\n\
             dreamer.actions.submit({ action: { kind: { select_pain: {} } } });\n",
        );
        assert_eq!(p.len(), 5, "{p:?}");
        assert!(
            p[0].starts_with("app.ts:1:25: `endpont` is not a field of"),
            "{p:?}"
        );
        assert!(p[0].contains("did you mean `endpoint`"), "{p:?}");
        assert!(
            p[1].starts_with(
                "app.ts:2:31: `pane` (ReadParams.pane) takes a whole number, not a string"
            ),
            "{p:?}"
        );
        assert!(p[2].starts_with("app.ts:2:45: `history`"), "{p:?}");
        assert!(
            p[3].contains("`pane` (SelectPane.pane) takes a whole number, not a boolean"),
            "{p:?}"
        );
        assert!(
            p[4].contains("`select_pain` is not one of the variants"),
            "{p:?}"
        );
    }

    #[test]
    fn ui_builders_and_their_properties_are_checked() {
        let p = found(
            "import { ui as u } from \"dreamer\";\n\
             u.colum([]);\n\
             u.text({ txt: \"a\" });\n\
             u.badge(\"hi\", { tone: \"loud\" });\n\
             u.gauge(\"full\");\n\
             u.button({ label: \"Go\", onPress: 3 });\n\
             u.split([u.text(\"a\")], { sizes: [1, \"2\"], direction: \"row\" });\n\
             u.status([{ text: \"ok\", tone: \"succes\" }]);\n\
             u.screen(u.text(\"x\"), { stauts: \"q quit\" });\n",
        );
        let want = [
            "app.ts:2:3: `ui.colum` is not a ui builder; did you mean `column`?",
            "app.ts:3:10: `txt` is not a property of ui.text; did you mean `text`?",
            "app.ts:4:23: `tone` of ui.badge is one of \"neutral\", \"accent\", \"success\", \"warning\", \"danger\", not \"loud\"",
            "app.ts:5:9: `ratio` of ui.gauge takes a number, not a string",
            "app.ts:6:34: `onPress` of ui.button takes a function, not a number",
            "app.ts:7:37: an item of `sizes` of ui.split takes a number, not a string",
            "app.ts:8:31: `tone` of StatusItem is not \"succes\"; did you mean `success`?",
            "app.ts:9:25: `stauts` is not a property of ui.screen; did you mean `status`?",
        ];
        assert_eq!(p, want);
    }

    #[test]
    fn what_is_correct_or_unknown_is_left_alone() {
        let p = found(
            "import { app, ui, dreamer } from \"dreamer\";\n\
             interface Pane { id: number }\n\
             const opts = { txt: 1 };\n\
             app({\n\
               state: { filter: \"\" },\n\
               refresh: 1000,\n\
               async load() { return await dreamer.tmux.topology({ endpoint: undefined }); },\n\
               view(s, data: any) {\n\
                 const local = { tmux: { topolgy: () => 1 } };\n\
                 local.tmux.topolgy();\n\
                 return ui.screen(ui.column([\n\
                   ui.text(opts as any),\n\
                   ui.input({ key: 1, value: s.filter, onChange: (v: string) => ({ filter: v }) }),\n\
                   ui.table({ columns: [\"id\"], rows: data.panes, onEnter: async (p: Pane) => {\n\
                     await dreamer.actions.submit({ action: { kind: { select_pane: { pane: p.id } } } });\n\
                   } }),\n\
                   ui.select([\"a\", \"b\"], { value: \"a\" }),\n\
                   ui.badge({ text: `n ${1}`, tone: \"accent\" }),\n\
                 ]), { status: \"q quit\", focus: \"f\" });\n\
               },\n\
             });\n\
             function shadow(ui: any) { return ui.nothing(); }\n\
             dreamer.call(\"x.\" + \"y\");\n\
             const e = new dreamer.DreamerError(\"m\", 1);\n",
        );
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn app_takes_only_its_fields() {
        let p = found("app({ veiw() { return null; }, refresh: \"1s\" });\n");
        assert_eq!(
            p,
            [
                "app.ts:1:7: `veiw` is not a property of app(); did you mean `view`?",
                "app.ts:1:41: `refresh` of app() takes a number, not a string",
            ]
        );
    }

    /// The generated declarations are TypeScript a declaration file may
    /// hold (no compiler here, so swc's parser is the judge).
    #[test]
    fn the_declarations_parse_as_a_declaration_file() {
        GLOBALS.set(&Globals::new(), || {
            let cm: Lrc<SourceMap> = Lrc::default();
            let fm = cm.new_source_file(
                Lrc::new(FileName::Custom("dreamer.d.ts".into())),
                api::TYPESCRIPT_DECLARATIONS.to_owned(),
            );
            let syntax = Syntax::Typescript(TsSyntax {
                dts: true,
                ..TsSyntax::default()
            });
            let mut recovered = Vec::new();
            let m = swc_ecma_parser::parse_file_as_script(
                &fm,
                syntax,
                EsVersion::latest(),
                None,
                &mut recovered,
            );
            assert!(m.is_ok() && recovered.is_empty(), "{m:?} {recovered:?}");
        });
    }

    /// Agents learn the components from the `dreamer-apps` skill, so it
    /// names every builder and property the check accepts.
    #[test]
    fn the_apps_skill_names_every_builder_and_property() {
        let skill = providers::pi_profile::shared_skills()
            .into_iter()
            .find(|(name, _)| *name == "dreamer-apps")
            .map(|(_, text)| text)
            .expect("the dreamer-apps skill");
        let mut missing = Vec::new();
        for (name, props) in &known().props {
            if !skill.contains(&format!("`{name}`")) {
                missing.push(format!("ui.{name}"));
            }
            for prop in props.keys() {
                if !skill.contains(&format!("`{prop}`")) {
                    missing.push(format!("ui.{name} {prop}"));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "not in the skill: {missing:?}");
    }
}
