//! `dream app new`: a working package to start from, one of a few
//! templates kept in this program. Each is a script app (`app.ts`) with
//! the fixtures to preview and test it, the declarations for an editor,
//! and the expected screens of its test recorded.

use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: dream app new NAME [--template table|form|chat] [--dir DIR]";

/// A template: its capabilities and its files beside `app.toml`.
struct Template {
    name: &'static str,
    capabilities: &'static [&'static str],
    all_panes: bool,
    files: &'static [(&'static str, &'static str)],
}

const EVENTS_HEADER: &str = "\
# One event per line, as the app protocol sends them: key, text, select,
# scroll, or component. `dream app test` replays them and compares the
# screen after each with fixtures/expect/NNN.txt (000.txt is the first
# view); `dream app test DIR --update` records them again.
";

const TABLE_TS: &str = r#"import { app, ui, dreamer } from "dreamer";

// What load() returns. fixtures/load.json holds the same shape, so that
// `dream app preview` and `dream app test` draw without a daemon.
interface Data {
  panes: Dreamer.Pane[];
}

interface State {
  status: string;
}

app({
  state: { status: "Enter selects the pane" } as State,
  async load(): Promise<Data> {
    const t = await dreamer.tmux.topology();
    return { panes: t.panes };
  },
  view(s: State, data: Data) {
    return ui.screen(
      ui.table({
        key: "panes",
        title: "Panes",
        columns: ["id", "current_command", "width", "height"],
        rows: data.panes,
        onEnter: async (p: Dreamer.Pane) => {
          await dreamer.actions.submit({ action: { kind: { select_pane: { pane: p.id } } } });
          return { status: `selected %${p.id}` };
        },
      }),
      { status: s.status },
    );
  },
});
"#;

const TABLE_LOAD: &str = r#"{
  "panes": [
    { "id": 1, "window": 1, "current_command": "zsh", "width": 120, "height": 40, "title": "shell" },
    { "id": 2, "window": 1, "current_command": "cargo", "width": 80, "height": 40, "title": "build" },
    { "id": 3, "window": 2, "current_command": "vim", "width": 200, "height": 50, "title": "edit" }
  ]
}
"#;

const TABLE_EVENTS: &str = r#"{"kind":"key","key":"Down"}
{"kind":"key","key":"Enter"}
"#;

const ACTION_DONE: &str = r#"{
  "actions.submit": {
    "id": "fixture-1",
    "state": "completed",
    "endpoint": "local",
    "created_unix_ms": 0,
    "updated_unix_ms": 0
  }
}
"#;

const FORM_TS: &str = r#"import { app, ui, dreamer } from "dreamer";

// What load() returns. fixtures/load.json holds the same shape, so that
// `dream app preview` and `dream app test` draw without a daemon.
interface Data {
  windows: Dreamer.Window[];
}

interface State {
  window: number;
  name: string;
  status: string;
}

app({
  state: { window: 0, name: "", status: "Tab moves between the fields" } as State,
  async load(): Promise<Data> {
    const t = await dreamer.tmux.topology();
    return { windows: t.windows };
  },
  view(s: State, data: Data) {
    const rename = async () => {
      const w = data.windows[s.window];
      if (w === undefined) return { status: "no window to rename" };
      if (s.name.trim() === "") return { status: "a name is needed" };
      await dreamer.actions.submit({ action: { kind: { rename_window: { window: w.id, name: s.name } } } });
      return { status: `renamed @${w.id} to ${s.name}`, name: "" };
    };
    return ui.screen(
      ui.column({
        key: "form",
        title: "Rename a window",
        children: [
          ui.select({
            key: "window",
            label: "Window",
            options: data.windows.map((w) => `@${w.id} ${w.name}`),
            selected: s.window,
            onChange: (_option: string, index: number) => ({ window: index }),
          }),
          ui.input({
            key: "name",
            label: "New name",
            value: s.name,
            placeholder: "a name",
            onChange: (v: string) => ({ name: v }),
            onSubmit: rename,
          }),
          ui.button({ key: "rename", label: "Rename", onPress: rename }),
        ],
      }),
      { status: s.status },
    );
  },
});
"#;

const FORM_LOAD: &str = r#"{
  "windows": [
    { "id": 1, "name": "shell", "index": 0, "panes": [1, 2] },
    { "id": 2, "name": "editor", "index": 1, "panes": [3] }
  ]
}
"#;

const FORM_EVENTS: &str = r#"{"kind":"key","key":"Down"}
{"kind":"key","key":"Tab"}
{"kind":"text","text":"logs"}
{"kind":"key","key":"Enter"}
"#;

const CHAT_TS: &str = r#"import { app, ui, dreamer } from "dreamer";

// What load() returns. fixtures/load.json holds the same shape, so that
// `dream app preview` and `dream app test` draw without a daemon.
interface Data {
  daemon: string;
}

interface Line {
  from: string;
  text: string;
}

interface State {
  lines: Line[];
  draft: string;
}

// The answer to a message. Replace it with the app's own logic: a call
// through `dreamer`, or a message to another app (`dreamer.messages.*`).
function answer(text: string): string {
  return `you said ${text.length} characters`;
}

app({
  state: { lines: [{ from: "app", text: "Type a message; Enter sends it." }], draft: "" } as State,
  async load(): Promise<Data> {
    const v = await dreamer.daemon.version();
    return { daemon: v.daemon_version };
  },
  view(s: State, data: Data) {
    return ui.screen(
      ui.column({
        key: "chat",
        children: [
          ui.log({ key: "transcript", title: "Chat", lines: s.lines.map((l) => `${l.from}: ${l.text}`) }),
          ui.input({
            key: "composer",
            label: "Message",
            value: s.draft,
            placeholder: "type, then Enter",
            onChange: (v: string) => ({ draft: v }),
            onSubmit: (text: string) =>
              text.trim() === ""
                ? null
                : {
                    lines: [...s.lines, { from: "you", text }, { from: "app", text: answer(text) }],
                    draft: "",
                  },
          }),
        ],
      }),
      { status: `dreamerd ${data.daemon}` },
    );
  },
});
"#;

const CHAT_LOAD: &str = r#"{ "daemon": "0.0.0-fixture" }
"#;

const CHAT_EVENTS: &str = r#"{"kind":"text","text":"hello"}
{"kind":"key","key":"Enter"}
"#;

const TEMPLATES: &[Template] = &[
    Template {
        name: "table",
        capabilities: &["tmux.topology", "actions.submit"],
        all_panes: true,
        files: &[
            ("app.ts", TABLE_TS),
            ("fixtures/load.json", TABLE_LOAD),
            ("fixtures/calls.json", ACTION_DONE),
            ("fixtures/events.jsonl", TABLE_EVENTS),
        ],
    },
    Template {
        name: "form",
        capabilities: &["tmux.topology", "actions.submit"],
        all_panes: true,
        files: &[
            ("app.ts", FORM_TS),
            ("fixtures/load.json", FORM_LOAD),
            ("fixtures/calls.json", ACTION_DONE),
            ("fixtures/events.jsonl", FORM_EVENTS),
        ],
    },
    Template {
        name: "chat",
        capabilities: &["daemon.version"],
        all_panes: false,
        files: &[
            ("app.ts", CHAT_TS),
            ("fixtures/load.json", CHAT_LOAD),
            ("fixtures/events.jsonl", CHAT_EVENTS),
        ],
    },
];

/// The templates' names, for messages and tests.
pub fn names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|t| t.name).collect()
}

fn manifest(id: &str, title: &str, t: &Template) -> String {
    let caps: Vec<String> = t.capabilities.iter().map(|c| format!("\"{c}\"")).collect();
    let all_panes = if t.all_panes {
        "all_panes = true      # the panes are other panes, not the app's own\n"
    } else {
        ""
    };
    format!(
        "schema_version = 2\n\
         id = \"{id}\"\n\
         version = \"0.1.0\"\n\
         title = \"{title}\"\n\
         capabilities = [{}]\n\
         {all_panes}\n\
         [script]\n\
         source = \"app.ts\"\n",
        caps.join(", ")
    )
}

/// A package name: lowercase letters, digits, and `-`, starting with a
/// letter; the app's id is `local.NAME`.
fn valid(name: &str) -> Result<(), String> {
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not an app name (lowercase letters, digits, and -, starting with a letter)"
        ))
    }
}

/// `my-app` as `My app`.
fn title(name: &str) -> String {
    let words = name.replace('-', " ");
    let mut c = words.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// `dream app new NAME [--template T] [--dir DIR]`.
pub fn new(args: &[String]) -> Result<(), String> {
    let [name, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    if name.starts_with('-') {
        return Err(USAGE.into());
    }
    let mut template = "table".to_owned();
    let mut dir: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--template" => template = value.clone(),
            "--dir" => dir = Some(PathBuf::from(value)),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    valid(name)?;
    if template == "web" {
        return Err(
            "there is no web template in this version; start from table, form, or chat and \
             add a page with ui.web, or write a web app (\"Web pages and web apps\" in \
             docs/termapp.md)"
                .into(),
        );
    }
    let t = TEMPLATES
        .iter()
        .find(|t| t.name == template)
        .ok_or_else(|| format!("`{template}` is not a template ({})", names().join(", ")))?;
    let dir = dir.unwrap_or_else(|| PathBuf::from(name));
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if dir.exists()
        && std::fs::read_dir(&dir)
            .map_err(|e| fail(&dir, e))?
            .next()
            .is_some()
    {
        return Err(format!("{} exists and is not empty", dir.display()));
    }
    let id = format!("local.{name}");
    let mut files: Vec<(&str, String)> = vec![("app.toml", manifest(&id, &title(name), t))];
    files.extend(t.files.iter().map(|(p, text)| {
        let text = if p.ends_with(".jsonl") {
            format!("{EVENTS_HEADER}{text}")
        } else {
            (*text).to_owned()
        };
        (*p, text)
    }));
    for (path, text) in &files {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fail(parent, e))?;
        }
        std::fs::write(&file, text).map_err(|e| fail(&file, e))?;
    }
    let shown = dir.display().to_string();
    crate::build::types(std::slice::from_ref(&shown))?;
    // Build it, and record what its test expects now.
    let m = dir.join("app.toml");
    crate::strip::build(&m)?;
    match crate::build::record(&m, None) {
        Ok(n) => println!("recorded {n} expected screens in {shown}/fixtures/expect"),
        Err(e) => eprintln!("the expected screens were not recorded: {e}"),
    }
    println!(
        "created {id} from the {template} template in {shown}\n\
         next: edit app.ts, then\n  \
         dream app check {shown}\n  \
         dream app preview {shown} --text\n  \
         dream app test {shown}\n  \
         dream app install {shown}"
    );
    Ok(())
}
