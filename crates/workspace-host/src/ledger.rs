//! The action ledger: every transition of every action, in an
//! append-only file that is synced before the next step runs.
//!
//! Because `dispatching` is on disk before a command reaches tmux, a
//! restart can tell the two uncertain cases apart: an action that never
//! reached `dispatching` sent nothing and becomes `failed`; one that did may
//! have taken effect and becomes `outcome_unknown`. Neither is replayed.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use api::{ActionRecord, ActionState};

#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    file: File,
    records: BTreeMap<String, ActionRecord>,
    /// Creation order, for listing newest first.
    order: Vec<String>,
    keys: HashMap<String, String>,
}

#[derive(Debug)]
pub enum LedgerError {
    Io(std::io::Error),
    /// A line other than the last could not be read: not a torn write.
    Corrupt {
        line: usize,
        error: String,
    },
    /// A transition the action lifecycle does not allow.
    Transition {
        id: String,
        from: String,
        to: String,
    },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io(e) => write!(f, "action ledger: {e}"),
            LedgerError::Corrupt { line, error } => {
                write!(f, "action ledger line {line} is corrupt: {error}")
            }
            LedgerError::Transition { id, from, to } => {
                write!(f, "action {id}: illegal transition {from} -> {to}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<std::io::Error> for LedgerError {
    fn from(e: std::io::Error) -> Self {
        LedgerError::Io(e)
    }
}

fn domain_state(r: &ActionRecord) -> domain::ActionState {
    let reason = r.reason.clone().unwrap_or_default();
    match ActionState::try_from(r.state).unwrap_or(ActionState::Unspecified) {
        ActionState::Accepted | ActionState::Unspecified => domain::ActionState::Accepted,
        ActionState::Prepared => domain::ActionState::Prepared,
        ActionState::Dispatching => domain::ActionState::Dispatching,
        ActionState::Completed => domain::ActionState::Completed,
        ActionState::Failed => domain::ActionState::Failed { reason },
        ActionState::Cancelled => domain::ActionState::Cancelled,
        ActionState::OutcomeUnknown => domain::ActionState::OutcomeUnknown { reason },
    }
}

pub fn state_name(state: i32) -> &'static str {
    ActionState::try_from(state).map_or("unknown", ActionState::name)
}

impl Ledger {
    /// Open (or create) the ledger and settle actions a previous process
    /// left unfinished. Returns the ledger and the records it settled.
    pub fn open(path: &Path) -> Result<(Ledger, Vec<ActionRecord>), LedgerError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut records = BTreeMap::new();
        let mut order = Vec::new();
        let mut keys = HashMap::new();
        let mut ends_with_newline = true;
        if path.exists() {
            let text = std::fs::read(path)?;
            ends_with_newline = text.last().is_none_or(|b| *b == b'\n');
            let lines: Vec<&[u8]> = text
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .collect();
            for (i, line) in lines.iter().enumerate() {
                match serde_json::from_slice::<ActionRecord>(line) {
                    Ok(r) => {
                        if !records.contains_key(&r.id) {
                            order.push(r.id.clone());
                        }
                        if let Some(k) = &r.idempotency_key {
                            keys.insert(k.clone(), r.id.clone());
                        }
                        records.insert(r.id.clone(), r);
                    }
                    // A crash can tear only the final write.
                    Err(_) if i + 1 == lines.len() && !ends_with_newline => {}
                    Err(e) => {
                        return Err(LedgerError::Corrupt {
                            line: i + 1,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if !ends_with_newline {
            file.write_all(b"\n")?;
        }
        let mut ledger = Ledger {
            path: path.to_path_buf(),
            file,
            records,
            order,
            keys,
        };
        let mut settled = Vec::new();
        let unfinished: Vec<ActionRecord> = ledger
            .records
            .values()
            .filter(|r| !domain_state(r).is_terminal())
            .cloned()
            .collect();
        for mut r in unfinished {
            let dispatched = r.state == ActionState::Dispatching as i32;
            if dispatched {
                r.state = ActionState::OutcomeUnknown as i32;
                r.reason = Some(
                    "the daemon stopped while this action was being dispatched; \
                     its effect was not observed and it will not be retried"
                        .into(),
                );
            } else {
                r.state = ActionState::Failed as i32;
                r.reason = Some("the daemon stopped before dispatch; nothing was sent".into());
            }
            r.updated_unix_ms = now_ms();
            ledger.write(&r)?;
            settled.push(r);
        }
        Ok((ledger, settled))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, id: &str) -> Option<&ActionRecord> {
        self.records.get(id)
    }

    pub fn by_key(&self, key: &str) -> Option<&ActionRecord> {
        self.keys.get(key).and_then(|id| self.records.get(id))
    }

    /// Newest first.
    pub fn list(&self, limit: usize) -> Vec<ActionRecord> {
        self.order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| self.records.get(id).cloned())
            .collect()
    }

    /// Record a new action (state `accepted`).
    pub fn insert(&mut self, record: &ActionRecord) -> Result<(), LedgerError> {
        self.order.push(record.id.clone());
        if let Some(k) = &record.idempotency_key {
            self.keys.insert(k.clone(), record.id.clone());
        }
        self.write(record)
    }

    /// Record a transition; refuses ones the lifecycle does not allow.
    pub fn transition(&mut self, record: &ActionRecord) -> Result<(), LedgerError> {
        if let Some(prev) = self.records.get(&record.id)
            && !domain_state(prev).can_become(&domain_state(record))
        {
            return Err(LedgerError::Transition {
                id: record.id.clone(),
                from: state_name(prev.state).into(),
                to: state_name(record.state).into(),
            });
        }
        self.write(record)
    }

    fn write(&mut self, record: &ActionRecord) -> Result<(), LedgerError> {
        let mut line = serde_json::to_vec(record).map_err(|e| LedgerError::Corrupt {
            line: 0,
            error: e.to_string(),
        })?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.records.insert(record.id.clone(), record.clone());
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Read a ledger without settling anything (diagnostics).
pub fn read_only(path: &Path) -> std::io::Result<Vec<ActionRecord>> {
    let f = File::open(path)?;
    let mut out: BTreeMap<String, ActionRecord> = BTreeMap::new();
    for line in BufReader::new(f).lines() {
        if let Ok(r) = serde_json::from_str::<ActionRecord>(&line?) {
            out.insert(r.id.clone(), r);
        }
    }
    Ok(out.into_values().collect())
}
