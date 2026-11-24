//! The `sqlite` provider: one SQLite database per user holds every scope.
//! Each memory is a row tagged with its scope and owner (the project root
//! or the session id), so a query only ever sees the scopes its context
//! reaches. SQLite is compiled into the program (`rusqlite`, `bundled`):
//! no system library and no extension.
//!
//! The tables:
//!
//! ```text
//! memories(key, scope, owner, name, kind, title, description, body,
//!          links, created, updated)
//! edges(memory, relation, target, valid_from, valid_to)
//!     IN_PROJECT -> project root    IN_SESSION -> session id
//!     ABOUT_FILE -> path            ABOUT_PANE -> pane (`%3`)
//!     LINKS_TO   -> another memory's key
//! vectors(memory, model, v)         one per memory, little-endian f32
//! collector_rows(...)               rows the collectors keep
//! summaries(session, text, updated) one per agent session
//! imported(scope, owner, at, memories)
//!                                   scope owners copied in from files
//! ```
//!
//! Every edge has a valid time in microseconds: when a link is dropped its
//! edge is closed (`valid_to` set), not deleted, so the database keeps what
//! was true until when. Forgetting a memory deletes it, its edges, and the
//! edges other memories had to it. Recall is the one the Kosmonaut
//! provider gives ([`crate::graph::recall`]): words exactly as
//! [`crate::search`] ranks them, then memories one edge away, then, with an
//! embedder, memories whose vectors are near the query's.
//!
//! The database is opened on the first call and kept open; every call
//! reads it again, so nothing cached goes stale. Each write is one
//! transaction. `PRAGMA user_version` records the schema, so a later
//! Dreamer can migrate it and an older one refuses a newer database.
//!
//! Made with [`Sqlite::importing`], the provider copies in what the
//! `files` layout holds, once per scope owner: the user and host scopes on
//! the first call, a project the first time a context reaches it, a
//! session likewise. `imported` records each owner done (even one with no
//! files), so nothing is copied twice, and a memory forgotten here does
//! not come back. The copy keeps ids, kinds, text, links, and times, and
//! leaves a memory this database already holds under the same id; the
//! files are only read. A file that does not read is left where it is and
//! reported once, in the problems of the next list or recall.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::embed::Embedder;
use crate::graph::{self, CollectorRow, Recall, Summary, Vectors};
use crate::{
    Change, Context, Error, Files, Found, Kind, Memory, New, Problem, Provider, Scope, split_id,
    time,
};

/// The schema this Dreamer writes, in `PRAGMA user_version`.
const SCHEMA: i64 = 2;

/// Each schema step, from the one before it: `STEPS[0]` makes version 1.
const STEPS: &[&str] = &[
    "
CREATE TABLE memories(
    key TEXT PRIMARY KEY,
    scope TEXT NOT NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    body TEXT NOT NULL,
    links TEXT NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX memories_owner ON memories(scope, owner);
CREATE TABLE edges(
    memory TEXT NOT NULL REFERENCES memories(key) ON DELETE CASCADE,
    relation TEXT NOT NULL,
    target TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_to INTEGER
);
CREATE INDEX edges_memory ON edges(memory);
CREATE INDEX edges_target ON edges(relation, target);
CREATE TABLE vectors(
    memory TEXT PRIMARY KEY REFERENCES memories(key) ON DELETE CASCADE,
    model TEXT NOT NULL,
    v BLOB NOT NULL
);
CREATE TABLE collector_rows(
    id INTEGER PRIMARY KEY,
    collector TEXT NOT NULL,
    at INTEGER NOT NULL,
    pane TEXT NOT NULL,
    session TEXT,
    cwd TEXT,
    project TEXT,
    command TEXT,
    fields TEXT NOT NULL,
    context TEXT NOT NULL
);
CREATE INDEX collector_rows_at ON collector_rows(collector, at);
CREATE INDEX collector_rows_pane ON collector_rows(pane);
CREATE INDEX collector_rows_session ON collector_rows(session);
CREATE INDEX collector_rows_project ON collector_rows(project);
CREATE TABLE summaries(
    session TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    updated INTEGER NOT NULL
);
",
    "
CREATE TABLE imported(
    scope TEXT NOT NULL,
    owner TEXT NOT NULL,
    at INTEGER NOT NULL,
    memories INTEGER NOT NULL,
    PRIMARY KEY(scope, owner)
);
",
];

/// How long a call waits for another connection's write to finish.
const BUSY: Duration = Duration::from_secs(5);

pub struct Sqlite {
    path: PathBuf,
    embedder: Option<Arc<dyn Embedder>>,
    /// Opened on first use and kept; tried again on the next call if
    /// opening failed. One call at a time uses it.
    conn: Mutex<Option<Connection>>,
    /// Where memories are copied in from, once per scope owner.
    files: Option<Arc<Files>>,
    /// Files the copy could not read, until a list or recall reports them.
    unread: Mutex<Vec<Problem>>,
}

impl std::fmt::Debug for Sqlite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sqlite")
            .field("path", &self.path)
            .field("embedder", &self.embedder.as_ref().map(|e| e.model()))
            .finish_non_exhaustive()
    }
}

fn db_error(e: impl std::fmt::Display) -> Error {
    Error::Io(format!("sqlite: {e}"))
}

fn json(v: &impl serde::Serialize) -> Result<String, Error> {
    serde_json::to_string(v).map_err(|e| Error::Io(e.to_string()))
}

fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn floats(b: &[u8]) -> Option<Vec<f32>> {
    let (words, rest) = b.as_chunks::<4>();
    rest.is_empty()
        .then(|| words.iter().map(|w| f32::from_le_bytes(*w)).collect())
}

/// Columns every memory read returns, in this order.
const COLUMNS: &str = "scope, name, kind, title, description, body, links, created, updated, key";

fn memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<(String, Memory)>> {
    let scope: String = row.get(0)?;
    let name: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let links: String = row.get(6)?;
    let (Some(scope), Some(kind)) = (Scope::from_name(&scope), Kind::from_name(&kind)) else {
        return Ok(None);
    };
    let ms = |i| row.get::<_, i64>(i).map(|v| u64::try_from(v).unwrap_or(0));
    Ok(Some((
        row.get(9)?,
        Memory {
            id: format!("{}/{name}", scope.name()),
            scope,
            kind,
            title: row.get(3)?,
            description: row.get(4)?,
            text: row.get(5)?,
            links: serde_json::from_str(&links).unwrap_or_default(),
            created_unix_ms: ms(7)?,
            updated_unix_ms: ms(8)?,
            path: None,
            score: None,
        },
    )))
}

fn find(c: &Connection, k: &str) -> Result<Option<Memory>, Error> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM memories WHERE key = ?1"),
        [k],
        memory,
    )
    .optional()
    .map(|m| m.flatten().map(|(_, m)| m))
    .map_err(db_error)
}

fn exists(c: &Connection, k: &str) -> Result<bool, Error> {
    c.query_row("SELECT 1 FROM memories WHERE key = ?1", [k], |_| Ok(()))
        .optional()
        .map(|r| r.is_some())
        .map_err(db_error)
}

/// Whether the scope owner was copied in from files already.
fn done(c: &Connection, scope: Scope, owner: &str) -> Result<bool, Error> {
    c.query_row(
        "SELECT 1 FROM imported WHERE scope = ?1 AND owner = ?2",
        params![scope.name(), owner],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .map_err(db_error)
}

/// Make the tables, or bring them up to [`SCHEMA`].
fn migrate(c: &mut Connection, path: &Path) -> Result<(), Error> {
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_error)?;
    if version > SCHEMA {
        return Err(Error::Io(format!(
            "sqlite: {} has schema {version}; this Dreamer reads {SCHEMA}",
            path.display()
        )));
    }
    for step in &STEPS[usize::try_from(version).unwrap_or(0)..] {
        tx.execute_batch(step).map_err(db_error)?;
    }
    if version < SCHEMA {
        tx.pragma_update(None, "user_version", SCHEMA)
            .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
}

impl Sqlite {
    /// The provider over the database at `path` (a file, made on first
    /// use). With `embedder`, memories also get vectors and recall also
    /// looks for memories near the query in meaning.
    pub fn new(path: PathBuf, embedder: Option<Arc<dyn Embedder>>) -> Sqlite {
        Sqlite {
            path,
            embedder,
            conn: Mutex::new(None),
            files: None,
            unread: Mutex::new(Vec::new()),
        }
    }

    /// Copy in what `files` holds, once for each scope owner a call
    /// reaches (see the module documentation). The files are only read.
    pub fn importing(mut self, files: Arc<Files>) -> Sqlite {
        self.files = Some(files);
        self
    }

    /// Copy in from `files` each scope owner `cx` reaches that has not
    /// been copied yet.
    fn adopt(&self, cx: &Context) -> Result<(), Error> {
        let Some(files) = &self.files else {
            return Ok(());
        };
        for scope in crate::reach(cx) {
            // A scope the context cannot name, or the files layout has no
            // place for (no configuration directory), has nothing to copy;
            // the call itself says what is wrong.
            let (Ok(owner), Ok(dir)) = (graph::owner(cx, scope), files.dir(cx, scope)) else {
                continue;
            };
            if done(self.conn()?.get(), scope, &owner)? {
                continue;
            }
            let found = match files.list(cx, Some(scope), None) {
                Ok(f) => f,
                Err(e) => {
                    // Not recorded: tried again on the next call.
                    self.unread(vec![Problem {
                        path: dir,
                        message: format!("not copied into sqlite: {e}"),
                    }]);
                    continue;
                }
            };
            let mut problems = found.problems;
            // Oldest first, so a later memory's links find the earlier.
            let mut items = found.items;
            items.reverse();
            self.write(|tx| {
                // Another call may have copied it meanwhile.
                if done(tx, scope, &owner)? {
                    return Ok(());
                }
                let mut copied = Vec::new();
                for m in &items {
                    let placed = graph::placed(cx, m);
                    let (k, owner, m) = match placed {
                        Ok(p) => p,
                        Err(e) => {
                            problems.push(Problem {
                                path: m.path.clone().unwrap_or_else(|| dir.clone()),
                                message: e.to_string(),
                            });
                            continue;
                        }
                    };
                    if exists(tx, &k)? {
                        continue;
                    }
                    Sqlite::row(tx, &k, &owner, &m)?;
                    copied.push((k, owner, m));
                }
                // Edges once every memory is in, so links between them
                // hold whatever their order.
                for (k, owner, m) in &copied {
                    let wanted = graph::wanted(cx, m.scope, owner, &m.links, |t| exists(tx, t))?;
                    relate(tx, k, &wanted)?;
                }
                tx.execute(
                    "INSERT INTO imported(scope, owner, at, memories) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        scope.name(),
                        owner,
                        int(time::now_ms()),
                        i64::try_from(copied.len()).unwrap_or(i64::MAX)
                    ],
                )
                .map(drop)
                .map_err(db_error)
            })?;
            self.unread(problems);
        }
        Ok(())
    }

    fn unread(&self, problems: Vec<Problem>) {
        if !problems.is_empty() {
            let mut u = self.unread.lock().unwrap_or_else(|e| e.into_inner());
            u.extend(problems);
        }
    }

    /// `found` with the files the copy could not read, reported once.
    fn with_unread(&self, mut found: Found) -> Found {
        let mut u = self.unread.lock().unwrap_or_else(|e| e.into_inner());
        if !u.is_empty() {
            let mut problems = std::mem::take(&mut *u);
            problems.append(&mut found.problems);
            found.problems = problems;
        }
        found
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(&self) -> Result<Connection, Error> {
        let at = |e: &dyn std::fmt::Display| Error::Io(format!("{}: {e}", self.path.display()));
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| at(&e))?;
        }
        let mut c = Connection::open(&self.path).map_err(|e| at(&e))?;
        c.busy_timeout(BUSY).map_err(db_error)?;
        c.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        c.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_error)?;
        c.pragma_update(None, "foreign_keys", true)
            .map_err(db_error)?;
        migrate(&mut c, &self.path)?;
        Ok(c)
    }

    /// The connection, opened on first use.
    fn conn(&self) -> Result<Db<'_>, Error> {
        let mut slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(self.open()?);
        }
        Ok(Db(slot))
    }

    /// Run `f` in one write transaction: all of it commits, or none.
    fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T, Error>) -> Result<T, Error> {
        let mut db = self.conn()?;
        let tx = db
            .get()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let out = f(&tx)?;
        tx.commit().map_err(db_error)?;
        Ok(out)
    }

    /// Memories in `scopes` for `cx`, newest first, with their keys.
    fn load(&self, cx: &Context, scopes: &[Scope]) -> Result<Vec<(String, Memory)>, Error> {
        let mut owners = Vec::new();
        for &scope in scopes {
            owners.push((scope, graph::owner(cx, scope)?));
        }
        let mut db = self.conn()?;
        let c = db.get();
        let mut q = c
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM memories WHERE scope = ?1 AND owner = ?2"
            ))
            .map_err(db_error)?;
        let mut out = Vec::new();
        for (scope, owner) in owners {
            let rows = q
                .query_map(params![scope.name(), owner], memory)
                .map_err(db_error)?;
            for r in rows {
                out.extend(r.map_err(db_error)?);
            }
        }
        graph::newest_first(&mut out);
        Ok(out)
    }

    fn existing(&self, cx: &Context, id: &str) -> Result<(String, Memory), Error> {
        let (scope, name) = split_id(id)?;
        let k = graph::key(scope, &graph::owner(cx, scope)?, name);
        let m = find(self.conn()?.get(), &k)?.ok_or_else(|| Error::NotFound(id.to_owned()))?;
        Ok((k, m))
    }

    /// A vector for `m` now, if there is an embedder. A failure is left
    /// for recall, which embeds what has no vector yet.
    fn embed_one(&self, m: &Memory) -> Option<(String, Vec<f32>)> {
        let e = self.embedder.as_ref()?;
        let mut v = e.embed(&[&graph::vector_text(m)]).ok()?;
        Some((e.model().to_owned(), v.pop()?))
    }

    /// Keep `m` under key `k`, creating or replacing it, with its edges
    /// brought up to date and its vector, all in `tx`.
    fn store(
        tx: &Transaction<'_>,
        cx: &Context,
        k: &str,
        owner: &str,
        m: &Memory,
        vector: Option<(String, Vec<f32>)>,
    ) -> Result<(), Error> {
        let wanted = graph::wanted(cx, m.scope, owner, &m.links, |t| exists(tx, t))?;
        Sqlite::row(tx, k, owner, m)?;
        relate(tx, k, &wanted)?;
        match vector {
            Some((model, v)) => {
                tx.execute(
                    "INSERT OR REPLACE INTO vectors(memory, model, v) VALUES (?1, ?2, ?3)",
                    params![k, model, blob(&v)],
                )
                .map_err(db_error)?;
            }
            // The text may have changed: an old vector would mislead.
            None => {
                tx.execute("DELETE FROM vectors WHERE memory = ?1", [k])
                    .map_err(db_error)?;
            }
        }
        Ok(())
    }

    /// The memory's row under key `k`, created or replaced; its edges and
    /// vector are left as they are.
    fn row(tx: &Transaction<'_>, k: &str, owner: &str, m: &Memory) -> Result<(), Error> {
        let (_, name) = split_id(&m.id)?;
        tx.execute(
            "INSERT INTO memories(key, scope, owner, name, kind, title, description, body, \
             links, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(key) DO UPDATE SET kind = excluded.kind, title = excluded.title, \
             description = excluded.description, body = excluded.body, links = excluded.links, \
             created = excluded.created, updated = excluded.updated",
            params![
                k,
                m.scope.name(),
                owner,
                name,
                m.kind.name(),
                m.title,
                m.description,
                m.text,
                json(&m.links)?,
                int(m.created_unix_ms),
                int(m.updated_unix_ms),
            ],
        )
        .map(drop)
        .map_err(db_error)
    }

    /// Keep a collector's row, with its pane, session, and project.
    pub fn add_row(&self, row: &CollectorRow) -> Result<(), Error> {
        let fields = json(&row.fields)?;
        let context = json(&row.context)?;
        let cwd = row.cwd.as_ref().map(|c| c.display().to_string());
        let project = row
            .cwd
            .as_deref()
            .filter(|c| c.is_absolute())
            .map(|c| crate::project_root(c).display().to_string());
        self.write(|tx| {
            tx.execute(
                "INSERT INTO collector_rows(collector, at, pane, session, cwd, project, command, \
                 fields, context) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.collector,
                    int(row.at_unix_ms),
                    row.pane,
                    row.session,
                    cwd,
                    project,
                    row.command,
                    fields,
                    context,
                ],
            )
            .map(drop)
            .map_err(db_error)
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
        let mut db = self.conn()?;
        let mut q = db
            .get()
            .prepare_cached(
                "SELECT collector, at, pane, session, cwd, command, fields, context \
                 FROM collector_rows WHERE collector = ?1 AND at >= ?2 ORDER BY at, id LIMIT ?3",
            )
            .map_err(db_error)?;
        let rows = q
            .query_map(
                params![
                    collector,
                    int(since_unix_ms),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |r| {
                    let opt = |i| {
                        r.get::<_, Option<String>>(i)
                            .map(|v| v.filter(|t| !t.is_empty()))
                    };
                    let fields: String = r.get(6)?;
                    let context: String = r.get(7)?;
                    Ok(CollectorRow {
                        collector: r.get(0)?,
                        at_unix_ms: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                        pane: r.get(2)?,
                        session: opt(3)?,
                        cwd: opt(4)?.map(PathBuf::from),
                        command: opt(5)?,
                        fields: serde_json::from_str(&fields).unwrap_or_default(),
                        context: serde_json::from_str(&context).unwrap_or_default(),
                    })
                },
            )
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_error)
    }

    /// Keep `text` as the summary of `session`, replacing the one there.
    pub fn set_summary(&self, session: &str, text: &str) -> Result<Summary, Error> {
        let cx = Context {
            cwd: None,
            session: Some(session.to_owned()),
        };
        let session = crate::session_of(&cx)?;
        let now = time::now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO summaries(session, text, updated) VALUES (?1, ?2, ?3)",
                params![session, text, int(now)],
            )
            .map(drop)
            .map_err(db_error)
        })?;
        Ok(Summary {
            session: session.to_owned(),
            text: text.to_owned(),
            updated_unix_ms: now,
        })
    }

    pub fn summary(&self, session: &str) -> Result<Option<Summary>, Error> {
        self.conn()?
            .get()
            .query_row(
                "SELECT session, text, updated FROM summaries WHERE session = ?1",
                [session],
                |r| {
                    Ok(Summary {
                        session: r.get(0)?,
                        text: r.get(1)?,
                        updated_unix_ms: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                    })
                },
            )
            .optional()
            .map_err(db_error)
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
        let mut db = self.conn()?;
        let mut q = db
            .get()
            .prepare_cached(
                "SELECT relation, target, valid_from, valid_to FROM edges WHERE memory = ?1",
            )
            .map_err(db_error)?;
        let rows = q
            .query_map([&k], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })
            .map_err(db_error)?;
        let mut out = Vec::new();
        for r in rows {
            let (rel, target, from, to) = r.map_err(db_error)?;
            if graph::held(from, to, at) {
                let shown = graph::shown(&rel, &target);
                out.push((rel, shown));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// The connection, held for one call.
struct Db<'a>(MutexGuard<'a, Option<Connection>>);

impl Db<'_> {
    fn get(&mut self) -> &mut Connection {
        self.0.as_mut().expect("opened before use")
    }
}

/// Close the current edges of memory `k` that are not `wanted`, and open
/// the wanted ones it does not have.
fn relate(
    tx: &Transaction<'_>,
    k: &str,
    wanted: &std::collections::BTreeSet<(&'static str, String)>,
) -> Result<(), Error> {
    let now = graph::now_us();
    let mut q = tx
        .prepare_cached(
            "SELECT rowid, relation, target, valid_from FROM edges \
             WHERE memory = ?1 AND valid_to IS NULL",
        )
        .map_err(db_error)?;
    let current: Vec<(i64, String, String, i64)> = q
        .query_map([k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)?;
    for (rowid, rel, target, from) in &current {
        if wanted.iter().any(|(r, t)| r == rel && t == target) {
            continue;
        }
        // An interval must not be empty: an edge closed in the
        // microsecond it opened ends one later.
        tx.execute(
            "UPDATE edges SET valid_to = ?1 WHERE rowid = ?2",
            params![now.max(from + 1), rowid],
        )
        .map_err(db_error)?;
    }
    for (rel, target) in wanted {
        if current.iter().any(|(_, r, t, _)| r == rel && t == target) {
            continue;
        }
        tx.execute(
            "INSERT INTO edges(memory, relation, target, valid_from) VALUES (?1, ?2, ?3, ?4)",
            params![k, rel, target, now],
        )
        .map_err(db_error)?;
    }
    Ok(())
}

impl Recall for Sqlite {
    fn neighbours(&self, keys: &[String]) -> Result<BTreeMap<String, u32>, Error> {
        let mut out: BTreeMap<String, u32> = BTreeMap::new();
        let seeds = json(&keys)?;
        let queries = [
            "SELECT target FROM edges WHERE relation = 'LINKS_TO' AND valid_to IS NULL \
             AND memory IN (SELECT value FROM json_each(?1))",
            "SELECT memory FROM edges WHERE relation = 'LINKS_TO' AND valid_to IS NULL \
             AND target IN (SELECT value FROM json_each(?1))",
            "SELECT b.memory FROM edges a JOIN edges b \
             ON b.relation = a.relation AND b.target = a.target AND b.rowid != a.rowid \
             WHERE a.relation IN ('ABOUT_FILE', 'ABOUT_PANE') \
             AND a.memory IN (SELECT value FROM json_each(?1)) \
             AND a.valid_to IS NULL AND b.valid_to IS NULL",
        ];
        let mut db = self.conn()?;
        let c = db.get();
        for sql in queries {
            let mut q = c.prepare_cached(sql).map_err(db_error)?;
            let rows = q
                .query_map([&seeds], |r| r.get::<_, String>(0))
                .map_err(db_error)?;
            for k in rows {
                *out.entry(k.map_err(db_error)?).or_default() += 1;
            }
        }
        Ok(out)
    }

    fn vectors(
        &self,
        e: &dyn Embedder,
        query: &str,
        items: &[(String, Memory)],
    ) -> Result<(Vec<f32>, Vectors), Error> {
        let q = e.embed(&[query])?.pop().unwrap_or_default();
        let keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
        let mut have = Vectors::new();
        {
            let mut db = self.conn()?;
            let mut s = db
                .get()
                .prepare_cached(
                    "SELECT memory, v FROM vectors WHERE model = ?1 \
                     AND memory IN (SELECT value FROM json_each(?2))",
                )
                .map_err(db_error)?;
            let rows = s
                .query_map(params![e.model(), json(&keys)?], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_error)?;
            for r in rows {
                let (k, b) = r.map_err(db_error)?;
                if let Some(v) = floats(&b).filter(|v| v.len() == e.dimension()) {
                    have.insert(k, v);
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
            self.write(|tx| {
                for (k, v) in &new {
                    // Only for a memory still there: one forgotten since
                    // the read keeps no vector.
                    tx.execute(
                        "INSERT OR REPLACE INTO vectors(memory, model, v) \
                         SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM memories WHERE key = ?1)",
                        params![k, e.model(), blob(v)],
                    )
                    .map_err(db_error)?;
                }
                Ok(())
            })?;
            have.extend(new);
        }
        Ok((q, have))
    }

    fn place(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Provider for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn remember(&self, cx: &Context, new: New) -> Result<Memory, Error> {
        // Checked before the embedder runs or the database opens.
        if new.text.trim().is_empty() {
            return Err(Error::Invalid("a memory needs text".into()));
        }
        graph::owner(cx, new.scope)?;
        self.adopt(cx)?;
        let mut vector = None;
        if self.embedder.is_some() {
            // The vector's text does not depend on the name picked.
            let (_, _, draft) = graph::fresh(cx, new.clone(), |_| Ok(false))?;
            vector = self.embed_one(&draft);
        }
        self.write(|tx| {
            let (k, owner, m) = graph::fresh(cx, new, |k| exists(tx, k))?;
            Sqlite::store(tx, cx, &k, &owner, &m, vector)?;
            Ok(m)
        })
    }

    fn list(&self, cx: &Context, scope: Option<Scope>, kind: Option<Kind>) -> Result<Found, Error> {
        self.adopt(cx)?;
        let items = self
            .load(cx, &graph::scopes(cx, scope))?
            .into_iter()
            .map(|(_, m)| m)
            .filter(|m| kind.is_none_or(|k| k == m.kind))
            .collect();
        Ok(self.with_unread(Found {
            items,
            problems: Vec::new(),
        }))
    }

    fn get(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        self.adopt(cx)?;
        self.existing(cx, id).map(|(_, m)| m)
    }

    fn update(&self, cx: &Context, id: &str, change: Change) -> Result<Memory, Error> {
        let (scope, name) = split_id(id)?;
        let owner = graph::owner(cx, scope)?;
        let k = graph::key(scope, &owner, name);
        self.adopt(cx)?;
        let (_, current) = self.existing(cx, id)?;
        let draft = graph::changed(current, change.clone())?;
        let vector = self.embed_one(&draft);
        self.write(|tx| {
            // Read again inside the transaction, so a change made in
            // between is not lost.
            let m = find(tx, &k)?.ok_or_else(|| Error::NotFound(id.to_owned()))?;
            let m = graph::changed(m, change)?;
            let vector = vector.filter(|_| graph::vector_text(&m) == graph::vector_text(&draft));
            Sqlite::store(tx, cx, &k, &owner, &m, vector)?;
            Ok(m)
        })
    }

    fn forget(&self, cx: &Context, id: &str) -> Result<Memory, Error> {
        let (scope, name) = split_id(id)?;
        let k = graph::key(scope, &graph::owner(cx, scope)?, name);
        self.adopt(cx)?;
        self.write(|tx| {
            let m = find(tx, &k)?.ok_or_else(|| Error::NotFound(id.to_owned()))?;
            // Its own edges and vector go with it (ON DELETE CASCADE);
            // the links other memories had to it go too.
            tx.execute(
                "DELETE FROM edges WHERE relation = 'LINKS_TO' AND target = ?1",
                [&k],
            )
            .map_err(db_error)?;
            tx.execute("DELETE FROM memories WHERE key = ?1", [&k])
                .map_err(db_error)?;
            Ok(m)
        })
    }

    fn put(&self, cx: &Context, m: &Memory) -> Result<Memory, Error> {
        let (k, owner, m) = graph::placed(cx, m)?;
        self.adopt(cx)?;
        let vector = self.embed_one(&m);
        self.write(|tx| {
            Sqlite::store(tx, cx, &k, &owner, &m, vector)?;
            Ok(m)
        })
    }

    fn recall(
        &self,
        cx: &Context,
        query: &str,
        scope: Option<Scope>,
        limit: Option<usize>,
    ) -> Result<Found, Error> {
        self.adopt(cx)?;
        let all = self.load(cx, &graph::scopes(cx, scope))?;
        graph::recall(self, self.embedder.as_deref(), all, query, limit)
            .map(|f| self.with_unread(f))
    }
}
