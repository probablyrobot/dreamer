//! The `kosmonaut` provider: Kosmonaut, a graph database, embedded in the
//! daemon. One database per user holds every scope; each memory is a node
//! tagged with its scope and owner (the project root or the session id),
//! so a query only ever sees the scopes its context reaches.
//!
//! The graph:
//!
//! ```text
//! (Memory)-[IN_PROJECT]->(Project {path})     project memories
//! (Memory)-[IN_SESSION]->(Session {id})       session memories
//! (Memory)-[ABOUT_FILE]->(File {path})        links that are paths
//! (Memory)-[ABOUT_PANE]->(Pane {id})          links like `pane:%3`
//! (Memory)-[LINKS_TO]->(Memory)               links that are memory ids
//! (CollectorRow)-[ROW_FROM_PANE|ROW_IN_SESSION|ROW_IN_PROJECT]->(...)
//! (Summary)-[SUMMARIZES]->(Session)
//! ```
//!
//! Every edge from a memory has a valid time (`valid_from`, `valid_to`):
//! when a link is dropped its edge is closed, not deleted, so the graph
//! keeps what was true until when. Recall ranks by words exactly as
//! [`crate::search`] does for every provider, then adds memories one edge
//! away from the matches, then, with an embedder, memories whose vectors
//! are near the query's.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use kosmonaut::{Database, DatabaseConfig, QueryResult, Value};

use crate::embed::{Embedder, cosine};
use crate::files::{one_line, slug, title_of};
use crate::{
    Change, Context, DEFAULT_LIMIT, Error, Found, Kind, MAX_LIMIT, Memory, New, Problem, Provider,
    Scope, search, split_id, time,
};

/// Bumped when the tables change shape.
const SCHEMA: &str = "1";

const TABLES: &[&str] = &[
    "CREATE NODE TABLE Meta(name STRING, value STRING, PRIMARY KEY(name))",
    "CREATE NODE TABLE Memory(key STRING, scope STRING, owner STRING, name STRING, kind STRING, \
     title STRING, description STRING, body STRING, links STRING, created INT64, updated INT64, \
     PRIMARY KEY(key))",
    "CREATE NODE TABLE Project(path STRING, PRIMARY KEY(path))",
    "CREATE NODE TABLE Session(id STRING, PRIMARY KEY(id))",
    "CREATE NODE TABLE File(path STRING, PRIMARY KEY(path))",
    "CREATE NODE TABLE Pane(id STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IN_PROJECT(FROM Memory TO Project, valid_from TIMESTAMP, valid_to TIMESTAMP) \
     WITH VALID TIME (valid_from, valid_to)",
    "CREATE REL TABLE IN_SESSION(FROM Memory TO Session, valid_from TIMESTAMP, valid_to TIMESTAMP) \
     WITH VALID TIME (valid_from, valid_to)",
    "CREATE REL TABLE ABOUT_FILE(FROM Memory TO File, valid_from TIMESTAMP, valid_to TIMESTAMP) \
     WITH VALID TIME (valid_from, valid_to)",
    "CREATE REL TABLE ABOUT_PANE(FROM Memory TO Pane, valid_from TIMESTAMP, valid_to TIMESTAMP) \
     WITH VALID TIME (valid_from, valid_to)",
    "CREATE REL TABLE LINKS_TO(FROM Memory TO Memory, valid_from TIMESTAMP, valid_to TIMESTAMP) \
     WITH VALID TIME (valid_from, valid_to)",
    "CREATE NODE TABLE CollectorRow(key STRING, collector STRING, at INT64, pane STRING, \
     session STRING, cwd STRING, command STRING, fields STRING, context STRING, PRIMARY KEY(key))",
    "CREATE REL TABLE ROW_FROM_PANE(FROM CollectorRow TO Pane)",
    "CREATE REL TABLE ROW_IN_SESSION(FROM CollectorRow TO Session)",
    "CREATE REL TABLE ROW_IN_PROJECT(FROM CollectorRow TO Project)",
    "CREATE NODE TABLE Summary(session STRING, text STRING, updated INT64, PRIMARY KEY(session))",
    "CREATE REL TABLE SUMMARIZES(FROM Summary TO Session)",
];

/// The node table each relation from a memory points at, and its key.
const RELATIONS: [(&str, &str, &str); 5] = [
    ("IN_PROJECT", "Project", "path"),
    ("IN_SESSION", "Session", "id"),
    ("ABOUT_FILE", "File", "path"),
    ("ABOUT_PANE", "Pane", "id"),
    ("LINKS_TO", "Memory", "key"),
];

/// Memory the database may use for cached pages. A memory store is small.
const BUFFER_POOL: usize = 16 * 1024 * 1024;

/// Recall: a memory one edge from a match scores this, plus 10 for each
/// further edge, below any word match (every word match scores 1000 or
/// more; see [`crate::search`]).
const RELATED_BASE: u32 = 500;
/// Recall: a memory whose vector is at least this close to the query's
/// (cosine) is included; its score is the similarity times 400.
const NEAR: f32 = 0.5;

/// A row a collector kept: what the daemon's collectors write when this is
/// the memory provider, so rows can be found next to the memories, panes,
/// and sessions they came from.
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

pub struct Kosmonaut {
    path: PathBuf,
    embedder: Option<Arc<dyn Embedder>>,
    /// Opened on first use and kept; tried again on the next call if
    /// opening failed (another process holding the database, say).
    db: Mutex<Option<Arc<Database>>>,
    /// Writes in this process one at a time, so two never pick the same
    /// name.
    write: Mutex<()>,
    rows: AtomicU64,
}

impl std::fmt::Debug for Kosmonaut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kosmonaut")
            .field("path", &self.path)
            .field("embedder", &self.embedder.as_ref().map(|e| e.model()))
            .finish_non_exhaustive()
    }
}

type Params = HashMap<String, Value>;

/// Vectors by memory key.
type Vectors = HashMap<String, Vec<f32>>;

fn params<const N: usize>(pairs: [(&str, Value); N]) -> Params {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn int(v: u64) -> Value {
    Value::Int64(i64::try_from(v).unwrap_or(i64::MAX))
}

fn strings(v: &[String]) -> Value {
    Value::List(
        v.iter().map(|x| s(x)).collect(),
        kosmonaut::LogicalType::String,
    )
}

fn db_error(e: impl std::fmt::Display) -> Error {
    Error::Io(format!("kosmonaut: {e}"))
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(t)) => t.clone(),
        _ => String::new(),
    }
}

fn number_of(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Int64(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

fn floats_of(v: Option<&Value>) -> Option<Vec<f32>> {
    match v {
        Some(Value::List(items, _) | Value::Array(items, _, _)) => items
            .iter()
            .map(|x| match x {
                Value::Float(f) => Some(*f),
                Value::Double(d) => Some(*d as f32),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Microseconds, for valid-time columns.
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// The database key of a memory: scope, owner, and name, which no part
/// can make ambiguous (none of them holds a NUL).
fn key(scope: Scope, owner: &str, name: &str) -> String {
    format!("{}\0{owner}\0{name}", scope.name())
}

/// Columns every memory read returns, in this order.
const COLUMNS: &str = "m.scope, m.name, m.kind, m.title, m.description, m.body, m.links, \
                       m.created, m.updated, m.key";

fn memory(row: &kosmonaut::Row) -> Option<(String, Memory)> {
    let v = |i| row.get_by_index(i);
    let scope = Scope::from_name(&text_of(v(0)))?;
    let name = text_of(v(1));
    let kind = Kind::from_name(&text_of(v(2)))?;
    let links: Vec<String> = serde_json::from_str(&text_of(v(6))).unwrap_or_default();
    Some((
        text_of(v(9)),
        Memory {
            id: format!("{}/{name}", scope.name()),
            scope,
            kind,
            title: text_of(v(3)),
            description: text_of(v(4)),
            text: text_of(v(5)),
            links,
            created_unix_ms: number_of(v(7)),
            updated_unix_ms: number_of(v(8)),
            path: None,
            score: None,
        },
    ))
}

impl Kosmonaut {
    /// The provider over the database at `path` (a directory, made on first
    /// use). With `embedder`, memories also get vectors and recall also
    /// looks for memories near the query in meaning.
    pub fn new(path: PathBuf, embedder: Option<Arc<dyn Embedder>>) -> Kosmonaut {
        Kosmonaut {
            path,
            embedder,
            db: Mutex::new(None),
            write: Mutex::new(()),
            rows: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.write.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn db(&self) -> Result<Arc<Database>, Error> {
        let mut slot = self.db.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = slot.as_ref() {
            return Ok(db.clone());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Io(format!("{}: {e}", parent.display())))?;
        }
        let config = DatabaseConfig {
            buffer_pool_size_bytes: BUFFER_POOL,
            wal_preallocate: Some(false),
            ..DatabaseConfig::default()
        };
        let db = Database::open_with_config(&self.path, config)
            .map_err(|e| Error::Io(format!("{}: {e}", self.path.display())))?;
        schema(&db)?;
        let db = Arc::new(db);
        *slot = Some(db.clone());
        Ok(db)
    }

    fn read(&self, cypher: &str, p: &Params) -> Result<QueryResult, Error> {
        let db = self.db()?;
        let mut conn = db.connect();
        let mut tx = conn.begin_read_only().map_err(db_error)?;
        tx.run_with_params(cypher, p).map_err(db_error)
    }

    /// Run `f` in one write transaction: all of it commits, or none.
    fn write<T>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<T, Error>) -> Result<T, Error> {
        let db = self.db()?;
        let mut conn = db.connect();
        let tx = conn.begin().map_err(db_error)?;
        let mut w = Tx { tx };
        let out = f(&mut w)?;
        w.tx.commit().map_err(db_error)?;
        Ok(out)
    }

    /// The owner a scope's memories are kept under for `cx`.
    fn owner(cx: &Context, scope: Scope) -> Result<String, Error> {
        Ok(match scope {
            Scope::User | Scope::Host => String::new(),
            Scope::Project => crate::project_of(cx)?.display().to_string(),
            Scope::Session => crate::session_of(cx)?.to_owned(),
        })
    }

    fn scopes(cx: &Context, scope: Option<Scope>) -> Vec<Scope> {
        scope.map_or_else(|| crate::reach(cx), |s| vec![s])
    }

    /// Memories in `scopes` for `cx`, newest first, with their keys.
    fn load(&self, cx: &Context, scopes: &[Scope]) -> Result<Vec<(String, Memory)>, Error> {
        let mut out = Vec::new();
        for &scope in scopes {
            let owner = Kosmonaut::owner(cx, scope)?;
            let r = self.read(
                &format!(
                    "MATCH (m:Memory) WHERE m.scope = $scope AND m.owner = $owner RETURN {COLUMNS}"
                ),
                &params([("scope", s(scope.name())), ("owner", s(&owner))]),
            )?;
            out.extend(r.rows().iter().filter_map(memory));
        }
        out.sort_by(|(_, a), (_, b)| {
            b.updated_unix_ms
                .cmp(&a.updated_unix_ms)
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    fn find(&self, k: &str) -> Result<Option<Memory>, Error> {
        let r = self.read(
            &format!("MATCH (m:Memory) WHERE m.key = $key RETURN {COLUMNS}"),
            &params([("key", s(k))]),
        )?;
        Ok(r.rows().first().and_then(memory).map(|(_, m)| m))
    }

    fn existing(&self, cx: &Context, id: &str) -> Result<(String, Memory), Error> {
        let (scope, name) = split_id(id)?;
        let k = key(scope, &Kosmonaut::owner(cx, scope)?, name);
        let m = self
            .find(&k)?
            .ok_or_else(|| Error::NotFound(id.to_owned()))?;
        Ok((k, m))
    }

    /// The edges a memory should have now: (relation, target key).
    fn wanted(
        &self,
        cx: &Context,
        scope: Scope,
        owner: &str,
        links: &[String],
    ) -> Result<BTreeSet<(&'static str, String)>, Error> {
        let mut out = BTreeSet::new();
        match scope {
            Scope::Project => {
                out.insert(("IN_PROJECT", owner.to_owned()));
            }
            Scope::Session => {
                out.insert(("IN_SESSION", owner.to_owned()));
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
                && let Ok(o) = Kosmonaut::owner(cx, s)
            {
                let target = key(s, &o, name);
                if self.find(&target)?.is_some() {
                    out.insert(("LINKS_TO", target));
                    continue;
                }
            }
            if let Some(pane) = l.strip_prefix("pane:") {
                out.insert(("ABOUT_PANE", pane.trim().to_owned()));
                continue;
            }
            if l.starts_with('%') && l.len() > 1 && l[1..].bytes().all(|b| b.is_ascii_digit()) {
                out.insert(("ABOUT_PANE", l.to_owned()));
                continue;
            }
            // A path; a relative one is the project's, for a project memory.
            let p = Path::new(l);
            let path = if p.is_relative() && scope == Scope::Project {
                Path::new(owner).join(p).display().to_string()
            } else {
                l.to_owned()
            };
            out.insert(("ABOUT_FILE", path));
        }
        Ok(out)
    }

    /// Store `m` under key `k` for `cx`, creating or replacing it, and
    /// bring its edges up to date.
    fn store(&self, cx: &Context, k: &str, owner: &str, m: &Memory) -> Result<(), Error> {
        let (_, name) = split_id(&m.id)?;
        let wanted = self.wanted(cx, m.scope, owner, &m.links)?;
        let links = serde_json::to_string(&m.links).map_err(|e| Error::Io(e.to_string()))?;
        self.write(|w| {
            let fields = params([
                ("key", s(k)),
                ("scope", s(m.scope.name())),
                ("owner", s(owner)),
                ("name", s(name)),
                ("kind", s(m.kind.name())),
                ("title", s(&m.title)),
                ("description", s(&m.description)),
                ("body", s(&m.text)),
                ("links", s(&links)),
                ("created", int(m.created_unix_ms)),
                ("updated", int(m.updated_unix_ms)),
            ]);
            let found = w.run(
                "MATCH (m:Memory) WHERE m.key = $key RETURN m.key",
                &params([("key", s(k))]),
            )?;
            if found.rows().is_empty() {
                w.run(
                    "CREATE (:Memory {key: $key, scope: $scope, owner: $owner, name: $name, \
                     kind: $kind, title: $title, description: $description, body: $body, \
                     links: $links, created: $created, updated: $updated})",
                    &fields,
                )?;
            } else {
                w.run(
                    "MATCH (m:Memory) WHERE m.key = $key SET m.kind = $kind, m.title = $title, \
                     m.description = $description, m.body = $body, m.links = $links, \
                     m.created = $created, m.updated = $updated",
                    &fields,
                )?;
            }
            w.relate(k, &wanted)
        })?;
        self.embed_one(k, m);
        Ok(())
    }

    /// Give `m` a vector now, if there is an embedder. A failure is left
    /// for recall, which embeds what has no vector yet.
    fn embed_one(&self, k: &str, m: &Memory) {
        let Some(e) = self.embedder.as_ref() else {
            return;
        };
        if let Ok(mut v) = e.embed(&[&vector_text(m)])
            && let Some(v) = v.pop()
        {
            let _ = self.put_vectors(e.as_ref(), &[(k.to_owned(), v)]);
        }
    }

    fn vector_table(e: &dyn Embedder) -> String {
        format!("Vector{}", e.dimension())
    }

    fn put_vectors(&self, e: &dyn Embedder, vs: &[(String, Vec<f32>)]) -> Result<(), Error> {
        if vs.is_empty() {
            return Ok(());
        }
        let table = Kosmonaut::vector_table(e);
        let db = self.db()?;
        {
            let conn = db.connect();
            let exists = conn
                .list_tables()
                .map_err(db_error)?
                .iter()
                .any(|(name, _)| *name == table);
            if !exists {
                let mut conn = db.connect();
                conn.execute(&format!(
                    "CREATE NODE TABLE {table}(key STRING, model STRING, v FLOAT[{}], \
                     PRIMARY KEY(key))",
                    e.dimension()
                ))
                .map_err(db_error)?;
            }
        }
        self.write(|w| {
            for (k, v) in vs {
                let p = params([
                    ("key", s(k)),
                    ("model", s(e.model())),
                    (
                        "v",
                        Value::List(
                            v.iter().map(|x| Value::Float(*x)).collect(),
                            kosmonaut::LogicalType::Float,
                        ),
                    ),
                ]);
                w.run(
                    &format!("MATCH (x:{table}) WHERE x.key = $key DELETE x"),
                    &p,
                )?;
                w.run(
                    &format!("CREATE (:{table} {{key: $key, model: $model, v: $v}})"),
                    &p,
                )?;
            }
            Ok(())
        })
    }

    fn drop_vector(&self, k: &str) -> Result<(), Error> {
        let Some(e) = self.embedder.as_ref() else {
            return Ok(());
        };
        let table = Kosmonaut::vector_table(e.as_ref());
        let db = self.db()?;
        let exists = db
            .connect()
            .list_tables()
            .map_err(db_error)?
            .iter()
            .any(|(name, _)| *name == table);
        if !exists {
            return Ok(());
        }
        self.write(|w| {
            w.run(
                &format!("MATCH (x:{table}) WHERE x.key = $key DELETE x"),
                &params([("key", s(k))]),
            )
            .map(|_| ())
        })
    }

    /// The query's vector and the vectors of `items` from the embedder's
    /// model, embedding the ones that have none yet.
    fn vectors(
        &self,
        e: &dyn Embedder,
        query: &str,
        items: &[(String, Memory)],
    ) -> Result<(Vec<f32>, Vectors), Error> {
        let q = e.embed(&[query])?.pop().unwrap_or_default();
        let table = Kosmonaut::vector_table(e);
        let keys: Vec<String> = items.iter().map(|(k, _)| k.clone()).collect();
        let mut have = HashMap::new();
        let exists = self
            .db()?
            .connect()
            .list_tables()
            .map_err(db_error)?
            .iter()
            .any(|(name, _)| *name == table);
        if exists {
            let r = self.read(
                &format!(
                    "MATCH (x:{table}) WHERE x.key IN $keys AND x.model = $model \
                     RETURN x.key, x.v"
                ),
                &params([("keys", strings(&keys)), ("model", s(e.model()))]),
            )?;
            for row in r.rows() {
                if let Some(v) = floats_of(row.get_by_index(1)) {
                    have.insert(text_of(row.get_by_index(0)), v);
                }
            }
        }
        let missing: Vec<&(String, Memory)> = items
            .iter()
            .filter(|(k, _)| !have.contains_key(k))
            .collect();
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|(_, m)| vector_text(m)).collect();
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let got = e.embed(&refs)?;
            let new: Vec<(String, Vec<f32>)> =
                missing.iter().map(|(k, _)| k.clone()).zip(got).collect();
            self.put_vectors(e, &new)?;
            have.extend(new);
        }
        Ok((q, have))
    }

    /// Keys of memories one current edge from any of `keys`, with how
    /// many such edges each has.
    fn neighbours(&self, keys: &[String]) -> Result<BTreeMap<String, u32>, Error> {
        let mut out: BTreeMap<String, u32> = BTreeMap::new();
        if keys.is_empty() {
            return Ok(out);
        }
        let p = params([("keys", strings(keys))]);
        let queries = [
            "MATCH (a:Memory)-[r:LINKS_TO]->(b:Memory) WHERE a.key IN $keys AND r.valid_to IS NULL \
             RETURN b.key",
            "MATCH (b:Memory)-[r:LINKS_TO]->(a:Memory) WHERE a.key IN $keys AND r.valid_to IS NULL \
             RETURN b.key",
            "MATCH (a:Memory)-[r:ABOUT_FILE]->(f:File)<-[q:ABOUT_FILE]-(b:Memory) \
             WHERE a.key IN $keys AND r.valid_to IS NULL AND q.valid_to IS NULL RETURN b.key",
            "MATCH (a:Memory)-[r:ABOUT_PANE]->(f:Pane)<-[q:ABOUT_PANE]-(b:Memory) \
             WHERE a.key IN $keys AND r.valid_to IS NULL AND q.valid_to IS NULL RETURN b.key",
        ];
        for q in queries {
            for row in self.read(q, &p)?.rows() {
                *out.entry(text_of(row.get_by_index(0))).or_default() += 1;
            }
        }
        Ok(out)
    }

    /// Keep a collector's row, linked to its pane, session, and project.
    pub fn add_row(&self, row: &CollectorRow) -> Result<(), Error> {
        let n = self.rows.fetch_add(1, Ordering::Relaxed);
        let k = format!(
            "{}\0{}\0{}\0{n}",
            row.collector,
            row.at_unix_ms,
            time::now_ms()
        );
        let fields = serde_json::to_string(&row.fields).map_err(|e| Error::Io(e.to_string()))?;
        let context = serde_json::to_string(&row.context).map_err(|e| Error::Io(e.to_string()))?;
        let cwd = row.cwd.as_ref().map(|c| c.display().to_string());
        let project = row
            .cwd
            .as_deref()
            .filter(|c| c.is_absolute())
            .map(|c| crate::project_root(c).display().to_string());
        let _guard = self.lock();
        self.write(|w| {
            let p = params([
                ("key", s(&k)),
                ("collector", s(&row.collector)),
                ("at", int(row.at_unix_ms)),
                ("pane", s(&row.pane)),
                ("session", s(row.session.as_deref().unwrap_or(""))),
                ("cwd", s(cwd.as_deref().unwrap_or(""))),
                ("command", s(row.command.as_deref().unwrap_or(""))),
                ("fields", s(&fields)),
                ("context", s(&context)),
                ("project", s(project.as_deref().unwrap_or(""))),
            ]);
            w.run(
                "CREATE (:CollectorRow {key: $key, collector: $collector, at: $at, pane: $pane, \
                 session: $session, cwd: $cwd, command: $command, fields: $fields, \
                 context: $context})",
                &p,
            )?;
            w.node("Pane", "id", &row.pane)?;
            w.run(
                "MATCH (r:CollectorRow), (t:Pane) WHERE r.key = $key AND t.id = $pane \
                 CREATE (r)-[:ROW_FROM_PANE]->(t)",
                &p,
            )?;
            if let Some(session) = row.session.as_deref() {
                w.node("Session", "id", session)?;
                w.run(
                    "MATCH (r:CollectorRow), (t:Session) WHERE r.key = $key AND t.id = $session \
                     CREATE (r)-[:ROW_IN_SESSION]->(t)",
                    &p,
                )?;
            }
            if let Some(project) = project.as_deref() {
                w.node("Project", "path", project)?;
                w.run(
                    "MATCH (r:CollectorRow), (t:Project) WHERE r.key = $key AND t.path = $project \
                     CREATE (r)-[:ROW_IN_PROJECT]->(t)",
                    &p,
                )?;
            }
            Ok(())
        })
    }

    /// A collector's rows at or after `since_unix_ms`, oldest first, at
    /// most `limit`.
    pub fn rows(
        &self,
        collector: &str,
        since_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<CollectorRow>, Error> {
        let r = self.read(
            "MATCH (r:CollectorRow) WHERE r.collector = $collector AND r.at >= $since \
             RETURN r.collector, r.at, r.pane, r.session, r.cwd, r.command, r.fields, r.context, \
             r.key",
            &params([("collector", s(collector)), ("since", int(since_unix_ms))]),
        )?;
        let opt = |v: Option<&Value>| Some(text_of(v)).filter(|t| !t.is_empty());
        let mut rows: Vec<(String, CollectorRow)> = r
            .rows()
            .iter()
            .map(|row| {
                let v = |i| row.get_by_index(i);
                (
                    text_of(v(8)),
                    CollectorRow {
                        collector: text_of(v(0)),
                        at_unix_ms: number_of(v(1)),
                        pane: text_of(v(2)),
                        session: opt(v(3)),
                        cwd: opt(v(4)).map(PathBuf::from),
                        command: opt(v(5)),
                        fields: serde_json::from_str(&text_of(v(6))).unwrap_or_default(),
                        context: serde_json::from_str(&text_of(v(7))).unwrap_or_default(),
                    },
                )
            })
            .collect();
        rows.sort_by(|(ka, a), (kb, b)| a.at_unix_ms.cmp(&b.at_unix_ms).then(ka.cmp(kb)));
        Ok(rows.into_iter().take(limit).map(|(_, r)| r).collect())
    }

    /// Keep `text` as the summary of `session`, replacing the one there.
    pub fn set_summary(&self, session: &str, text: &str) -> Result<Summary, Error> {
        let cx = Context {
            cwd: None,
            session: Some(session.to_owned()),
        };
        let session = crate::session_of(&cx)?;
        let now = time::now_ms();
        let _guard = self.lock();
        self.write(|w| {
            let p = params([("session", s(session)), ("text", s(text)), ("at", int(now))]);
            w.run(
                "MATCH (x:Summary) WHERE x.session = $session DETACH DELETE x",
                &p,
            )?;
            w.run(
                "CREATE (:Summary {session: $session, text: $text, updated: $at})",
                &p,
            )?;
            w.node("Session", "id", session)?;
            w.run(
                "MATCH (x:Summary), (t:Session) WHERE x.session = $session AND t.id = $session \
                 CREATE (x)-[:SUMMARIZES]->(t)",
                &p,
            )?;
            Ok(())
        })?;
        Ok(Summary {
            session: session.to_owned(),
            text: text.to_owned(),
            updated_unix_ms: now,
        })
    }

    pub fn summary(&self, session: &str) -> Result<Option<Summary>, Error> {
        let r = self.read(
            "MATCH (x:Summary) WHERE x.session = $session RETURN x.session, x.text, x.updated",
            &params([("session", s(session))]),
        )?;
        Ok(r.rows().first().map(|row| Summary {
            session: text_of(row.get_by_index(0)),
            text: text_of(row.get_by_index(1)),
            updated_unix_ms: number_of(row.get_by_index(2)),
        }))
    }

    /// The relations of a memory as they were at `at_unix_ms`: (relation,
    /// target), from the valid times of its edges, current ones included.
    pub fn relations_at(
        &self,
        cx: &Context,
        id: &str,
        at_unix_ms: u64,
    ) -> Result<Vec<(String, String)>, Error> {
        let (k, _) = self.existing(cx, id)?;
        let at = i64::try_from(at_unix_ms)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        let mut out = Vec::new();
        for (rel, table, field) in RELATIONS {
            let r = self.read(
                &format!(
                    "MATCH (m:Memory)-[r:{rel}]->(t:{table}) WHERE m.key = $key \
                     RETURN t.{field}, r.valid_from, r.valid_to"
                ),
                &params([("key", s(&k))]),
            )?;
            for row in r.rows() {
                let from = match row.get_by_index(1) {
                    Some(Value::Timestamp(t)) => *t,
                    _ => continue,
                };
                let to = match row.get_by_index(2) {
                    Some(Value::Timestamp(t)) => Some(*t),
                    _ => None,
                };
                if from <= at && to.is_none_or(|t| t > at) {
                    out.push((rel.to_owned(), shown(rel, &text_of(row.get_by_index(0)))));
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// A relation's target as a person reads it: a memory's id for LINKS_TO.
fn shown(rel: &str, target: &str) -> String {
    if rel == "LINKS_TO" {
        let mut parts = target.split('\0');
        let scope = parts.next().unwrap_or_default();
        let name = parts.nth(1).unwrap_or_default();
        format!("{scope}/{name}")
    } else {
        target.to_owned()
    }
}

/// What a memory's vector is made from.
fn vector_text(m: &Memory) -> String {
    format!("{}\n{}\n{}", m.title, m.description, m.text)
}

/// Create the tables that are missing.
fn schema(db: &Database) -> Result<(), Error> {
    let have: BTreeSet<String> = db
        .connect()
        .list_tables()
        .map_err(db_error)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut conn = db.connect();
    for ddl in TABLES {
        let name = ddl
            .split_whitespace()
            .nth(3)
            .and_then(|n| n.split('(').next())
            .unwrap_or_default();
        if !have.contains(name) {
            conn.execute(ddl).map_err(db_error)?;
        }
    }
    let mut tx = conn.begin().map_err(db_error)?;
    let p = params([("name", s("schema")), ("value", s(SCHEMA))]);
    let r = tx
        .run_with_params("MATCH (x:Meta) WHERE x.name = $name RETURN x.value", &p)
        .map_err(db_error)?;
    match r.rows().first().map(|row| text_of(row.get_by_index(0))) {
        None => {
            tx.run_with_params("CREATE (:Meta {name: $name, value: $value})", &p)
                .map_err(db_error)?;
        }
        Some(v) if v == SCHEMA => {}
        Some(v) => {
            return Err(Error::Io(format!(
                "kosmonaut: the memory database has schema {v}; this Dreamer reads {SCHEMA}"
            )));
        }
    }
    tx.commit().map_err(db_error)
}

/// A write transaction.
struct Tx<'t> {
    tx: kosmonaut::Transaction<'t>,
}

impl Tx<'_> {
    fn run(&mut self, cypher: &str, p: &Params) -> Result<QueryResult, Error> {
        self.tx.run_with_params(cypher, p).map_err(db_error)
    }

    /// Make sure a node exists.
    fn node(&mut self, table: &str, field: &str, value: &str) -> Result<(), Error> {
        let p = params([("v", s(value))]);
        let found = self.run(
            &format!("MATCH (t:{table}) WHERE t.{field} = $v RETURN t.{field}"),
            &p,
        )?;
        if found.rows().is_empty() {
            self.run(&format!("CREATE (:{table} {{{field}: $v}})"), &p)?;
        }
        Ok(())
    }

    /// Close the current edges of memory `k` that are not `wanted`, and
    /// open the wanted ones it does not have.
    fn relate(&mut self, k: &str, wanted: &BTreeSet<(&'static str, String)>) -> Result<(), Error> {
        let now = now_us();
        let mut current = BTreeSet::new();
        for (rel, table, field) in RELATIONS {
            let r = self.run(
                &format!(
                    "MATCH (m:Memory)-[r:{rel}]->(t:{table}) WHERE m.key = $key \
                     AND r.valid_to IS NULL RETURN t.{field}, r.valid_from"
                ),
                &params([("key", s(k))]),
            )?;
            for row in r.rows() {
                let target = text_of(row.get_by_index(0));
                let from = match row.get_by_index(1) {
                    Some(Value::Timestamp(t)) => *t,
                    _ => now,
                };
                current.insert((rel, target, from));
            }
        }
        for (rel, target, from) in &current {
            if wanted.contains(&(*rel, target.clone())) {
                continue;
            }
            let (_, table, field) = RELATIONS
                .iter()
                .find(|(r, _, _)| r == rel)
                .copied()
                .unwrap_or(RELATIONS[0]);
            // An interval must not be empty: an edge closed in the
            // microsecond it opened ends one later.
            let to = now.max(from + 1);
            self.run(
                &format!(
                    "MATCH (m:Memory)-[r:{rel}]->(t:{table}) WHERE m.key = $key \
                     AND t.{field} = $target AND r.valid_to IS NULL SET r.valid_to = $to"
                ),
                &params([
                    ("key", s(k)),
                    ("target", s(target)),
                    ("to", Value::Timestamp(to)),
                ]),
            )?;
        }
        for (rel, target) in wanted {
            if current.iter().any(|(r, t, _)| r == rel && t == target) {
                continue;
            }
            let (_, table, field) = RELATIONS
                .iter()
                .find(|(r, _, _)| r == rel)
                .copied()
                .unwrap_or(RELATIONS[0]);
            if table != "Memory" {
                self.node(table, field, target)?;
            }
            self.run(
                &format!(
                    "MATCH (m:Memory), (t:{table}) WHERE m.key = $key AND t.{field} = $target \
                     CREATE (m)-[:{rel} {{valid_from: $from}}]->(t)"
                ),
                &params([
                    ("key", s(k)),
                    ("target", s(target)),
                    ("from", Value::Timestamp(now)),
                ]),
            )?;
        }
        Ok(())
    }
}

impl Provider for Kosmonaut {
    fn name(&self) -> &'static str {
        "kosmonaut"
    }

    fn remember(&self, cx: &Context, new: New) -> Result<Memory, Error> {
        if new.text.trim().is_empty() {
            return Err(Error::Invalid("a memory needs text".into()));
        }
        let owner = Kosmonaut::owner(cx, new.scope)?;
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
        let _guard = self.lock();
        let base = slug(&title);
        let mut name = base.clone();
        for n in 2.. {
            if self.find(&key(new.scope, &owner, &name))?.is_none() {
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
        let k = key(new.scope, &owner, &name);
        self.store(cx, &k, &owner, &m)?;
        Ok(m)
    }

    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error> {
        let items = self
            .load(cx, &Kosmonaut::scopes(cx, scope))?
            .into_iter()
            .map(|(_, m)| m)
            .filter(|m| kind.is_none_or(|k| k == m.kind))
            .collect();
        Ok(Found {
            items,
            problems: Vec::new(),
        })
    }

    fn get(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        self.existing(cx, id).map(|(_, m)| m)
    }

    fn update(&self, cx: &Context, id: &str, change: Change) -> Result<Memory, Error> {
        let _guard = self.lock();
        let (k, mut m) = self.existing(cx, id)?;
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
        let owner = Kosmonaut::owner(cx, m.scope)?;
        self.store(cx, &k, &owner, &m)?;
        Ok(m)
    }

    fn forget(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        let _guard = self.lock();
        let (k, m) = self.existing(cx, id)?;
        self.write(|w| {
            w.run(
                "MATCH (m:Memory) WHERE m.key = $key DETACH DELETE m",
                &params([("key", s(&k))]),
            )
            .map(|_| ())
        })?;
        self.drop_vector(&k)?;
        Ok(m)
    }

    fn put(&self, cx: &Context, m: &Memory) -> Result<Memory, Error> {
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
        let owner = Kosmonaut::owner(cx, scope)?;
        let _guard = self.lock();
        let mut m = m.clone();
        m.text = m.text.trim_end().to_owned();
        m.path = None;
        m.score = None;
        self.store(cx, &key(scope, &owner, name), &owner, &m)?;
        Ok(m)
    }

    fn recall(
        &self,
        cx: &Context,
        query: &str,
        scope: Option<Scope>,
        limit: Option<usize>,
    ) -> Result<Found, Error> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let all = self.load(cx, &Kosmonaut::scopes(cx, scope))?;
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
        let near = self.neighbours(&seeds)?;
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
        if let Some(e) = self.embedder.as_ref() {
            let rest: Vec<(String, Memory)> = all
                .iter()
                .filter(|(k, _)| !picked.contains_key(k))
                .cloned()
                .collect();
            if !rest.is_empty() {
                // The words and the graph still answer when the embedder
                // fails; the reply says why there are no vectors.
                match self.vectors(e.as_ref(), query, &rest) {
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
                        path: self.path.clone(),
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
}
