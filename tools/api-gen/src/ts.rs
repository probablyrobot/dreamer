//! The TypeScript side of the schema: `dreamer.d.ts`, the declarations an
//! app.ts is written against (the API from api.yaml, the `ui` builders from
//! ui.yaml), and `app-types.json`, the same facts as data for `dream app
//! check`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::schema::{Kind, Schema};

/// `crates/api/schema/ui.yaml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    pub types: BTreeMap<String, UiType>,
    pub common: BTreeMap<String, String>,
    pub components: Vec<Component>,
    pub screen: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiType {
    pub values: Option<Vec<String>>,
    pub fields: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    pub form: Form,
    pub first: Option<String>,
    pub doc: Option<String>,
    #[serde(default)]
    pub props: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Form {
    Layout,
    Content,
    Props,
}

impl Form {
    fn name(self) -> &'static str {
        match self {
            Form::Layout => "layout",
            Form::Content => "content",
            Form::Props => "props",
        }
    }
}

/// The names the declarations define beside the schema's own types.
const OWN_NAMES: [&str; 12] = [
    "In",
    "Api",
    "DreamerError",
    "Events",
    "Node",
    "Child",
    "Screen",
    "ScreenOptions",
    "Patch",
    "Ctx",
    "App",
    "Ui",
];

const BASE_TYPES: [&str; 6] = ["string", "number", "boolean", "unknown", "any", "Child"];

impl Ui {
    pub fn validate(&self, schema: &Schema) -> Vec<String> {
        let mut errors = Vec::new();
        let mut taken: BTreeSet<String> = OWN_NAMES.iter().map(|s| (*s).to_owned()).collect();
        taken.insert("Common".into());
        for c in &self.components {
            if !taken.insert(props_name(&c.name)) {
                errors.push(format!("ui.yaml: component `{}` defined twice", c.name));
            }
        }
        for name in self.types.keys() {
            if !taken.insert(name.clone()) {
                errors.push(format!("ui.yaml: type `{name}` clashes with another name"));
            }
        }
        for name in taken.iter() {
            if schema.kind(name).is_some() {
                errors.push(format!(
                    "api.yaml type `{name}` clashes with a name dreamer.d.ts defines"
                ));
            }
        }
        for (name, t) in &self.types {
            match (&t.values, &t.fields) {
                (Some(v), None) if !v.is_empty() => {}
                (None, Some(fields)) => {
                    for (f, ty) in fields {
                        self.check_type(&format!("{name}.{f}"), ty, &mut errors);
                    }
                }
                _ => errors.push(format!(
                    "ui.yaml: type `{name}` needs either `values` or `fields`"
                )),
            }
        }
        for (p, ty) in self.common.iter().chain(&self.screen) {
            self.check_type(p, ty, &mut errors);
        }
        for c in &self.components {
            for (p, ty) in &c.props {
                self.check_type(&format!("{}.{p}", c.name), ty, &mut errors);
                if self.common.contains_key(p) {
                    errors.push(format!("ui.yaml: {}.{p} is a common property", c.name));
                }
            }
            match (c.form, &c.first) {
                (Form::Content, Some(f)) if c.props.contains_key(f) => {}
                (Form::Content, _) => errors.push(format!(
                    "ui.yaml: {}: a content builder's `first` names one of its props",
                    c.name
                )),
                (_, Some(_)) => errors.push(format!(
                    "ui.yaml: {}: only a content builder has `first`",
                    c.name
                )),
                (Form::Layout, None) if !c.props.contains_key("children") => errors.push(format!(
                    "ui.yaml: {}: a layout builder takes `children`",
                    c.name
                )),
                _ => {}
            }
        }
        errors
    }

    /// `A | B`, `T[]`, a base type, a type of this file, or a function.
    fn check_type(&self, at: &str, ty: &str, errors: &mut Vec<String>) {
        if ty.starts_with('(') {
            if !ty.contains("=>") {
                errors.push(format!("ui.yaml: {at}: `{ty}` is not a function type"));
            }
            return;
        }
        for part in ty.split('|').map(str::trim) {
            let base = part.trim_end_matches("[]");
            if !BASE_TYPES.contains(&base) && !self.types.contains_key(base) {
                errors.push(format!("ui.yaml: {at}: unknown type `{part}`"));
            }
        }
    }
}

fn props_name(component: &str) -> String {
    format!("{}Props", crate::schema::camel(component))
}

fn jsdoc(o: &mut String, indent: &str, doc: Option<&str>) {
    let Some(d) = doc else { return };
    let d = d.replace("*/", "* /");
    let lines: Vec<&str> = d.lines().collect();
    if let [one] = lines.as_slice() {
        let _ = writeln!(o, "{indent}/** {one} */");
        return;
    }
    let _ = writeln!(o, "{indent}/**");
    for l in lines {
        let _ = writeln!(o, "{indent} * {l}");
    }
    let _ = writeln!(o, "{indent} */");
}

fn ts_scalar(ty: &str) -> &str {
    match ty {
        "string" | "bytes" => "string",
        "bool" => "boolean",
        "u32" | "u64" | "i32" | "i64" => "number",
        other => other,
    }
}

/// The methods as a tree of namespaces: `agent.sessions.create` under
/// `agent`, then `sessions`.
#[derive(Default)]
struct Tree<'a> {
    groups: BTreeMap<&'a str, Tree<'a>>,
    methods: Vec<&'a crate::schema::MethodDef>,
}

fn method_tree(s: &Schema) -> Tree<'_> {
    let mut root = Tree::default();
    for m in &s.methods {
        let parts: Vec<&str> = m.method.split('.').collect();
        let mut t = &mut root;
        for p in &parts[..parts.len() - 1] {
            t = t.groups.entry(p).or_default();
        }
        t.methods.push(m);
    }
    root
}

fn render_tree(o: &mut String, t: &Tree<'_>, depth: usize) {
    let indent = "  ".repeat(depth + 1);
    for m in &t.methods {
        jsdoc(o, &indent, m.doc.as_deref());
        let name = m.method.rsplit('.').next().unwrap_or(&m.method);
        let _ = writeln!(
            o,
            "{indent}{name}(params?: In<{}>): Promise<{}>;",
            m.params, m.result
        );
    }
    for (name, g) in &t.groups {
        let _ = writeln!(o, "{indent}readonly {name}: {{");
        render_tree(o, g, depth + 1);
        let _ = writeln!(o, "{indent}}};");
    }
}

pub fn dts(s: &Schema, ui: &Ui) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "// @generated by tools/api-gen from crates/api/schema/api.yaml and ui.yaml. Do not edit.\n\
         // Dreamer API {}.{} ({}) for app.ts: the globals the app host gives an app\n\
         // (`app`, `ui`, `dreamer`, timers, `console`) and the module \"dreamer\" it may\n\
         // import them from. Write it beside app.ts with `dream app types DIR`.\n",
        s.version.major, s.version.minor, s.package
    );
    o.push_str("declare namespace Dreamer {\n");
    o.push_str("  // ---- API records (JSON: enums by name, bytes as base64, 64-bit numbers as numbers)\n\n");
    for (name, t) in s.enums() {
        jsdoc(&mut o, "  ", t.doc.as_deref());
        let names: Vec<String> = t.values.iter().map(|v| format!("\"{}\"", v.name)).collect();
        let _ = writeln!(o, "  type {name} = {};\n", names.join(" | "));
    }
    for (name, t) in s.messages() {
        jsdoc(&mut o, "  ", t.doc.as_deref());
        if t.fields.is_empty() && t.oneof.is_none() {
            let _ = writeln!(o, "  interface {name} {{}}\n");
            continue;
        }
        let _ = writeln!(o, "  interface {name} {{");
        for f in &t.fields {
            jsdoc(&mut o, "    ", f.doc.as_deref());
            let base = ts_scalar(&f.ty);
            // A result always holds its scalars, lists, and enums; an
            // optional scalar or a message may be absent.
            let absent = f.optional || (!f.repeated && s.kind(&f.ty) == Some(Kind::Message));
            let ty = if f.repeated {
                format!("{base}[]")
            } else {
                base.to_owned()
            };
            let q = if absent { "?" } else { "" };
            let _ = writeln!(o, "    {}{q}: {ty};", f.name);
        }
        if let Some(of) = &t.oneof {
            let variants: Vec<String> = of
                .variants
                .iter()
                .map(|v| format!("{{ {}: {} }}", v.name, v.ty))
                .collect();
            let _ = writeln!(o, "    /** Exactly one of these. */");
            let _ = writeln!(o, "    {}?: {};", of.name, variants.join(" | "));
        }
        o.push_str("  }\n\n");
    }
    o.push_str(
        "  /** A method's parameters: any field may be left out, and takes its default. */\n  \
         type In<T> = T extends readonly (infer U)[] ? In<U>[] : T extends object ? { [K in keyof T]?: In<T[K]> } : T;\n\n",
    );
    o.push_str(
        "  /** What the daemon pushes, by topic: `onEvent`'s payloads. */\n  interface Events {\n",
    );
    for n in &s.notifications {
        jsdoc(&mut o, "    ", n.doc.as_deref());
        let _ = writeln!(o, "    \"{}\": {};", n.topic, n.payload);
    }
    o.push_str("  }\n\n");
    o.push_str(
        "  /** A refused or failed call: the API's error `code` and `message`. */\n  \
         class DreamerError extends Error {\n    readonly code: number;\n  }\n\n",
    );
    o.push_str(
        "  /**\n   * The API: one async function per method, named as in the API. Only the\n   \
         * methods the app's capabilities allow exist at run time.\n   */\n  \
         interface Api {\n    /** Call a method by name. */\n    \
         call(method: string, params?: object): Promise<unknown>;\n    \
         readonly DreamerError: typeof DreamerError;\n",
    );
    render_tree(&mut o, &method_tree(s), 1);
    o.push_str("  }\n\n");

    o.push_str("  // ---- Views\n\n");
    for (name, t) in &ui.types {
        if let Some(values) = &t.values {
            let v: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
            let _ = writeln!(o, "  type {name} = {};\n", v.join(" | "));
        } else if let Some(fields) = &t.fields {
            let _ = writeln!(o, "  interface {name} {{");
            for (f, ty) in fields {
                let _ = writeln!(o, "    {f}?: {ty};");
            }
            o.push_str("  }\n\n");
        }
    }
    o.push_str(
        "  /** A node, built with `ui.*`. */\n  interface Node {\n    readonly type: string;\n  }\n\n  \
         /** A child: a node, or text; `null`, `undefined`, and `false` are left out. */\n  \
         type Child = Node | string | number | null | undefined | false;\n\n  \
         /** What a handler returns: a part of the state to change, or a promise of one. */\n  \
         type Patch = object | void | null | undefined | Promise<object | void | null | undefined>;\n\n  \
         interface Ctx<S = any, D = any> {\n    readonly state: S;\n    readonly data: D;\n    \
         /** Whether the host is connected to the daemon. */\n    readonly live: boolean;\n    \
         set(partial: Partial<S>): void;\n    reload(): void;\n  }\n\n",
    );
    o.push_str("  interface ScreenOptions {\n");
    for (p, ty) in &ui.screen {
        let _ = writeln!(o, "    {p}?: {ty};");
    }
    o.push_str("  }\n\n  interface Screen extends ScreenOptions {\n    readonly screen: true;\n    root: Child;\n  }\n\n");
    o.push_str("  /** Every builder's properties take these too. */\n  interface Common {\n");
    for (p, ty) in &ui.common {
        let _ = writeln!(o, "    {p}?: {ty};");
    }
    o.push_str("  }\n\n");
    for c in &ui.components {
        let _ = writeln!(o, "  interface {} extends Common {{", props_name(&c.name));
        for (p, ty) in &c.props {
            let _ = writeln!(o, "    {}?: {ty};", prop_key(p));
        }
        o.push_str("  }\n\n");
    }
    o.push_str("  interface Ui {\n");
    for c in &ui.components {
        let props = props_name(&c.name);
        let name = &c.name;
        match (c.form, &c.first) {
            (Form::Layout, _) => {
                jsdoc(&mut o, "    ", c.doc.as_deref());
                let _ = writeln!(o, "    {name}(children: Child[], props?: {props}): Node;");
            }
            (Form::Content, Some(first)) => {
                jsdoc(&mut o, "    ", c.doc.as_deref());
                let ty = c.props.get(first).map(String::as_str).unwrap_or("unknown");
                let _ = writeln!(o, "    {name}({first}: {ty}, props?: {props}): Node;");
            }
            _ => {}
        }
        jsdoc(&mut o, "    ", c.doc.as_deref());
        let _ = writeln!(o, "    {name}(props?: {props}): Node;");
    }
    o.push_str(
        "    /** The view with its status line, focused node's key, and title. */\n    \
         screen(root: Child, options?: ScreenOptions): Screen;\n  }\n\n",
    );
    o.push_str(
        "  /** What `app()` takes. */\n  interface App<S, D> {\n    \
         /** The starting state. */\n    state?: S;\n    \
         /** The app's `data`; called on connecting, on `ctx.reload()`, and every `refresh` ms. */\n    \
         load?(ctx: Ctx<S, D>): D | Promise<D>;\n    \
         /** Reload every this many milliseconds, at least 200. */\n    refresh?: number;\n    \
         view(state: S, data: D, ctx: Ctx<S, D>): Child | Screen;\n    \
         /** A key no component took. */\n    onKey?(key: string, ctx: Ctx<S, D>): Patch;\n    \
         /** An event from a subscription the app made (see `Events`). */\n    \
         onEvent?(topic: string, payload: any, ctx: Ctx<S, D>): Patch;\n  }\n}\n\n",
    );
    o.push_str(
        "/** Start the app; called once. */\n\
         declare var app: <S extends object = {}, D = any>(spec: Dreamer.App<S, D>) => void;\n\
         declare var ui: Dreamer.Ui;\n\
         declare var dreamer: Dreamer.Api;\n\
         declare function setTimeout(fn: (...args: any[]) => unknown, ms?: number, ...args: any[]): number;\n\
         declare function setInterval(fn: (...args: any[]) => unknown, ms?: number, ...args: any[]): number;\n\
         declare function clearTimeout(id?: number): void;\n\
         declare function clearInterval(id?: number): void;\n\
         interface Console {\n  log(...data: unknown[]): void;\n  warn(...data: unknown[]): void;\n  error(...data: unknown[]): void;\n}\n\
         declare var console: Console;\n\n\
         /** Request headers: an object, `[name, value]` pairs, or `Headers`. */\n\
         type HeadersInit = Record<string, string> | [string, string][] | Headers;\n\
         /** What `fetch` takes beside the URL. */\n\
         interface RequestInit {\n  \
         /** GET (the default), HEAD, POST, PUT, PATCH, DELETE, or OPTIONS. */\n  method?: string;\n  \
         headers?: HeadersInit;\n  \
         /** A string; none for GET and HEAD. */\n  body?: string | null;\n  \
         /** Follow redirects to listed hosts (the default), fail on one, or answer with it. */\n  \
         redirect?: \"follow\" | \"error\" | \"manual\";\n}\n\
         /** Headers, names in lowercase. */\n\
         declare class Headers {\n  \
         constructor(init?: HeadersInit);\n  \
         append(name: string, value: string): void;\n  \
         set(name: string, value: string): void;\n  \
         get(name: string): string | null;\n  \
         has(name: string): boolean;\n  \
         delete(name: string): void;\n  \
         forEach(fn: (value: string, name: string, headers: Headers) => void): void;\n  \
         entries(): IterableIterator<[string, string]>;\n  \
         keys(): IterableIterator<string>;\n  \
         values(): IterableIterator<string>;\n  \
         [Symbol.iterator](): IterableIterator<[string, string]>;\n}\n\
         /** A response, its body read whole. */\n\
         interface Response {\n  \
         readonly status: number;\n  \
         readonly statusText: string;\n  \
         /** The status is 200 to 299. */\n  readonly ok: boolean;\n  \
         /** The URL it came from, after redirects. */\n  readonly url: string;\n  \
         readonly redirected: boolean;\n  \
         readonly headers: Headers;\n  \
         readonly bodyUsed: boolean;\n  \
         text(): Promise<string>;\n  \
         json(): Promise<any>;\n}\n\
         /** An HTTP request to a host the manifest's `[net] hosts` lists; needs `net.fetch` in the app's grant. Anything else, a redirect to an unlisted host included, rejects with a `TypeError`. */\n\
         declare function fetch(url: string | { url: string }, init?: RequestInit): Promise<Response>;\n\n\
         declare module \"dreamer\" {\n  \
         export const app: typeof globalThis.app;\n  \
         export const ui: Dreamer.Ui;\n  \
         export const dreamer: Dreamer.Api;\n}\n",
    );
    o
}

/// A property name as TypeScript writes it.
fn prop_key(p: &str) -> String {
    if p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        p.to_owned()
    } else {
        format!("\"{p}\"")
    }
}

/// The facts `dream app check` checks against, as JSON.
pub fn data(s: &Schema, ui: &Ui) -> String {
    let methods: Map<String, Value> = s
        .methods
        .iter()
        .map(|m| (m.method.clone(), json!(m.params)))
        .collect();
    let messages: Map<String, Value> = s
        .messages()
        .map(|(name, t)| {
            let fields: Map<String, Value> = t
                .fields
                .iter()
                .map(|f| {
                    (
                        f.name.clone(),
                        json!({ "type": f.ty, "repeated": f.repeated }),
                    )
                })
                .collect();
            let mut v = json!({ "fields": fields });
            if let Some(of) = &t.oneof {
                let variants: Map<String, Value> = of
                    .variants
                    .iter()
                    .map(|v| (v.name.clone(), json!(v.ty)))
                    .collect();
                v["oneof"] = json!({ "name": of.name, "variants": variants });
            }
            (name.to_owned(), v)
        })
        .collect();
    let enums: Map<String, Value> = s
        .enums()
        .map(|(name, t)| {
            let values: Vec<&str> = t.values.iter().map(|v| v.name.as_str()).collect();
            (name.to_owned(), json!(values))
        })
        .collect();
    let types: Map<String, Value> = ui
        .types
        .iter()
        .map(|(name, t)| {
            let v = match (&t.values, &t.fields) {
                (Some(values), _) => json!({ "values": values }),
                (_, fields) => json!({ "fields": fields }),
            };
            (name.clone(), v)
        })
        .collect();
    let components: Vec<Value> = ui
        .components
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "form": c.form.name(),
                "first": c.first,
                "props": c.props,
            })
        })
        .collect();
    let doc = json!({
        "version": format!("{}.{}", s.version.major, s.version.minor),
        "methods": methods,
        "messages": messages,
        "enums": enums,
        "ui": {
            "types": types,
            "common": ui.common,
            "components": components,
            "screen": ui.screen,
        },
    });
    let mut out = serde_json::to_string_pretty(&doc).unwrap_or_default();
    out.push('\n');
    out
}
