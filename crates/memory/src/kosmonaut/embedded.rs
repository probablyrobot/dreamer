//! The embedded store: Kosmonaut's engine in this process, the database a
//! directory on this machine.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kosmonaut::{Database, DatabaseConfig, LogicalType, QueryResult, Value};

use super::{Params, Rows, Run, db_error};
use crate::Error;

/// Memory the database may use for cached pages. A memory store is small.
const BUFFER_POOL: usize = 16 * 1024 * 1024;

pub(super) struct Embedded {
    path: PathBuf,
    /// Opened on first use and kept; tried again on the next call if
    /// opening failed (another process holding the database, say).
    db: Mutex<Option<Arc<Database>>>,
}

impl Embedded {
    pub(super) fn new(path: PathBuf) -> Embedded {
        Embedded {
            path,
            db: Mutex::new(None),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
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
        let db = Arc::new(db);
        *slot = Some(db.clone());
        Ok(db)
    }

    pub(super) fn read(&self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        let db = self.db()?;
        let mut conn = db.connect();
        let mut tx = conn.begin_read_only().map_err(db_error)?;
        tx.run_with_params(cypher, &values(p))
            .map(|r| rows(&r))
            .map_err(db_error)
    }

    /// Run `f` in one write transaction; dropped without a commit when `f`
    /// fails, which rolls it back.
    pub(super) fn write<T>(
        &self,
        f: impl FnOnce(&mut dyn Run) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let db = self.db()?;
        let mut conn = db.connect();
        let tx = conn.begin().map_err(db_error)?;
        let mut w = Tx(tx);
        let out = f(&mut w)?;
        w.0.commit().map_err(db_error)?;
        Ok(out)
    }

    pub(super) fn tables(&self) -> Result<BTreeSet<String>, Error> {
        Ok(self
            .db()?
            .connect()
            .list_tables()
            .map_err(db_error)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    pub(super) fn execute(&self, ddl: &str) -> Result<(), Error> {
        self.db()?
            .connect()
            .execute(ddl)
            .map(|_| ())
            .map_err(db_error)
    }
}

struct Tx<'t>(kosmonaut::Transaction<'t>);

impl Run for Tx<'_> {
    fn run(&mut self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        self.0
            .run_with_params(cypher, &values(p))
            .map(|r| rows(&r))
            .map_err(db_error)
    }
}

/// The cells of a result, as the server would send them.
fn rows(r: &QueryResult) -> Rows {
    r.rows()
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(kosmonaut::result_json::value_to_json)
                .collect()
        })
        .collect()
}

/// Parameters as the engine binds them, by the server's JSON mapping.
fn values(p: &Params) -> HashMap<String, Value> {
    p.iter().map(|(k, v)| (k.clone(), value(v))).collect()
}

fn value(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Null(LogicalType::String),
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Int64(i),
            None => Value::Double(n.as_f64().unwrap_or_default()),
        },
        J::String(t) => Value::String(t.clone()),
        J::Array(items) => {
            let items: Vec<Value> = items.iter().map(value).collect();
            let t = items
                .first()
                .map_or(LogicalType::String, Value::logical_type);
            Value::List(items, t)
        }
        J::Object(_) => Value::String(v.to_string()),
    }
}
