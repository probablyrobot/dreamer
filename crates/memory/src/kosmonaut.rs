//! The `kosmonaut` provider: Kosmonaut, a graph database, reached as a
//! server over gRPC ([`server`]). One database per user holds every scope;
//! each memory is a node tagged with its scope and owner (the project root
//! or the session id), so a query only ever sees the scopes its context
//! reaches. Statements are Cypher, with parameters and result cells as
//! JSON, the form Kosmonaut's server takes and gives.
//!
//! What it keeps, and how recall ranks, is what the `sqlite` provider
//! keeps and how it ranks ([`crate::graph`]).
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::embed::Embedder;
use crate::graph::{self, CollectorRow, Recall, Summary, Vectors, key};
use crate::{Change, Context, Error, Found, Kind, Memory, New, Provider, Scope, split_id, time};

mod server;

pub use server::Server;

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

pub struct Kosmonaut {
    store: Server,
    embedder: Option<Arc<dyn Embedder>>,
    /// Whether the tables are known to be there: checked on first use, and
    /// again on the next call if that failed (another process holding the
    /// database, or a server that cannot be reached).
    ready: Mutex<bool>,
    /// Writes in this process one at a time, so two never pick the same
    /// name.
    write: Mutex<()>,
    rows: AtomicU64,
}

impl std::fmt::Debug for Kosmonaut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kosmonaut")
            .field("server", &self.store.url())
            .field("embedder", &self.embedder.as_ref().map(|e| e.model()))
            .finish_non_exhaustive()
    }
}

/// Statement parameters, as JSON values: a string binds a `STRING`, an
/// integer an `INT64`, a number with a fraction a `DOUBLE`, an array a
/// list. The mapping is Kosmonaut's server's; the embedded store applies
/// the same one.
type Params = BTreeMap<String, Value>;

/// A result: rows of cells, each as Kosmonaut's server renders it in JSON
/// (a `TIMESTAMP` is microseconds since the epoch, a vector an array).
type Rows = Vec<Vec<Value>>;

fn params<const N: usize>(pairs: [(&str, Value); N]) -> Params {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn int(v: u64) -> Value {
    Value::from(i64::try_from(v).unwrap_or(i64::MAX))
}

fn strings(v: &[String]) -> Value {
    Value::Array(v.iter().map(|x| s(x)).collect())
}

/// A valid time, for `timestamp($p)`: parameters have no timestamp type.
fn stamp(us: i64) -> Value {
    Value::String(time::format_us(us))
}

fn text_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn number_of(v: Option<&Value>) -> u64 {
    v.and_then(Value::as_u64).unwrap_or(0)
}

/// A `TIMESTAMP` cell: microseconds since the epoch.
fn micros_of(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

fn floats_of(v: Option<&Value>) -> Option<Vec<f32>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

/// Columns every memory read returns, in this order.
const COLUMNS: &str = "m.scope, m.name, m.kind, m.title, m.description, m.body, m.links, \
                       m.created, m.updated, m.key";

fn memory(row: &[Value]) -> Option<(String, Memory)> {
    let v = |i| row.get(i);
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

/// Runs statements in one write transaction.
trait Run {
    fn run(&mut self, cypher: &str, p: &Params) -> Result<Rows, Error>;
}

impl Kosmonaut {
    /// The provider over a Kosmonaut server. With `embedder`, memories
    /// also get vectors and recall also looks for memories near the query
    /// in meaning.
    pub fn server(server: Server, embedder: Option<Arc<dyn Embedder>>) -> Kosmonaut {
        Kosmonaut {
            store: server,
            embedder,
            ready: Mutex::new(false),
            write: Mutex::new(()),
            rows: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.write.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The server, with its tables made on first use.
    fn db(&self) -> Result<&Server, Error> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        if !*ready {
            schema(&self.store)?;
            *ready = true;
        }
        Ok(&self.store)
    }

    fn read(&self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        self.db()?.read(cypher, p)
    }

    /// Run `f` in one write transaction: all of it commits, or none.
    fn write<T>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<T, Error>) -> Result<T, Error> {
        self.db()?.write(|run| f(&mut Tx { run }))
    }

    /// Memories in `scopes` for `cx`, newest first, with their keys.
    fn load(&self, cx: &Context, scopes: &[Scope]) -> Result<Vec<(String, Memory)>, Error> {
        let mut out = Vec::new();
        for &scope in scopes {
            let owner = graph::owner(cx, scope)?;
            let r = self.read(
                &format!(
                    "MATCH (m:Memory) WHERE m.scope = $scope AND m.owner = $owner RETURN {COLUMNS}"
                ),
                &params([("scope", s(scope.name())), ("owner", s(&owner))]),
            )?;
            out.extend(r.iter().map(Vec::as_slice).filter_map(memory));
        }
        graph::newest_first(&mut out);
        Ok(out)
    }

    fn find(&self, k: &str) -> Result<Option<Memory>, Error> {
        let r = self.read(
            &format!("MATCH (m:Memory) WHERE m.key = $key RETURN {COLUMNS}"),
            &params([("key", s(k))]),
        )?;
        Ok(r.first().and_then(|row| memory(row)).map(|(_, m)| m))
    }

    fn existing(&self, cx: &Context, id: &str) -> Result<(String, Memory), Error> {
        let (scope, name) = split_id(id)?;
        let k = key(scope, &graph::owner(cx, scope)?, name);
        let m = self
            .find(&k)?
            .ok_or_else(|| Error::NotFound(id.to_owned()))?;
        Ok((k, m))
    }

    /// Store `m` under key `k` for `cx`, creating or replacing it, and
    /// bring its edges up to date.
    fn store(&self, cx: &Context, k: &str, owner: &str, m: &Memory) -> Result<(), Error> {
        let (_, name) = split_id(&m.id)?;
        let wanted = graph::wanted(cx, m.scope, owner, &m.links, |t| {
            Ok(self.find(t)?.is_some())
        })?;
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
            if found.is_empty() {
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
        if let Ok(mut v) = e.embed(&[&graph::vector_text(m)])
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
        if !db.tables()?.contains(&table) {
            db.execute(&format!(
                "CREATE NODE TABLE {table}(key STRING, model STRING, v FLOAT[{}], \
                 PRIMARY KEY(key))",
                e.dimension()
            ))?;
        }
        self.write(|w| {
            for (k, v) in vs {
                let p = params([
                    ("key", s(k)),
                    ("model", s(e.model())),
                    (
                        "v",
                        Value::Array(v.iter().map(|x| Value::from(f64::from(*x))).collect()),
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
        if !self.db()?.tables()?.contains(&table) {
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
            .iter()
            .map(|row| {
                let v = |i| row.get(i);
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
        Ok(r.first().map(|row| Summary {
            session: text_of(row.first()),
            text: text_of(row.get(1)),
            updated_unix_ms: number_of(row.get(2)),
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
        let at = graph::ms_to_us(at_unix_ms);
        let mut out = Vec::new();
        for (rel, table, field) in RELATIONS {
            let r = self.read(
                &format!(
                    "MATCH (m:Memory)-[r:{rel}]->(t:{table}) WHERE m.key = $key \
                     RETURN t.{field}, r.valid_from, r.valid_to"
                ),
                &params([("key", s(&k))]),
            )?;
            for row in &r {
                let Some(from) = micros_of(row.get(1)) else {
                    continue;
                };
                let to = micros_of(row.get(2));
                if graph::held(from, to, at) {
                    out.push((rel.to_owned(), graph::shown(rel, &text_of(row.first()))));
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Create the tables that are missing.
fn schema(db: &Server) -> Result<(), Error> {
    let have = db.tables()?;
    for ddl in TABLES {
        let name = ddl
            .split_whitespace()
            .nth(3)
            .and_then(|n| n.split('(').next())
            .unwrap_or_default();
        if !have.contains(name) {
            db.execute(ddl)?;
        }
    }
    db.write(|run| {
        let w = &mut Tx { run };
        let p = params([("name", s("schema")), ("value", s(SCHEMA))]);
        let r = w.run("MATCH (x:Meta) WHERE x.name = $name RETURN x.value", &p)?;
        match r.first().map(|row| text_of(row.first())) {
            None => {
                w.run("CREATE (:Meta {name: $name, value: $value})", &p)?;
            }
            Some(v) if v == SCHEMA => {}
            Some(v) => {
                return Err(Error::Io(format!(
                    "kosmonaut: the memory database has schema {v}; this Dreamer reads {SCHEMA}"
                )));
            }
        }
        Ok(())
    })
}

/// A write transaction.
struct Tx<'t> {
    run: &'t mut dyn Run,
}

impl Tx<'_> {
    fn run(&mut self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        self.run.run(cypher, p)
    }

    /// Make sure a node exists.
    fn node(&mut self, table: &str, field: &str, value: &str) -> Result<(), Error> {
        let p = params([("v", s(value))]);
        let found = self.run(
            &format!("MATCH (t:{table}) WHERE t.{field} = $v RETURN t.{field}"),
            &p,
        )?;
        if found.is_empty() {
            self.run(&format!("CREATE (:{table} {{{field}: $v}})"), &p)?;
        }
        Ok(())
    }

    /// Close the current edges of memory `k` that are not `wanted`, and
    /// open the wanted ones it does not have.
    fn relate(&mut self, k: &str, wanted: &BTreeSet<(&'static str, String)>) -> Result<(), Error> {
        let now = graph::now_us();
        let mut current = BTreeSet::new();
        for (rel, table, field) in RELATIONS {
            let r = self.run(
                &format!(
                    "MATCH (m:Memory)-[r:{rel}]->(t:{table}) WHERE m.key = $key \
                     AND r.valid_to IS NULL RETURN t.{field}, r.valid_from"
                ),
                &params([("key", s(k))]),
            )?;
            for row in &r {
                let target = text_of(row.first());
                let from = micros_of(row.get(1)).unwrap_or(now);
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
                     AND t.{field} = $target AND r.valid_to IS NULL \
                     SET r.valid_to = timestamp($to)"
                ),
                &params([("key", s(k)), ("target", s(target)), ("to", stamp(to))]),
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
                     CREATE (m)-[:{rel} {{valid_from: timestamp($from)}}]->(t)"
                ),
                &params([("key", s(k)), ("target", s(target)), ("from", stamp(now))]),
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
        graph::owner(cx, new.scope)?;
        let _guard = self.lock();
        let (k, owner, m) = graph::fresh(cx, new, |k| Ok(self.find(k)?.is_some()))?;
        self.store(cx, &k, &owner, &m)?;
        Ok(m)
    }

    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error> {
        let items = self
            .load(cx, &graph::scopes(cx, scope))?
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
        let (k, m) = self.existing(cx, id)?;
        let m = graph::changed(m, change)?;
        let owner = graph::owner(cx, m.scope)?;
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
        let (k, owner, m) = graph::placed(cx, m)?;
        let _guard = self.lock();
        self.store(cx, &k, &owner, &m)?;
        Ok(m)
    }

    fn recall(
        &self,
        cx: &Context,
        query: &str,
        scope: Option<Scope>,
        limit: Option<usize>,
    ) -> Result<Found, Error> {
        let all = self.load(cx, &graph::scopes(cx, scope))?;
        graph::recall(self, self.embedder.as_deref(), all, query, limit)
    }
}

impl Recall for Kosmonaut {
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
        if self.db()?.tables()?.contains(&table) {
            let r = self.read(
                &format!(
                    "MATCH (x:{table}) WHERE x.key IN $keys AND x.model = $model \
                     RETURN x.key, x.v"
                ),
                &params([("keys", strings(&keys)), ("model", s(e.model()))]),
            )?;
            for row in &r {
                if let Some(v) = floats_of(row.get(1)) {
                    have.insert(text_of(row.first()), v);
                }
            }
        }
        let missing: Vec<&(String, Memory)> = items
            .iter()
            .filter(|(k, _)| !have.contains_key(k))
            .collect();
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|(_, m)| graph::vector_text(m)).collect();
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let got = e.embed(&refs)?;
            let new: Vec<(String, Vec<f32>)> =
                missing.iter().map(|(k, _)| k.clone()).zip(got).collect();
            self.put_vectors(e, &new)?;
            have.extend(new);
        }
        Ok((q, have))
    }

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
            for row in self.read(q, &p)? {
                *out.entry(text_of(row.first())).or_default() += 1;
            }
        }
        Ok(out)
    }

    fn place(&self) -> PathBuf {
        PathBuf::from(self.store.url())
    }
}
