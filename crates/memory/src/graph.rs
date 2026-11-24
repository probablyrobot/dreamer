//! What the database providers (`sqlite`, and `kosmonaut` with its
//! feature) share: how a memory is keyed and owned, which edges its links
//! make, how a new or changed memory is filled in, and how recall merges
//! word matches, neighbours, and vectors. Each provider keeps the same
//! things its own way; with this they answer alike.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::embed::{Embedder, cosine};
use crate::files::{one_line, slug, title_of};
use crate::{
    Change, Context, DEFAULT_LIMIT, Error, Found, MAX_LIMIT, Memory, New, Problem, Scope, search,
    split_id, time,
};

/// Recall: a memory one edge from a match scores this, plus 10 for each
/// further edge, below any word match (every word match scores 1000 or
/// more; see [`crate::search`]).
pub(crate) const RELATED_BASE: u32 = 500;
/// Recall: a memory whose vector is at least this close to the query's
/// (cosine) is included; its score is the similarity times 400.
pub(crate) const NEAR: f32 = 0.5;

/// The relations from a memory: to its project, its session, a file, a
/// pane, and another memory.
pub(crate) const IN_PROJECT: &str = "IN_PROJECT";
pub(crate) const IN_SESSION: &str = "IN_SESSION";
pub(crate) const ABOUT_FILE: &str = "ABOUT_FILE";
pub(crate) const ABOUT_PANE: &str = "ABOUT_PANE";
pub(crate) const LINKS_TO: &str = "LINKS_TO";

/// A row a collector kept: what the daemon's collectors write when a
/// database provider keeps memory, so rows can be found next to the
/// memories, panes, and sessions they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorRow {
    pub collector: String,
    pub at_unix_ms: u64,
    /// The pane, as tmux names it (`%3`).
    pub pane: String,
    pub session: Option<String>,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    pub fields: Vec<(String, String)>,
    pub context: Vec<String>,
}

/// What an agent session came to: one per session, replaced on each write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub session: String,
    pub text: String,
    pub updated_unix_ms: u64,
}

/// Vectors by memory key.
pub(crate) type Vectors = HashMap<String, Vec<f32>>;

/// The database key of a memory: scope, owner, and name, which no part
/// can make ambiguous (none of them holds a NUL).
pub(crate) fn key(scope: Scope, owner: &str, name: &str) -> String {
    format!("{}\0{owner}\0{name}", scope.name())
}

/// The owner a scope's memories are kept under for `cx`: the project root
/// or the session id; nothing for user and host. The project root is the
/// real path (symbolic links resolved), so one project is one owner
/// however a caller names it: `/tmp/app` and `/private/tmp/app` on macOS,
/// or a shell's path through a link and the process's own working
/// directory. A root that cannot be resolved is kept as given.
pub(crate) fn owner(cx: &Context, scope: Scope) -> Result<String, Error> {
    Ok(match scope {
        Scope::User | Scope::Host => String::new(),
        Scope::Project => {
            let root = crate::project_of(cx)?;
            std::fs::canonicalize(&root)
                .unwrap_or(root)
                .display()
                .to_string()
        }
        Scope::Session => crate::session_of(cx)?.to_owned(),
    })
}

/// The scopes a call looks in: the one given, or every one `cx` reaches.
pub(crate) fn scopes(cx: &Context, scope: Option<Scope>) -> Vec<Scope> {
    scope.map_or_else(|| crate::reach(cx), |s| vec![s])
}

/// Newest first, then by id: the order `list` gives.
pub(crate) fn newest_first(items: &mut [(String, Memory)]) {
    items.sort_by(|(_, a), (_, b)| {
        b.updated_unix_ms
            .cmp(&a.updated_unix_ms)
            .then(a.id.cmp(&b.id))
    });
}

/// The edges a memory should have now: (relation, target). A link that
/// is another memory's id is an edge to it when `exists` finds its key.
pub(crate) fn wanted(
    cx: &Context,
    scope: Scope,
    owner: &str,
    links: &[String],
    mut exists: impl FnMut(&str) -> Result<bool, Error>,
) -> Result<BTreeSet<(&'static str, String)>, Error> {
    let mut out = BTreeSet::new();
    match scope {
        Scope::Project => {
            out.insert((IN_PROJECT, owner.to_owned()));
        }
        Scope::Session => {
            out.insert((IN_SESSION, owner.to_owned()));
        }
        Scope::User | Scope::Host => {}
    }
    for l in links {
        let l = l.trim();
        if l.is_empty() || l.contains("://") {
            continue;
        }
        // Another memory's id, when it names one that exists.
        if let Ok((s, name)) = split_id(l)
            && let Ok(o) = self::owner(cx, s)
        {
            let target = key(s, &o, name);
            if exists(&target)? {
                out.insert((LINKS_TO, target));
                continue;
            }
        }
        if let Some(pane) = l.strip_prefix("pane:") {
            out.insert((ABOUT_PANE, pane.trim().to_owned()));
            continue;
        }
        if l.starts_with('%') && l.len() > 1 && l[1..].bytes().all(|b| b.is_ascii_digit()) {
            out.insert((ABOUT_PANE, l.to_owned()));
            continue;
        }
        // A path; a relative one is the project's, for a project memory.
        let p = Path::new(l);
        let path = if p.is_relative() && scope == Scope::Project {
            Path::new(owner).join(p).display().to_string()
        } else {
            l.to_owned()
        };
        out.insert((ABOUT_FILE, path));
    }
    Ok(out)
}

/// A new memory as `remember` keeps it, under the first name from its
/// title that `taken` says is free (`-2`, `-3`, ... after the first).
/// Returns its key and owner too.
pub(crate) fn fresh(
    cx: &Context,
    new: New,
    mut taken: impl FnMut(&str) -> Result<bool, Error>,
) -> Result<(String, String, Memory), Error> {
    if new.text.trim().is_empty() {
        return Err(Error::Invalid("a memory needs text".into()));
    }
    let owner = owner(cx, new.scope)?;
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
    let mut name = base.clone();
    for n in 2.. {
        if !taken(&key(new.scope, &owner, &name))? {
            break;
        }
        name = format!("{base}-{n}");
    }
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
        path: None,
        score: None,
    };
    Ok((key(new.scope, &owner, &name), owner, m))
}

/// The memory with `change` applied, as `update` keeps it.
pub(crate) fn changed(mut m: Memory, change: Change) -> Result<Memory, Error> {
    if let Some(t) = change.text {
        if t.trim().is_empty() {
            return Err(Error::Invalid("a memory needs text".into()));
        }
        m.text = t.trim_end().to_owned();
    }
    if let Some(kind) = change.kind {
        m.kind = kind;
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
    Ok(m)
}

/// `m` as `put` keeps it, checked, with its key and owner.
pub(crate) fn placed(cx: &Context, m: &Memory) -> Result<(String, String, Memory), Error> {
    if m.text.trim().is_empty() {
        return Err(Error::Invalid("a memory needs text".into()));
    }
    let (scope, name) = split_id(&m.id)?;
    if scope != m.scope {
        return Err(Error::Invalid(format!(
            "`{}` is not in the {} scope",
            m.id,
            m.scope.name()
        )));
    }
    let owner = owner(cx, scope)?;
    let mut m = m.clone();
    m.text = m.text.trim_end().to_owned();
    m.path = None;
    m.score = None;
    Ok((key(scope, &owner, name), owner, m))
}

/// A relation's target as a person reads it: a memory's id for LINKS_TO.
pub(crate) fn shown(rel: &str, target: &str) -> String {
    if rel == LINKS_TO {
        let mut parts = target.split('\0');
        let scope = parts.next().unwrap_or_default();
        let name = parts.nth(1).unwrap_or_default();
        format!("{scope}/{name}")
    } else {
        target.to_owned()
    }
}

/// What a memory's vector is made from.
pub(crate) fn vector_text(m: &Memory) -> String {
    format!("{}\n{}\n{}", m.title, m.description, m.text)
}

/// Whether an edge open from `from` and closed at `to` (microseconds;
/// `None` while open) held at `at`.
pub(crate) fn held(from: i64, to: Option<i64>, at: i64) -> bool {
    from <= at && to.is_none_or(|t| t > at)
}

/// Microseconds, for valid times.
pub(crate) fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// A time in milliseconds as valid-time microseconds.
pub(crate) fn ms_to_us(unix_ms: u64) -> i64 {
    i64::try_from(unix_ms)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000)
}

/// Where recall gets what goes past the words: a provider's store.
pub(crate) trait Recall {
    /// Keys of memories one current edge from any of `keys`, with how
    /// many such edges each has.
    fn neighbours(&self, keys: &[String]) -> Result<BTreeMap<String, u32>, Error>;

    /// The query's vector and the vectors of `items` from the embedder's
    /// model, embedding (and keeping) the ones that have none yet.
    fn vectors(
        &self,
        e: &dyn Embedder,
        query: &str,
        items: &[(String, Memory)],
    ) -> Result<(Vec<f32>, Vectors), Error>;

    /// The database, or the server: what a problem names.
    fn place(&self) -> PathBuf;
}

/// Recall over `all` (every memory the call reaches, newest first, with
/// keys): word matches ranked exactly as [`search::rank`] ranks them, then
/// memories one edge from the best of them, then, with an embedder,
/// memories near the query in meaning.
pub(crate) fn recall(
    store: &dyn Recall,
    embedder: Option<&dyn Embedder>,
    all: Vec<(String, Memory)>,
    query: &str,
    limit: Option<usize>,
) -> Result<Found, Error> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut terms = search::words(query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        let items = all.into_iter().take(limit).map(|(_, m)| m).collect();
        return Ok(Found {
            items,
            problems: Vec::new(),
        });
    }
    let mut problems = Vec::new();
    // Words first, the same ranking every provider gives.
    let mut picked: BTreeMap<String, Memory> = BTreeMap::new();
    for (k, m) in &all {
        let score = search::score(&terms, m);
        if score > 0 {
            let mut m = m.clone();
            m.score = Some(score);
            picked.insert(k.clone(), m);
        }
    }
    // Then memories one edge from the best matches.
    let mut seeds: Vec<(&String, &Memory)> = picked.iter().collect();
    seeds.sort_by_key(|a| std::cmp::Reverse(a.1.score));
    let seeds: Vec<String> = seeds
        .into_iter()
        .take(limit)
        .map(|(k, _)| k.clone())
        .collect();
    let near = if seeds.is_empty() {
        BTreeMap::new()
    } else {
        store.neighbours(&seeds)?
    };
    for (k, m) in &all {
        if picked.contains_key(k) {
            continue;
        }
        if let Some(n) = near.get(k) {
            let mut m = m.clone();
            m.score = Some(RELATED_BASE + 10 * (*n).min(49));
            picked.insert(k.clone(), m);
        }
    }
    // Then, with an embedder, memories near the query in meaning.
    if let Some(e) = embedder {
        let rest: Vec<(String, Memory)> = all
            .iter()
            .filter(|(k, _)| !picked.contains_key(k))
            .cloned()
            .collect();
        if !rest.is_empty() {
            // The words and the graph still answer when the embedder
            // fails; the reply says why there are no vectors.
            match store.vectors(e, query, &rest) {
                Ok((q, have)) => {
                    for (k, m) in rest {
                        let sim = have.get(&k).map_or(0.0, |v| cosine(&q, v));
                        if sim >= NEAR {
                            let mut m = m;
                            m.score = Some(((sim * 400.0) as u32).clamp(1, RELATED_BASE - 1));
                            picked.insert(k, m);
                        }
                    }
                }
                Err(err) => problems.push(Problem {
                    path: store.place(),
                    message: format!("recalled without vectors: embedder {}: {err}", e.model()),
                }),
            }
        }
    }
    let mut items: Vec<Memory> = picked.into_values().collect();
    items.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.updated_unix_ms.cmp(&a.updated_unix_ms))
            .then(a.id.cmp(&b.id))
    });
    items.truncate(limit);
    Ok(Found { items, problems })
}
