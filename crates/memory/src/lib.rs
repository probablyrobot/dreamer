//! Durable memory: facts worth keeping across agent sessions and harnesses.
//!
//! A [`Provider`] keeps memories in four scopes (user, host, project,
//! session) and finds them again by words. The daemon serves the
//! `memory.*` API through whichever provider the settings name; this
//! crate holds the interface, the deterministic full-text ranking every
//! provider can share ([`search`]), and the providers: [`Files`], one
//! markdown file per memory plus a `MEMORY.md` index, the layout Claude
//! Code's memory uses; and, with the `kosmonaut` feature, `Kosmonaut`, a
//! graph database embedded in the daemon or reached as a server over gRPC.
//! [`transfer`] copies memories from one provider to another. See
//! `docs/memory.md`.
//!
//! Headless: no GUI, and no model unless an [`embed`] embedder is
//! configured. Calls do blocking I/O; the daemon runs them off its event
//! loop. Only the Kosmonaut server client runs an async runtime, a small
//! one of its own.

pub mod embed;
mod files;
mod index;
#[cfg(feature = "kosmonaut")]
mod kosmonaut;
pub mod search;
mod time;

use std::path::{Path, PathBuf};

pub use files::Files;
pub use index::{INDEX_BYTES, INDEX_HEAD, INDEX_LINES, index};
#[cfg(feature = "kosmonaut")]
pub use kosmonaut::{CollectorRow, Kosmonaut, Server, Summary};

/// Where a memory applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// The user, on every host and in every project.
    User,
    /// This host only.
    Host,
    /// One project, found from the caller's working directory.
    Project,
    /// One agent session.
    Session,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::User, Scope::Host, Scope::Project, Scope::Session];

    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Host => "host",
            Scope::Project => "project",
            Scope::Session => "session",
        }
    }

    pub fn from_name(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// What a memory is about: Claude Code's four kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    User,
    Feedback,
    Project,
    Reference,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::User, Kind::Feedback, Kind::Project, Kind::Reference];

    pub fn name(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Feedback => "feedback",
            Kind::Project => "project",
            Kind::Reference => "reference",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// Who is asking: what the project and session scopes are resolved from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// The caller's working directory (absolute).
    pub cwd: Option<PathBuf>,
    /// The agent session's id.
    pub session: Option<String>,
}

/// One memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// `<scope>/<name>`; stable while the memory exists.
    pub id: String,
    pub scope: Scope,
    pub kind: Kind,
    pub title: String,
    /// One line, for the index.
    pub description: String,
    pub text: String,
    pub links: Vec<String>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    /// The file that holds it, for a provider that keeps files.
    pub path: Option<PathBuf>,
    /// Set by recall: how well it matched, higher is better.
    pub score: Option<u32>,
}

/// A new memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct New {
    pub text: String,
    pub kind: Kind,
    pub scope: Scope,
    /// Default: the text's first line, shortened.
    pub title: Option<String>,
    /// Default: the title.
    pub description: Option<String>,
    pub links: Vec<String>,
}

/// A change to a memory: `None` keeps what is there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    pub text: Option<String>,
    pub kind: Option<Kind>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub links: Option<Vec<String>>,
}

/// A stored memory that could not be read. It is left alone and skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: PathBuf,
    pub message: String,
}

/// Memories, and what could not be read on the way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub items: Vec<Memory>,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No memory with this id.
    NotFound(String),
    /// The request itself is wrong: an empty text, a bad id, or a scope
    /// the context does not reach (project without a directory, session
    /// without an id).
    Invalid(String),
    /// The memory exists but cannot be read.
    Malformed(Problem),
    /// The store failed.
    Io(String),
    /// The store cannot be reached: a Kosmonaut server that is down, does
    /// not answer in time, or refuses the token. Memory is unavailable
    /// until it is back; nothing else depends on it.
    Unavailable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(id) => write!(f, "no memory `{id}`"),
            Error::Invalid(m) | Error::Io(m) => f.write_str(m),
            Error::Unavailable(m) => write!(f, "memory unavailable: {m}"),
            Error::Malformed(p) => write!(f, "{}: {}", p.path.display(), p.message),
        }
    }
}

impl std::error::Error for Error {}

/// Default and largest number of memories recall returns.
pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 100;

/// A memory store. Every method resolves the project and session scopes
/// from `cx`; `scope: None` means every scope `cx` reaches. Providers
/// keep no cache that could go stale against their store.
pub trait Provider: Send + Sync + std::fmt::Debug {
    /// The settings name: `files` or `kosmonaut`.
    fn name(&self) -> &'static str;

    fn remember(&self, cx: &Context, new: New) -> Result<Memory, Error>;

    /// Newest first.
    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error>;

    fn get(&self, cx: &Context, id: &str) -> Result<Memory, Error>;

    fn update(&self, cx: &Context, id: &str, change: Change) -> Result<Memory, Error>;

    /// Remove a memory; returns what was removed.
    fn forget(&self, cx: &Context, id: &str) -> Result<Memory, Error>;

    /// Keep `m` as it is, under its id and with its times, replacing a
    /// memory with the same id: how memories move between providers
    /// ([`transfer`]). `m.path` and `m.score` are ignored.
    fn put(&self, cx: &Context, m: &Memory) -> Result<Memory, Error>;

    /// Memories matching `query`, best first; at most `limit`
    /// ([`DEFAULT_LIMIT`] when `None`, never more than [`MAX_LIMIT`]).
    fn recall(
        &self,
        cx: &Context,
        query: &str,
        scope: Option<Scope>,
        limit: Option<usize>,
    ) -> Result<Found, Error> {
        let mut found = self.list(cx, scope, None)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        found.items = search::rank(query, found.items, limit);
        Ok(found)
    }
}

/// The scopes `cx` reaches: user and host always, project with a working
/// directory, session with a session id.
pub fn reach(cx: &Context) -> Vec<Scope> {
    Scope::ALL
        .into_iter()
        .filter(|s| match s {
            Scope::Project => cx.cwd.is_some(),
            Scope::Session => cx.session.is_some(),
            _ => true,
        })
        .collect()
}

/// The project a working directory belongs to: the nearest directory at
/// or above it that holds `.dreamer/memory` or `.git`, else the directory
/// itself.
pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|a| a.join(".dreamer").join("memory").is_dir() || a.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// The project `cx` names, checked.
pub(crate) fn project_of(cx: &Context) -> Result<PathBuf, Error> {
    let cwd = cx
        .cwd
        .as_deref()
        .ok_or_else(|| Error::Invalid("the project scope needs a working directory".into()))?;
    if !cwd.is_absolute() {
        return Err(Error::Invalid(format!(
            "the working directory `{}` is not absolute",
            cwd.display()
        )));
    }
    Ok(project_root(cwd))
}

/// The session `cx` names, checked: letters, digits, `.`, `_`, `-`.
pub(crate) fn session_of(cx: &Context) -> Result<&str, Error> {
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
    Ok(id)
}

/// `user/prefers-tabs` to its scope and name.
pub(crate) fn split_id(id: &str) -> Result<(Scope, &str), Error> {
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

/// What [`transfer`] copied, and what it could not read on the way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moved {
    /// The ids copied, newest first.
    pub ids: Vec<String>,
    pub problems: Vec<Problem>,
}

/// Copy every memory `from` holds in `scope` (every scope `cx` reaches
/// when `None`) into `to`, keeping ids, kinds, text, links, and times.
/// A memory `to` already holds under the same id is replaced; nothing is
/// removed from either side. Import from `files` and export back to it are
/// this, with the providers swapped.
pub fn transfer(
    from: &dyn Provider,
    to: &dyn Provider,
    cx: &Context,
    scope: Option<Scope>,
) -> Result<Moved, Error> {
    let found = from.list(cx, scope, None)?;
    let mut moved = Moved {
        ids: Vec::with_capacity(found.items.len()),
        problems: found.problems,
    };
    for m in &found.items {
        to.put(cx, m)?;
        moved.ids.push(m.id.clone());
    }
    Ok(moved)
}
