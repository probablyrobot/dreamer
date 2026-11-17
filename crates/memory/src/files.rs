//! The `files` provider: one markdown file per memory, with YAML
//! frontmatter, and a `MEMORY.md` index with one line per memory, in a
//! directory per scope. Every call reads the files again, so a hand edit
//! counts at once; nothing is cached.
//!
//! ```text
//! ---
//! name: Prefers tabs
//! description: Indent with tabs in every language
//! type: feedback
//! created: 2026-11-17T09:30:00Z
//! updated: 2026-11-17T09:30:00Z
//! ---
//!
//! The user indents with tabs, also in YAML where the tool allows it.
//! ```
//!
//! The directory decides a memory's scope; `type` (or `kind`) its kind.
//! Other frontmatter keys are kept as they are. A file that cannot be
//! read is reported and skipped, and never changed or deleted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use serde_norway::{Mapping, Value};

use crate::{Change, Context, Error, Found, Kind, Memory, New, Problem, Provider, Scope, time};

/// The index file in every scope's directory.
pub const INDEX: &str = "MEMORY.md";

/// Longest generated title and file name, in characters.
const TITLE_MAX: usize = 60;
const NAME_MAX: usize = 48;

#[derive(Debug)]
pub struct Files {
    user: Option<PathBuf>,
    host: PathBuf,
    sessions: PathBuf,
    /// Writes in this process one at a time, so two never pick the same
    /// file name or interleave index edits.
    write: Mutex<()>,
}

impl Files {
    /// `user`: the user scope's directory (`<config dir>/memory`), if
    /// there is a config directory. `host`: this host's directory.
    /// `sessions`: where each session's directory is made.
    pub fn new(user: Option<PathBuf>, host: PathBuf, sessions: PathBuf) -> Files {
        Files {
            user,
            host,
            sessions,
            write: Mutex::new(()),
        }
    }

    /// The project a working directory belongs to: the nearest directory
    /// at or above it that holds `.dreamer/memory` or `.git`, else the
    /// directory itself.
    pub fn project_root(cwd: &Path) -> PathBuf {
        cwd.ancestors()
            .find(|a| a.join(".dreamer").join("memory").is_dir() || a.join(".git").exists())
            .unwrap_or(cwd)
            .to_path_buf()
    }

    /// The directory that holds `scope`'s memories for `cx`.
    pub fn dir(&self, cx: &Context, scope: Scope) -> Result<PathBuf, Error> {
        match scope {
            Scope::User => self.user.clone().ok_or_else(|| {
                Error::Invalid("the user scope needs a configuration directory".into())
            }),
            Scope::Host => Ok(self.host.clone()),
            Scope::Project => {
                let cwd = cx.cwd.as_deref().ok_or_else(|| {
                    Error::Invalid("the project scope needs a working directory".into())
                })?;
                if !cwd.is_absolute() {
                    return Err(Error::Invalid(format!(
                        "the working directory `{}` is not absolute",
                        cwd.display()
                    )));
                }
                Ok(Files::project_root(cwd).join(".dreamer").join("memory"))
            }
            Scope::Session => {
                let id = cx
                    .session
                    .as_deref()
                    .ok_or_else(|| Error::Invalid("the session scope needs a session id".into()))?;
                let ok = !id.is_empty()
                    && id.len() <= 128
                    && !id.starts_with('.')
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
                if !ok {
                    return Err(Error::Invalid(format!("`{id}` is not a session id")));
                }
                Ok(self.sessions.join(id))
            }
        }
    }

    fn locate(&self, cx: &Context, id: &str) -> Result<(Scope, PathBuf), Error> {
        let (scope, name) = split_id(id)?;
        Ok((scope, self.dir(cx, scope)?.join(format!("{name}.md"))))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scopes(&self, cx: &Context, scope: Option<Scope>) -> Vec<Scope> {
        match scope {
            Some(s) => vec![s],
            None => crate::reach(cx)
                .into_iter()
                .filter(|s| *s != Scope::User || self.user.is_some())
                .collect(),
        }
    }

    fn existing(&self, cx: &Context, id: &str) -> Result<(Scope, PathBuf, Entry), Error> {
        let (scope, path) = self.locate(cx, id)?;
        if !path.is_file() {
            return Err(Error::NotFound(id.to_owned()));
        }
        let entry = read(&path, scope).map_err(Error::Malformed)?;
        Ok((scope, path, entry))
    }
}

/// `user/prefers-tabs` to its scope and file name.
fn split_id(id: &str) -> Result<(Scope, &str), Error> {
    let bad = || Error::Invalid(format!("`{id}` is not a memory id (scope/name)"));
    let (scope, name) = id.split_once('/').ok_or_else(bad)?;
    let scope = Scope::from_name(scope).ok_or_else(bad)?;
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name != "MEMORY"
        && !name.contains(['/', '\\', '\0']);
    if !ok {
        return Err(bad());
    }
    Ok((scope, name))
}

/// What a file holds besides the memory: frontmatter keys Dreamer does
/// not use, kept on rewrite.
#[derive(Debug)]
struct Entry {
    memory: Memory,
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct Front {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "type", alias = "kind")]
    kind: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    created: Option<Value>,
    updated: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// The frontmatter and the body, if the text starts with a `---` block.
fn split(text: &str) -> Option<(&str, &str)> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))?;
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..at], &rest[at + line.len()..]));
        }
        at += line.len();
    }
    None
}

fn stamp(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::String(s) => time::parse(s),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn file_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read(path: &Path, scope: Scope) -> Result<Entry, Problem> {
    let problem = |message: String| Problem {
        path: path.to_path_buf(),
        message,
    };
    let meta = std::fs::metadata(path).map_err(|e| problem(e.to_string()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    let text = std::fs::read_to_string(path).map_err(|e| problem(e.to_string()))?;
    let (yaml, body) = split(&text).ok_or_else(|| {
        problem("no frontmatter: the file must start with a `---` block holding `type:`".into())
    })?;
    let front: Front = if yaml.trim().is_empty() {
        Front::default()
    } else {
        serde_norway::from_str(yaml).map_err(|e| problem(format!("frontmatter: {e}")))?
    };
    let kinds = || {
        Kind::ALL
            .iter()
            .map(|k| k.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let kind = match front.kind.as_deref() {
        None => return Err(problem(format!("no `type:` ({})", kinds()))),
        Some(k) => Kind::from_name(k.trim())
            .ok_or_else(|| problem(format!("`type: {k}` is not one of {}", kinds())))?,
    };
    let name = file_name(path);
    let title = front
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| name.clone());
    let created = stamp(front.created.as_ref()).unwrap_or(mtime);
    // A hand edit leaves `updated` behind; the file's time is later then.
    let updated = stamp(front.updated.as_ref()).unwrap_or(0).max(mtime);
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    Ok(Entry {
        memory: Memory {
            id: format!("{}/{name}", scope.name()),
            scope,
            kind,
            title,
            description: front.description.unwrap_or_default().trim().to_owned(),
            text: body.trim_end().to_owned(),
            links: front.links,
            created_unix_ms: created,
            updated_unix_ms: updated,
            path: Some(path.to_path_buf()),
            score: None,
        },
        extra: front.extra,
    })
}

/// One line: newlines and runs of space become one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut s: String = text.chars().take(max - 1).collect();
    s = s.trim_end().to_owned();
    s.push('…');
    s
}

/// A title from the text's first line that is not empty.
fn title_of(text: &str) -> String {
    let first = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("memory");
    shorten(first, TITLE_MAX)
}

/// A file name from a title: lowercase letters and digits joined by `-`.
fn slug(title: &str) -> String {
    let mut s = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
        if s.chars().count() >= NAME_MAX {
            break;
        }
    }
    let s = s.trim_end_matches('-').to_owned();
    if s.is_empty() || s == "memory" {
        "note".into()
    } else {
        s
    }
}

/// Write `text` to `path` through a temporary file beside it, so a reader
/// never sees half a file.
fn write_atomic(path: &Path, text: &str) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
    let tmp = path.with_file_name(format!(".{}.tmp", file_name(path)));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

fn render(m: &Memory, extra: &BTreeMap<String, Value>) -> Result<String, Error> {
    let mut map = Mapping::new();
    let s = |v: &str| Value::String(v.to_owned());
    map.insert(s("name"), s(&m.title));
    map.insert(s("description"), s(&m.description));
    map.insert(s("type"), s(m.kind.name()));
    if !m.links.is_empty() {
        map.insert(
            s("links"),
            Value::Sequence(m.links.iter().map(|l| s(l)).collect()),
        );
    }
    map.insert(s("created"), s(&time::format(m.created_unix_ms)));
    map.insert(s("updated"), s(&time::format(m.updated_unix_ms)));
    for (k, v) in extra {
        if ![
            "name",
            "description",
            "type",
            "kind",
            "links",
            "created",
            "updated",
        ]
        .contains(&k.as_str())
        {
            map.insert(s(k), v.clone());
        }
    }
    let yaml =
        serde_norway::to_string(&Value::Mapping(map)).map_err(|e| Error::Io(e.to_string()))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", m.text.trim_end()))
}

/// The index entry for a memory in `file`.
fn index_line(m: &Memory, file: &str) -> String {
    let target = if file.contains([' ', '(', ')', '<', '>']) {
        format!("<{file}>")
    } else {
        file.to_owned()
    };
    let title = m.title.replace(['[', ']'], "");
    if m.description.is_empty() {
        format!("- [{title}]({target})")
    } else {
        format!("- [{title}]({target}) — {}", m.description)
    }
}

/// Point the index line for `file` at `line`, or remove it (`None`).
/// Other lines, a person's own included, stay as they are.
fn set_index(dir: &Path, file: &str, line: Option<String>) -> Result<(), Error> {
    let path = dir.join(INDEX);
    let old = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    let points = |l: &str| l.contains(&format!("]({file})")) || l.contains(&format!("](<{file}>)"));
    let mut out: Vec<String> = Vec::new();
    let mut line = line;
    let mut placed = false;
    for l in old.lines() {
        if points(l) {
            if !placed && let Some(new) = line.take() {
                out.push(new);
            }
            placed = true;
        } else {
            out.push(l.to_owned());
        }
    }
    if let Some(new) = line {
        out.push(new);
    }
    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    if text == old {
        return Ok(());
    }
    write_atomic(&path, &text)
}

impl Provider for Files {
    fn name(&self) -> &'static str {
        "files"
    }

    fn remember(&self, cx: &Context, new: New) -> Result<Memory, Error> {
        if new.text.trim().is_empty() {
            return Err(Error::Invalid("a memory needs text".into()));
        }
        let dir = self.dir(cx, new.scope)?;
        let io = |e: std::io::Error| Error::Io(format!("{}: {e}", dir.display()));
        let _guard = self.lock();
        std::fs::create_dir_all(&dir).map_err(io)?;
        let title = new
            .title
            .as_deref()
            .map(one_line)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| title_of(&new.text));
        let description = new
            .description
            .as_deref()
            .map(one_line)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| title.clone());
        let base = slug(&title);
        let name = (1..)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                }
            })
            .find(|n| !dir.join(format!("{n}.md")).exists())
            .unwrap_or(base);
        let path = dir.join(format!("{name}.md"));
        let now = time::now_ms();
        let m = Memory {
            id: format!("{}/{name}", new.scope.name()),
            scope: new.scope,
            kind: new.kind,
            title,
            description,
            text: new.text.trim_end().to_owned(),
            links: new.links,
            created_unix_ms: now,
            updated_unix_ms: now,
            path: Some(path.clone()),
            score: None,
        };
        write_atomic(&path, &render(&m, &BTreeMap::new())?)?;
        let file = format!("{name}.md");
        set_index(&dir, &file, Some(index_line(&m, &file)))?;
        read(&path, new.scope)
            .map(|e| e.memory)
            .map_err(Error::Malformed)
    }

    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error> {
        let mut found = Found::default();
        for s in self.scopes(cx, scope) {
            let dir = self.dir(cx, s)?;
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Io(format!("{}: {e}", dir.display()))),
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    let n = p
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default();
                    n.ends_with(".md") && n != INDEX && !n.starts_with('.') && p.is_file()
                })
                .collect();
            paths.sort();
            for p in paths {
                match read(&p, s) {
                    Ok(e) if kind.is_none_or(|k| k == e.memory.kind) => {
                        found.items.push(e.memory);
                    }
                    Ok(_) => {}
                    Err(problem) => found.problems.push(problem),
                }
            }
        }
        found.items.sort_by(|a, b| {
            b.updated_unix_ms
                .cmp(&a.updated_unix_ms)
                .then(a.id.cmp(&b.id))
        });
        Ok(found)
    }

    fn get(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        self.existing(cx, id).map(|(_, _, e)| e.memory)
    }

    fn update(&self, cx: &Context, id: &str, change: Change) -> Result<Memory, Error> {
        let _guard = self.lock();
        let (scope, path, entry) = self.existing(cx, id)?;
        let mut m = entry.memory;
        if let Some(t) = change.text {
            if t.trim().is_empty() {
                return Err(Error::Invalid("a memory needs text".into()));
            }
            m.text = t.trim_end().to_owned();
        }
        if let Some(k) = change.kind {
            m.kind = k;
        }
        if let Some(t) = change.title.as_deref().map(one_line) {
            if t.is_empty() {
                return Err(Error::Invalid("a title cannot be empty".into()));
            }
            m.title = t;
        }
        if let Some(d) = change.description.as_deref() {
            m.description = one_line(d);
        }
        if let Some(l) = change.links {
            m.links = l;
        }
        m.updated_unix_ms = time::now_ms().max(m.updated_unix_ms);
        write_atomic(&path, &render(&m, &entry.extra)?)?;
        let file = format!("{}.md", file_name(&path));
        if let Some(dir) = path.parent() {
            set_index(dir, &file, Some(index_line(&m, &file)))?;
        }
        read(&path, scope)
            .map(|e| e.memory)
            .map_err(Error::Malformed)
    }

    fn forget(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        let _guard = self.lock();
        let (_, path, entry) = self.existing(cx, id)?;
        std::fs::remove_file(&path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        if let Some(dir) = path.parent() {
            set_index(dir, &format!("{}.md", file_name(&path)), None)?;
        }
        Ok(entry.memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_names_come_from_the_text() {
        assert_eq!(title_of("\n## Use tabs\nmore"), "Use tabs");
        assert_eq!(slug("Use tabs, not spaces!"), "use-tabs-not-spaces");
        assert_eq!(slug("???"), "note");
        assert_eq!(slug("Grüße aus Köln"), "grüße-aus-köln");
        assert!(split_id("user/x").is_ok());
        for bad in [
            "x",
            "nope/x",
            "user/",
            "user/../x",
            "user/.hidden",
            "user/MEMORY",
        ] {
            assert!(split_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frontmatter_needs_its_fences() {
        assert_eq!(split("---\na: 1\n---\nbody"), Some(("a: 1\n", "body")));
        assert_eq!(
            split("---\r\na: 1\r\n---\r\n\r\nbody"),
            Some(("a: 1\r\n", "\r\nbody"))
        );
        assert_eq!(split("a: 1\n---\nbody"), None);
        assert_eq!(split("---\na: 1\n"), None);
    }
}
