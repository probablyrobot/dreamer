//! Durable memory: facts worth keeping across agent sessions and harnesses.
//!
//! A [`Provider`] keeps memories in four scopes (user, host, project,
//! session) and finds them again by words. The daemon serves the
//! `memory.*` API through whichever provider the settings name; this
//! crate holds the interface, the deterministic full-text ranking every
//! provider can share ([`search`]), and the first provider, [`Files`]:
//! one markdown file per memory plus a `MEMORY.md` index, the layout
//! Claude Code's memory uses. See `docs/memory.md`.
//!
//! Headless: no GUI, async runtime, or model. Calls do blocking file I/O;
//! the daemon runs them off its event loop.

mod files;
mod index;
pub mod search;
mod time;

use std::path::PathBuf;

pub use files::Files;
pub use index::{INDEX_BYTES, INDEX_HEAD, INDEX_LINES, index};

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
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(id) => write!(f, "no memory `{id}`"),
            Error::Invalid(m) | Error::Io(m) => f.write_str(m),
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
    /// The settings name: `files`.
    fn name(&self) -> &'static str;

    fn remember(&self, cx: &Context, new: New) -> Result<Memory, Error>;

    /// Newest first.
    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error>;

    fn get(&self, cx: &Context, id: &str) -> Result<Memory, Error>;

    fn update(&self, cx: &Context, id: &str, change: Change) -> Result<Memory, Error>;

    /// Remove a memory; returns what was removed.
    fn forget(&self, cx: &Context, id: &str) -> Result<Memory, Error>;

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
