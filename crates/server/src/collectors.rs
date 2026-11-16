//! Collectors: standing rules that turn pane output into rows (see
//! `docs/collectors.md`).
//!
//! A collector follows each pane it covers through a subscription to the
//! pane's canonical mirror, as a view does, so the pane counts as followed
//! and the tier policy leaves it awake. The records are only a wake-up and
//! the carrier of live effects (bells, the shell's command steps): what a
//! line says is read from the mirror the way a text tap reads it, on the
//! collector's own task, at most once per 100 ms. The mirror never waits
//! for a collector, and nothing here writes to a pane or to tmux.
//!
//! Rows go to a bounded table per collector: in memory for queries, and
//! appended to a JSON-lines file in the state directory, which is rewritten
//! without the rows that are gone once it holds twice what is kept.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::{CollectorState, RpcError, code};
use matcher::{Group, Matcher, Spec};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::log;
use crate::service::State;
use crate::taps::TextState;

const DEFAULT_ROWS: u32 = 5000;
const MAX_ROWS: u32 = 100_000;
const DEFAULT_AGE_S: u64 = 7 * 24 * 3600;
const DEFAULT_PER_SECOND: u32 = 10;
const MAX_PER_SECOND: u32 = 1000;
const MAX_CONTEXT: u32 = 100;
const MAX_LINES: u32 = 1000;
const MAX_FIELDS: usize = 64;
/// Bytes kept of one line or field value.
const MAX_TEXT: usize = 8192;
/// How often a pane that wrote something is looked at.
const LOOK: Duration = Duration::from_millis(100);
/// How long a matched line waits for its `after` lines.
const AFTER_WAIT: Duration = Duration::from_secs(2);
/// Matched lines waiting for their `after` lines, per pane.
const MAX_PENDING: usize = 256;
/// Rows on their way to the file; a row that finds the queue full is
/// dropped and counted.
const QUEUE: usize = 4096;
/// Records a pane's subscription holds before the mirror drops it as
/// lagging (it is then resumed from the journal).
const RECORDS: usize = 1024;
/// How often a pane process's working directory is read again.
const CWD_EVERY: Duration = Duration::from_secs(10);
const SPEC_FILE: &str = "collector.json";
const ROWS_FILE: &str = "rows.jsonl";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn invalid(why: impl Into<String>) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, why)
}

fn cut(s: &str) -> String {
    let mut n = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_owned()
}

/// The path of a `file://host/path` working directory report (OSC 7),
/// percent-decoded; anything else as it is.
fn path_of(report: &str) -> String {
    let Some(rest) = report.strip_prefix("file://") else {
        return report.to_owned();
    };
    let path = rest.find('/').map_or("/", |i| &rest[i..]);
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- the rule -------------------------------------------------------------

#[derive(Debug, Clone)]
enum From {
    All,
    Session(String),
    Window(u32),
    Panes(Vec<u32>),
    Command(Matcher),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Line,
    CommandEnd,
    Idle(Duration),
    Bell,
    Exit,
    Every(Duration),
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Trigger::Line => "line",
            Trigger::CommandEnd => "command_end",
            Trigger::Idle(_) => "idle",
            Trigger::Bell => "bell",
            Trigger::Exit => "exit",
            Trigger::Every(_) => "interval",
        }
    }
}

/// A checked collector definition.
#[derive(Debug, Clone)]
struct Rule {
    from: From,
    trigger: Trigger,
    line: Option<Matcher>,
    /// The line pattern's groups, numbered from 1, with their names.
    groups: Vec<Option<String>>,
    fields: Vec<(String, String)>,
    json: bool,
    screen: bool,
    lines: Option<usize>,
    before: usize,
    after: usize,
    rows: usize,
    max_age_ms: u64,
    per_second: u32,
}

fn label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// Expand `$1`, `${1}`, and `${name}` in `template` with `lookup`; `$$` is
/// a dollar sign. A `$` followed by anything else stays as it is.
fn expand(template: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(r) = rest.strip_prefix('$') {
            out.push('$');
            rest = r;
            continue;
        }
        let (key, after) = if let Some(r) = rest.strip_prefix('{') {
            match r.find('}') {
                Some(j) => (&r[..j], &r[j + 1..]),
                None => {
                    out.push('$');
                    continue;
                }
            }
        } else {
            let n = rest.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                out.push('$');
                continue;
            }
            (&rest[..n], &rest[n..])
        };
        out.push_str(&lookup(key).unwrap_or_default());
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Which group `key` (a number from 1, or a name) refers to.
fn group_index(key: &str, names: &[Option<String>]) -> Option<usize> {
    match key.parse::<usize>() {
        Ok(n) if n >= 1 && n <= names.len() => Some(n - 1),
        Ok(_) => None,
        Err(_) => names.iter().position(|n| n.as_deref() == Some(key)),
    }
}

fn compile(spec: &api::CollectorSpec) -> Result<Rule, String> {
    if !label(&spec.name) {
        return Err("a collector name is 1 to 64 letters, digits, `-`, `_`, or `.`".into());
    }
    let regex = |what: &str, pattern: &str, ignore_case: bool| {
        Matcher::new(&Spec {
            pattern: pattern.to_owned(),
            literals: Vec::new(),
            literal: false,
            ignore_case,
        })
        .map_err(|e| format!("{what}: {e}"))
    };
    let f = spec.from.clone().unwrap_or_default();
    let mut froms = Vec::new();
    if f.all {
        froms.push(From::All);
    }
    if let Some(s) = &f.session {
        froms.push(From::Session(s.clone()));
    }
    if let Some(w) = f.window {
        froms.push(From::Window(w));
    }
    if !f.panes.is_empty() {
        froms.push(From::Panes(f.panes.clone()));
    }
    if let Some(c) = &f.command {
        froms.push(From::Command(regex("from.command", c, false)?));
    }
    let from = match froms.len() {
        1 => froms.remove(0),
        0 => return Err("from needs one of all, session, window, panes, or command".into()),
        _ => {
            return Err("from takes exactly one of all, session, window, panes, or command".into());
        }
    };

    let w = spec.when.clone().unwrap_or_default();
    let mut triggers = Vec::new();
    let mut line = None;
    if let Some(l) = &w.line {
        line = Some(regex("when.line", l, w.ignore_case)?);
        triggers.push(Trigger::Line);
    }
    if w.command_end {
        triggers.push(Trigger::CommandEnd);
    }
    if let Some(ms) = w.idle_ms {
        if !(100..=86_400_000).contains(&ms) {
            return Err("when.idle_ms is from 100 to 86400000".into());
        }
        triggers.push(Trigger::Idle(Duration::from_millis(u64::from(ms))));
    }
    if w.bell {
        triggers.push(Trigger::Bell);
    }
    if w.exit {
        triggers.push(Trigger::Exit);
    }
    if let Some(s) = w.every_s {
        if !(1..=86_400).contains(&s) {
            return Err("when.every_s is from 1 to 86400".into());
        }
        triggers.push(Trigger::Every(Duration::from_secs(u64::from(s))));
    }
    let trigger = match triggers.len() {
        1 => triggers[0],
        0 => {
            return Err(
                "when needs one of line, command_end, idle_ms, bell, exit, or every_s".into(),
            );
        }
        _ => {
            return Err(
                "when takes exactly one of line, command_end, idle_ms, bell, exit, or every_s"
                    .into(),
            );
        }
    };
    if w.ignore_case && line.is_none() {
        return Err("when.ignore_case applies to when.line only".into());
    }

    let c = spec.capture.clone().unwrap_or_default();
    let groups = line.as_ref().map(Matcher::group_names).unwrap_or_default();
    let is_line = trigger == Trigger::Line;
    if !is_line && (!c.fields.is_empty() || c.json || c.before > 0 || c.after > 0) {
        return Err(
            "capture.fields, json, before, and after apply to a when.line collector only".into(),
        );
    }
    if c.fields.len() > MAX_FIELDS {
        return Err(format!("at most {MAX_FIELDS} capture.fields"));
    }
    let mut fields = Vec::new();
    for field in &c.fields {
        if !label(&field.name) {
            return Err(format!(
                "capture field `{}`: a name is 1 to 64 letters, digits, `-`, `_`, or `.`",
                field.name
            ));
        }
        let mut unknown = Vec::new();
        expand(&field.value, |key| {
            if group_index(key, &groups).is_none() {
                unknown.push(key.to_owned());
            }
            None
        });
        if let Some(k) = unknown.first() {
            return Err(format!(
                "capture field `{}` refers to `{k}`, which is not a group of when.line ({} group(s))",
                field.name,
                groups.len()
            ));
        }
        fields.push((field.name.clone(), field.value.clone()));
    }
    if c.before > MAX_CONTEXT || c.after > MAX_CONTEXT {
        return Err(format!(
            "capture.before and after are at most {MAX_CONTEXT}"
        ));
    }
    if c.screen && c.lines.is_some() {
        return Err("capture takes screen or lines, not both".into());
    }
    if let Some(n) = c.lines
        && !(1..=MAX_LINES).contains(&n)
    {
        return Err(format!("capture.lines is from 1 to {MAX_LINES}"));
    }
    let k = spec.keep.clone().unwrap_or_default();
    let rows = k.rows.unwrap_or(DEFAULT_ROWS);
    if !(1..=MAX_ROWS).contains(&rows) {
        return Err(format!("keep.rows is from 1 to {MAX_ROWS}"));
    }
    let age = k.max_age_s.unwrap_or(DEFAULT_AGE_S);
    if age == 0 || age > 10 * 365 * 24 * 3600 {
        return Err("keep.max_age_s is from 1 second to 10 years".into());
    }
    let per_second = spec.per_second.unwrap_or(DEFAULT_PER_SECOND);
    if !(1..=MAX_PER_SECOND).contains(&per_second) {
        return Err(format!("per_second is from 1 to {MAX_PER_SECOND}"));
    }
    // An interval keeps the screen unless it asks for lines.
    let screen = c.screen || (matches!(trigger, Trigger::Every(_)) && c.lines.is_none());
    Ok(Rule {
        from,
        trigger,
        line,
        groups,
        fields,
        json: c.json,
        screen,
        lines: c.lines.map(|n| n as usize),
        before: c.before as usize,
        after: c.after as usize,
        rows: rows as usize,
        max_age_ms: age.saturating_mul(1000),
        per_second,
    })
}

impl Rule {
    /// The panes of `t` this rule covers now.
    fn covers(&self, t: &api::Topology) -> Vec<u32> {
        let mut out: Vec<u32> = match &self.from {
            From::All => t.panes.iter().map(|p| p.id).collect(),
            From::Session(name) => {
                let windows: Vec<u32> = t
                    .sessions
                    .iter()
                    .filter(|s| &s.name == name)
                    .flat_map(|s| s.windows.iter().copied())
                    .collect();
                t.panes
                    .iter()
                    .filter(|p| windows.contains(&p.window))
                    .map(|p| p.id)
                    .collect()
            }
            From::Window(w) => t
                .panes
                .iter()
                .filter(|p| p.window == *w)
                .map(|p| p.id)
                .collect(),
            From::Panes(list) => t
                .panes
                .iter()
                .filter(|p| list.contains(&p.id))
                .map(|p| p.id)
                .collect(),
            From::Command(m) => t
                .panes
                .iter()
                .filter(|p| m.is_match(&p.current_command))
                .map(|p| p.id)
                .collect(),
        };
        out.sort_unstable();
        out
    }

    /// History lines a look reads.
    fn history(&self) -> usize {
        crate::taps::WINDOW.max(self.lines.unwrap_or(0))
    }

    /// The fields of a matched line.
    fn fields_of(&self, line: &str, groups: &[Group]) -> Vec<api::CollectField> {
        let mut out: Vec<api::CollectField> = Vec::new();
        let field = |name: &str, value: &str| api::CollectField {
            name: name.to_owned(),
            value: cut(value),
        };
        let named = groups.iter().any(|g| g.name.is_some());
        for g in groups {
            if let (Some(name), Some(text)) = (&g.name, &g.text) {
                out.push(field(name, text));
            }
        }
        for (name, template) in &self.fields {
            let value = expand(template, |key| {
                group_index(key, &self.groups).and_then(|i| groups.get(i)?.text.clone())
            });
            out.retain(|f| &f.name != name);
            out.push(field(name, &value));
        }
        if !named && self.fields.is_empty() {
            for (i, g) in groups.iter().enumerate() {
                if let Some(text) = &g.text {
                    out.push(field(&(i + 1).to_string(), text));
                }
            }
        }
        if self.json
            && let Ok(serde_json::Value::Object(members)) =
                serde_json::from_str::<serde_json::Value>(line.trim())
        {
            for (k, v) in members.into_iter().take(MAX_FIELDS) {
                let value = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                out.retain(|f| f.name != k);
                out.push(field(&k, &value));
            }
        }
        out
    }
}

// ---- the table ------------------------------------------------------------

struct Table {
    /// Oldest first.
    rows: VecDeque<Arc<api::CollectRow>>,
    next_id: u64,
}

#[derive(Default)]
struct Stats {
    collected: u64,
    dropped: u64,
    panes: Vec<u32>,
    last_error: Option<String>,
    /// Counters changed since they were last saved.
    unsaved: bool,
}

pub struct Collector {
    name: String,
    spec: api::CollectorSpec,
    rule: Rule,
    created_unix_ms: u64,
    dir: PathBuf,
    /// Running (true) or stopped (false).
    running: watch::Sender<bool>,
    deleted: AtomicBool,
    table: Mutex<Table>,
    stats: Mutex<Stats>,
    writer: mpsc::Sender<Arc<api::CollectRow>>,
    /// A pane's follower ended: look at the panes again.
    wake: Notify,
}

impl Collector {
    fn status(&self) -> api::CollectorStatus {
        let state = if *self.running.borrow() {
            CollectorState::Running
        } else {
            CollectorState::Stopped
        };
        let s = lock(&self.stats);
        api::CollectorStatus {
            name: self.name.clone(),
            spec: Some(self.spec.clone()),
            state: state as i32,
            rows: lock(&self.table).rows.len() as u64,
            collected: s.collected,
            dropped: s.dropped,
            panes: s.panes.clone(),
            created_unix_ms: self.created_unix_ms,
            last_error: s.last_error.clone(),
        }
    }

    fn error(&self, why: String) {
        let mut s = lock(&self.stats);
        if s.last_error.as_ref() != Some(&why) {
            log::warn(format!("collectors: `{}`: {why}", self.name));
            s.last_error = Some(why);
        }
    }

    /// A follower of `pane` started (it sees everything the pane writes
    /// from now on) or ended.
    fn following(&self, pane: u32, on: bool) {
        let mut s = lock(&self.stats);
        s.panes.retain(|p| *p != pane);
        if on {
            s.panes.push(pane);
            s.panes.sort_unstable();
        }
    }

    fn dropped(&self, n: u64) {
        let mut s = lock(&self.stats);
        s.dropped += n;
        s.unsaved = true;
    }

    /// Add a row: the id is assigned here. `None` when the file's queue is
    /// full (counted as dropped).
    fn push(&self, state: &State, mut row: api::CollectRow) -> Option<u64> {
        let row = {
            let mut t = lock(&self.table);
            let Ok(permit) = self.writer.try_reserve() else {
                drop(t);
                self.dropped(1);
                return None;
            };
            row.id = t.next_id;
            t.next_id += 1;
            let row = Arc::new(row);
            t.rows.push_back(row.clone());
            while t.rows.len() > self.rule.rows {
                t.rows.pop_front();
            }
            // Sent under the table's lock, so the file gets rows in id order.
            permit.send(row.clone());
            row
        };
        {
            let mut s = lock(&self.stats);
            s.collected += 1;
            s.unsaved = true;
        }
        state.events.collect_row(&self.name, row.pane, row.id);
        Some(row.id)
    }

    /// Drop rows older than the rule keeps.
    fn expire(&self, now: u64) {
        let oldest = now.saturating_sub(self.rule.max_age_ms);
        let mut t = lock(&self.table);
        while t.rows.front().is_some_and(|r| r.time_unix_ms < oldest) {
            t.rows.pop_front();
        }
    }

    fn meta(&self) -> serde_json::Value {
        let next_id = lock(&self.table).next_id;
        let s = lock(&self.stats);
        serde_json::json!({
            "spec": self.spec,
            "created_unix_ms": self.created_unix_ms,
            "stopped": !*self.running.borrow(),
            "collected": s.collected,
            "dropped": s.dropped,
            "next_id": next_id,
        })
    }

    /// Write the definition and counters (atomically: a new file renamed
    /// over the old).
    fn save(&self) -> Result<(), String> {
        if self.deleted.load(Ordering::SeqCst) {
            return Ok(());
        }
        lock(&self.stats).unsaved = false;
        let text = serde_json::to_string_pretty(&self.meta()).map_err(|e| e.to_string())?;
        replace(&self.dir.join(SPEC_FILE), text.as_bytes())
    }
}

fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let tmp = path.with_extension("new");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    platform::owner_only(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn append(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn row_line(r: &api::CollectRow) -> String {
    let mut s = serde_json::to_string(r).unwrap_or_default();
    s.push('\n');
    s
}

/// Appends rows to the file in order, rewrites it without the rows that
/// are gone once it holds twice what is kept, expires rows by age, and
/// saves the counters.
async fn write(
    c: Arc<Collector>,
    mut rx: mpsc::Receiver<Arc<api::CollectRow>>,
    mut on_file: usize,
) {
    let sweep = Duration::from_millis((c.rule.max_age_ms / 4).clamp(250, 30_000));
    let mut tick = tokio::time::interval(sweep);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The newest row on file: a rewrite includes no row past it, so a row
    // still queued is never written twice.
    let mut written = lock(&c.table).next_id.saturating_sub(1);
    loop {
        let batch = tokio::select! {
            first = rx.recv() => {
                let Some(first) = first else { return };
                let mut batch = vec![first];
                while batch.len() < 1024 {
                    match rx.try_recv() {
                        Ok(r) => batch.push(r),
                        Err(_) => break,
                    }
                }
                batch
            }
            _ = tick.tick() => {
                c.expire(now_ms());
                Vec::new()
            }
        };
        if c.deleted.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(last) = batch.last() {
            written = last.id;
            on_file += batch.len();
            let text: String = batch.iter().map(|r| row_line(r)).collect();
            let path = c.dir.join(ROWS_FILE);
            match tokio::task::spawn_blocking(move || append(&path, &text)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => c.error(format!("cannot write rows: {e}")),
                Err(e) => c.error(format!("cannot write rows: {e}")),
            }
        }
        let kept = lock(&c.table).rows.len();
        if on_file > 2 * kept + 64 {
            let text: String = lock(&c.table)
                .rows
                .iter()
                .filter(|r| r.id <= written)
                .map(|r| row_line(r))
                .collect();
            let count = text.lines().count();
            let path = c.dir.join(ROWS_FILE);
            match tokio::task::spawn_blocking(move || replace(&path, text.as_bytes())).await {
                Ok(Ok(())) => on_file = count,
                Ok(Err(e)) => c.error(format!("cannot compact rows: {e}")),
                Err(e) => c.error(format!("cannot compact rows: {e}")),
            }
        }
        if lock(&c.stats).unsaved {
            let c2 = c.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || c2.save()).await {
                c.error(format!("cannot save: {e}"));
            }
        }
    }
}

// ---- the registry ---------------------------------------------------------

pub struct Collectors {
    entries: Mutex<HashMap<String, Arc<Collector>>>,
    dir: PathBuf,
}

impl std::fmt::Debug for Collectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collectors").finish_non_exhaustive()
    }
}

impl Collectors {
    /// Collectors kept under `dir` (in the daemon's state directory).
    pub fn new(dir: PathBuf) -> Collectors {
        Collectors {
            entries: Mutex::new(HashMap::new()),
            dir,
        }
    }

    fn get(&self, name: &str) -> Result<Arc<Collector>, RpcError> {
        lock(&self.entries)
            .get(name)
            .cloned()
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no collector `{name}`")))
    }

    pub fn list(&self) -> api::CollectList {
        let mut collectors: Vec<api::CollectorStatus> =
            lock(&self.entries).values().map(|c| c.status()).collect();
        collectors.sort_by(|a, b| a.name.cmp(&b.name));
        api::CollectList { collectors }
    }

    pub fn query(&self, q: &api::CollectQuery) -> Result<api::CollectResult, RpcError> {
        let c = self.get(&q.name)?;
        let limit = q.limit.unwrap_or(100);
        if !(1..=10_000).contains(&limit) {
            return Err(invalid("limit is from 1 to 10000"));
        }
        let limit = limit as usize;
        c.expire(now_ms());
        let rows: Vec<Arc<api::CollectRow>> = lock(&c.table)
            .rows
            .iter()
            .filter(|r| q.panes.is_empty() || q.panes.contains(&r.pane))
            .filter(|r| q.since_unix_ms.is_none_or(|t| r.time_unix_ms >= t))
            .filter(|r| q.until_unix_ms.is_none_or(|t| r.time_unix_ms <= t))
            .filter(|r| q.after.is_none_or(|a| r.id > a))
            .filter(|r| {
                q.fields.iter().all(|want| {
                    r.fields
                        .iter()
                        .any(|f| f.name == want.name && f.value == want.value)
                })
            })
            .cloned()
            .collect();
        let mut out = api::CollectResult {
            matched: rows.len() as u64,
            dropped: lock(&c.stats).dropped,
            ..api::CollectResult::default()
        };
        if let Some(by) = &q.group_by {
            let mut groups: HashMap<String, (u64, u64)> = HashMap::new();
            for r in &rows {
                let g = groups.entry(group_key(r, by)).or_default();
                g.0 += 1;
                g.1 = g.1.max(r.time_unix_ms);
            }
            let mut groups: Vec<api::CollectGroup> = groups
                .into_iter()
                .map(|(value, (count, latest))| api::CollectGroup {
                    value,
                    count,
                    latest_unix_ms: latest,
                })
                .collect();
            groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
            out.truncated = groups.len() > limit;
            groups.truncate(limit);
            out.groups = groups;
            return Ok(out);
        }
        out.truncated = rows.len() > limit;
        let picked: Vec<&Arc<api::CollectRow>> = if q.after.is_some() {
            rows.iter().take(limit).collect()
        } else {
            rows.iter().skip(rows.len().saturating_sub(limit)).collect()
        };
        out.rows = picked.into_iter().map(|r| (**r).clone()).collect();
        Ok(out)
    }

    /// Save every collector's counters (at shutdown).
    pub fn save_all(&self) {
        let all: Vec<Arc<Collector>> = lock(&self.entries).values().cloned().collect();
        for c in all {
            if let Err(e) = c.save() {
                log::warn(format!("collectors: `{}`: cannot save: {e}", c.name));
            }
        }
    }
}

/// The value a row is grouped by: a built-in column, else a field.
fn group_key(r: &api::CollectRow, by: &str) -> String {
    match by {
        "pane" => format!("%{}", r.pane),
        "session" => r.session.clone(),
        "window" => r.window.map(|w| format!("@{w}")).unwrap_or_default(),
        "cwd" => r.cwd.clone().unwrap_or_default(),
        "command" => r.command.clone().unwrap_or_default(),
        "trigger" => r.trigger.clone(),
        field => r
            .fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.value.clone())
            .unwrap_or_default(),
    }
}

/// What a collector starts from: kept rows and counters.
struct Saved {
    rows: VecDeque<Arc<api::CollectRow>>,
    next_id: u64,
    collected: u64,
    dropped: u64,
    on_file: usize,
}

impl Saved {
    fn empty() -> Saved {
        Saved {
            rows: VecDeque::new(),
            next_id: 1,
            collected: 0,
            dropped: 0,
            on_file: 0,
        }
    }
}

/// Make a collector and its writer. It is not started.
fn make(
    spec: api::CollectorSpec,
    rule: Rule,
    created_unix_ms: u64,
    dir: PathBuf,
    running: bool,
    saved: Saved,
) -> Arc<Collector> {
    let (writer, rx) = mpsc::channel(QUEUE);
    let on_file = saved.on_file;
    let c = Arc::new(Collector {
        name: spec.name.clone(),
        spec,
        rule,
        created_unix_ms,
        dir,
        running: watch::channel(running).0,
        deleted: AtomicBool::new(false),
        table: Mutex::new(Table {
            rows: saved.rows,
            next_id: saved.next_id,
        }),
        stats: Mutex::new(Stats {
            collected: saved.collected,
            dropped: saved.dropped,
            ..Stats::default()
        }),
        writer,
        wake: Notify::new(),
    });
    tokio::spawn(write(c.clone(), rx, on_file));
    c
}

/// `collect.create`: check the definition, save it, and start following.
/// It replies once the panes it covers now are followed, so what they
/// write afterwards is seen.
pub async fn create(
    state: &Arc<State>,
    spec: api::CollectorSpec,
) -> Result<api::CollectorStatus, RpcError> {
    let rule = compile(&spec).map_err(invalid)?;
    let dir = state.collectors.dir.join(&spec.name);
    // A stopped collector of the same name is replaced; its rows stay.
    let saved = match lock(&state.collectors.entries).get(&spec.name) {
        Some(old) if *old.running.borrow() => {
            return Err(RpcError::new(
                code::CONFLICT,
                format!(
                    "a collector named `{}` is running; stop it first",
                    spec.name
                ),
            ));
        }
        Some(old) => {
            old.deleted.store(true, Ordering::SeqCst);
            let t = lock(&old.table);
            let s = lock(&old.stats);
            Saved {
                rows: t.rows.clone(),
                next_id: t.next_id,
                collected: s.collected,
                dropped: s.dropped,
                on_file: usize::MAX / 4,
            }
        }
        None => Saved::empty(),
    };
    platform::private_dir(&dir).map_err(|e| RpcError::new(code::INTERNAL, e))?;
    let c = make(spec, rule, now_ms(), dir, true, saved);
    c.save().map_err(|e| RpcError::new(code::INTERNAL, e))?;
    lock(&state.collectors.entries).insert(c.name.clone(), c.clone());
    let (ready, started) = oneshot::channel();
    tokio::spawn(supervise(state.clone(), c.clone(), Some(ready)));
    let _ = tokio::time::timeout(Duration::from_secs(15), started).await;
    log::info(format!("collectors: `{}` created", c.name));
    Ok(c.status())
}

/// `collect.stop`: stop following; with `delete`, also forget the
/// definition and its rows.
pub async fn stop(
    state: &Arc<State>,
    name: &str,
    delete: bool,
) -> Result<api::CollectorStatus, RpcError> {
    let c = state.collectors.get(name)?;
    c.running.send_replace(false);
    let status = c.status();
    if delete {
        c.deleted.store(true, Ordering::SeqCst);
        lock(&state.collectors.entries).remove(name);
        let dir = c.dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        log::info(format!("collectors: `{name}` deleted"));
    } else {
        let c2 = c.clone();
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || c2.save()).await {
            c.error(format!("cannot save: {e}"));
        }
        log::info(format!("collectors: `{name}` stopped"));
    }
    Ok(status)
}

/// Read one collector's files.
fn load(dir: &Path) -> Result<(api::CollectorSpec, u64, bool, Saved), String> {
    let text = std::fs::read_to_string(dir.join(SPEC_FILE)).map_err(|e| e.to_string())?;
    let meta: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let spec: api::CollectorSpec =
        serde_json::from_value(meta["spec"].clone()).map_err(|e| e.to_string())?;
    let n = |k: &str| meta[k].as_u64().unwrap_or(0);
    let mut saved = Saved {
        next_id: n("next_id").max(1),
        collected: n("collected"),
        dropped: n("dropped"),
        ..Saved::empty()
    };
    // A torn last line (the daemon stopped mid-write) is skipped, as is
    // any line that does not parse.
    let rows = std::fs::read_to_string(dir.join(ROWS_FILE)).unwrap_or_default();
    let mut last = 0;
    for line in rows.lines() {
        saved.on_file += 1;
        if let Ok(r) = serde_json::from_str::<api::CollectRow>(line)
            && r.id > last
        {
            last = r.id;
            saved.rows.push_back(Arc::new(r));
        }
    }
    saved.next_id = saved.next_id.max(last + 1);
    Ok((
        spec,
        n("created_unix_ms"),
        meta["stopped"].as_bool().unwrap_or(false),
        saved,
    ))
}

/// Load the saved collectors and start the running ones.
pub async fn start_all(state: &Arc<State>) {
    let dir = state.collectors.dir.clone();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.join(SPEC_FILE).is_file() {
            continue;
        }
        let (spec, created, stopped, saved) = match load(&path) {
            Ok(x) => x,
            Err(why) => {
                log::warn(format!("collectors: {}: {why}", path.display()));
                continue;
            }
        };
        let rule = match compile(&spec) {
            Ok(r) => r,
            Err(why) => {
                log::warn(format!("collectors: `{}` is not valid: {why}", spec.name));
                continue;
            }
        };
        let c = make(spec, rule, created, path, !stopped, saved);
        // Rows past the rule's bounds go now.
        {
            let mut t = lock(&c.table);
            while t.rows.len() > c.rule.rows {
                t.rows.pop_front();
            }
        }
        c.expire(now_ms());
        lock(&state.collectors.entries).insert(c.name.clone(), c.clone());
        log::info(format!(
            "collectors: `{}` loaded ({} rows{})",
            c.name,
            lock(&c.table).rows.len(),
            if stopped { ", stopped" } else { "" }
        ));
        if !stopped {
            tokio::spawn(supervise(state.clone(), c, None));
        }
    }
}

// ---- following panes ------------------------------------------------------

/// Read tmux again and publish the topology; connects the endpoint first
/// if it is not connected.
async fn refresh(state: &Arc<State>) -> Result<(), String> {
    let mut guard = state.endpoint().await.map_err(|e| e.message)?;
    let ep = guard.as_mut().ok_or("the tmux endpoint is not connected")?;
    ep.tracker
        .refresh()
        .await
        .map_err(|e| format!("cannot read tmux: {e}"))?;
    crate::service::publish_topology(state, ep);
    Ok(())
}

struct Follower {
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

/// Keep one follower per covered pane until the collector stops.
async fn supervise(state: Arc<State>, c: Arc<Collector>, mut ready: Option<oneshot::Sender<()>>) {
    let mut running = c.running.subscribe();
    let mut topology = state.topology.subscribe();
    let mut followers: HashMap<u32, Follower> = HashMap::new();
    let by_command = matches!(c.rule.from, From::Command(_));
    let mut first = true;
    loop {
        if !*running.borrow() || c.deleted.load(Ordering::SeqCst) {
            for f in followers.values() {
                f.stop.send_replace(true);
            }
            return;
        }
        let stale = state.mirrors.read().await.is_none() || topology.borrow().is_none();
        if (first || by_command || stale)
            && let Err(e) = refresh(&state).await
        {
            c.error(e);
        }
        first = false;
        let host = state.mirrors.read().await.clone();
        let current = topology.borrow_and_update().clone();
        if let (Some(host), Some(update)) = (host, current) {
            let t = update.topology.clone().unwrap_or_default();
            let covered = c.rule.covers(&t);
            followers.retain(|pane, f| {
                if f.handle.is_finished() {
                    return false;
                }
                // A pane that is gone ends its follower itself (and records
                // its exit); one no longer covered is stopped.
                let present = t.panes.iter().any(|p| p.id == *pane);
                if present && !covered.contains(pane) {
                    f.stop.send_replace(true);
                    return false;
                }
                true
            });
            let mut starts = Vec::new();
            for pane in covered {
                if followers.contains_key(&pane) {
                    continue;
                }
                let (stop, stopped) = watch::channel(false);
                let (began, begun) = oneshot::channel();
                let handle = tokio::spawn(follow(
                    state.clone(),
                    c.clone(),
                    host.clone(),
                    pane,
                    stopped,
                    began,
                ));
                followers.insert(pane, Follower { stop, handle });
                starts.push(begun);
            }
            if ready.is_some() {
                for s in starts {
                    let _ = tokio::time::timeout(Duration::from_secs(10), s).await;
                }
            }
        }
        if let Some(r) = ready.take() {
            let _ = r.send(());
        }
        let wait = if by_command {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(5)
        };
        tokio::select! {
            _ = running.changed() => {}
            _ = topology.changed() => {}
            () = c.wake.notified() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Something the pane did, turned into a row after the next look.
enum Queued {
    Bell,
    CommandEnd {
        exit: Option<i32>,
        command: Option<String>,
        ms: Option<u64>,
    },
}

/// One collector following one pane.
struct Watch {
    state: Arc<State>,
    c: Arc<Collector>,
    host: mirror::MirrorHost,
    pane: u32,
    text: TextState,
    last: mirror::TailResult,
    /// Records arrived since the last look.
    dirty: bool,
    looked: Instant,
    /// The newest committed lines, for `before`.
    recent: VecDeque<String>,
    /// Matched lines waiting for their `after` lines, with a deadline.
    pending: VecDeque<(api::CollectRow, Instant)>,
    queued: Vec<Queued>,
    last_output: Instant,
    idle_armed: bool,
    next_every: Option<Instant>,
    /// The command line the shell reported, and when it started.
    command: Option<String>,
    command_started: Option<Instant>,
    tokens: f64,
    refilled: Instant,
    /// Rows this pane dropped since its last recorded row.
    dropped_since: u64,
    /// The pane process's working directory, and when it was read: asked
    /// of the system at most every 10 s, off the async threads.
    cwd: Option<(String, Instant)>,
}

struct Subscribed {
    rx: mpsc::Receiver<mirror::Live>,
    ended: Arc<std::sync::atomic::AtomicU8>,
}

async fn subscribe(
    host: &mirror::MirrorHost,
    pane: u32,
    resume: (domain::PaneGeneration, u64),
) -> Result<Subscribed, String> {
    let (tx, rx) = mpsc::channel(RECORDS);
    let ended = Arc::new(std::sync::atomic::AtomicU8::new(mirror::end::RUNNING));
    let sink = mirror::Sink {
        records: tx,
        client: None,
        ended: ended.clone(),
    };
    host.subscribe(domain::TmuxPaneId(pane), Some(resume), sink)
        .await?;
    Ok(Subscribed { rx, ended })
}

/// Follow `pane` for `c` until it is stopped or the pane's mirror is gone.
async fn follow(
    state: Arc<State>,
    c: Arc<Collector>,
    host: mirror::MirrorHost,
    pane: u32,
    mut stop: watch::Receiver<bool>,
    began: oneshot::Sender<()>,
) {
    let id = domain::TmuxPaneId(pane);
    let start = async {
        if !host.wait_live(&[id], Duration::from_secs(5)).await {
            return Err(format!("pane %{pane} has no live mirror"));
        }
        let t = match host.tail(id, c.rule.history(), None).await {
            Some(Ok(Some(t))) => t,
            Some(Err(e)) => return Err(e),
            _ => return Err(format!("pane %{pane} cannot be read")),
        };
        let sub = subscribe(&host, pane, (t.generation, t.sequence)).await?;
        Ok((t, sub))
    };
    let (t, mut sub) = match start.await {
        Ok(x) => x,
        Err(e) => {
            c.error(e);
            drop(began);
            c.wake.notify_one();
            return;
        }
    };
    let now = Instant::now();
    let mut w = Watch {
        state,
        host: host.clone(),
        pane,
        text: TextState::new(&t),
        last: t,
        dirty: false,
        looked: now,
        recent: VecDeque::new(),
        pending: VecDeque::new(),
        queued: Vec::new(),
        last_output: now,
        idle_armed: false,
        next_every: match c.rule.trigger {
            Trigger::Every(d) => Some(now + d),
            _ => None,
        },
        command: None,
        command_started: None,
        tokens: f64::from(c.rule.per_second),
        refilled: now,
        dropped_since: 0,
        cwd: None,
        c,
    };
    w.refresh_cwd().await;
    w.c.following(pane, true);
    let _ = began.send(());
    loop {
        let deadline = w.deadline();
        let ended = tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    w.flush_pending(true);
                    break;
                }
                false
            }
            item = sub.rx.recv() => match item {
                Some(live) => {
                    w.on_record(live);
                    false
                }
                None => true,
            },
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                w.tick().await;
                false
            }
        };
        if !ended {
            continue;
        }
        match sub.ended.load(Ordering::SeqCst) {
            // Fell behind or rebuilt: continue from the journal (what was
            // missed is read from the mirror anyway).
            mirror::end::LAGGED | mirror::end::REATTACHED => {
                match subscribe(&host, pane, (w.last.generation, w.last.sequence)).await {
                    Ok(s) => {
                        sub = s;
                        w.dirty = true;
                    }
                    Err(e) => {
                        w.c.error(e);
                        break;
                    }
                }
            }
            mirror::end::CONNECTION_LOST => break,
            _ => {
                // The mirror is gone: the pane closed, died, or was
                // respawned.
                w.flush_pending(true);
                if w.c.rule.trigger == Trigger::Exit {
                    w.exited().await;
                }
                break;
            }
        }
    }
    w.c.following(pane, false);
    w.c.wake.notify_one();
}

impl Watch {
    fn deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut at = |t: Instant| d = Some(d.map_or(t, |x| x.min(t)));
        if self.dirty || !self.queued.is_empty() {
            at(self.looked + LOOK);
        }
        if let Some((_, t)) = self.pending.front() {
            at(*t);
        }
        if let Trigger::Idle(idle) = self.c.rule.trigger
            && self.idle_armed
        {
            at(self.last_output + idle);
        }
        if let Some(t) = self.next_every {
            at(t);
        }
        d
    }

    fn on_record(&mut self, (_, record, effect): mirror::Live) {
        use terminal_core::CommandMark;
        if matches!(record, mirror::Record::Bytes(_)) {
            self.last_output = Instant::now();
            self.idle_armed = true;
        }
        self.dirty = true;
        let Some(fx) = effect else { return };
        if fx.bells > 0 && self.c.rule.trigger == Trigger::Bell {
            self.queued.push(Queued::Bell);
        }
        for mark in fx.commands {
            match mark {
                CommandMark::OutputStart { command } => {
                    self.command = command;
                    self.command_started = Some(Instant::now());
                }
                CommandMark::End { exit_code } => {
                    let ms = self
                        .command_started
                        .take()
                        .map(|t| t.elapsed().as_millis() as u64);
                    if self.c.rule.trigger == Trigger::CommandEnd {
                        self.queued.push(Queued::CommandEnd {
                            exit: exit_code,
                            command: self.command.clone(),
                            ms,
                        });
                    }
                }
                CommandMark::PromptStart | CommandMark::InputStart => {}
            }
        }
    }

    /// Read what is new since the last look.
    async fn look(&mut self) {
        self.dirty = false;
        self.looked = Instant::now();
        let id = domain::TmuxPaneId(self.pane);
        let t = match self.host.tail(id, self.c.rule.history(), None).await {
            Some(Ok(Some(t))) => t,
            _ => return,
        };
        let step = self.text.step(&t);
        if let Some((reason, lines)) = step.gap
            && reason == "window"
        {
            self.c.error(format!(
                "pane %{}: {} lines were committed between two looks and not matched",
                self.pane,
                lines.map_or_else(|| "more than 200".to_owned(), |n| n.to_string())
            ));
        }
        self.last = t;
        self.on_lines(step.lines);
    }

    fn on_lines(&mut self, lines: Vec<String>) {
        let rule = self.c.rule.clone();
        for line in lines {
            for (row, _) in &mut self.pending {
                if row.after.len() < rule.after {
                    row.after.push(cut(&line));
                }
            }
            self.flush_pending(false);
            if let Some(m) = &rule.line
                && let Some(groups) = m.captures(&line)
            {
                self.matched(&rule, &line, &groups);
            }
            if rule.before > 0 {
                self.recent.push_back(cut(&line));
                while self.recent.len() > rule.before {
                    self.recent.pop_front();
                }
            }
        }
    }

    /// Whether the rate cap lets one more row through; counts it if not.
    fn admit(&mut self) -> bool {
        let rate = f64::from(self.c.rule.per_second);
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.refilled).as_secs_f64() * rate).min(rate);
        self.refilled = now;
        if self.tokens < 1.0 {
            self.dropped_since += 1;
            self.c.dropped(1);
            return false;
        }
        self.tokens -= 1.0;
        true
    }

    fn matched(&mut self, rule: &Rule, line: &str, groups: &[Group]) {
        if !self.admit() {
            return;
        }
        let row = api::CollectRow {
            trigger: Trigger::Line.name().into(),
            line: cut(line),
            fields: rule.fields_of(line, groups),
            before: self.recent.iter().cloned().collect(),
            ..api::CollectRow::default()
        };
        if rule.after == 0 {
            self.record(row);
            return;
        }
        if self.pending.len() >= MAX_PENDING
            && let Some((oldest, _)) = self.pending.pop_front()
        {
            self.record(oldest);
        }
        self.pending.push_back((row, Instant::now() + AFTER_WAIT));
    }

    /// Record the matched lines whose `after` lines are in or whose wait is
    /// over (all of them with `all`).
    fn flush_pending(&mut self, all: bool) {
        let now = Instant::now();
        let after = self.c.rule.after;
        while let Some((row, deadline)) = self.pending.front() {
            if !(all || row.after.len() >= after || now >= *deadline) {
                break;
            }
            if let Some((row, _)) = self.pending.pop_front() {
                self.record(row);
            }
        }
    }

    /// The screen or the last lines, as the rule asks.
    fn captured(&self) -> Vec<String> {
        let t = &self.last;
        let trim = |mut v: Vec<String>| {
            while v.last().is_some_and(|l| l.trim().is_empty()) {
                v.pop();
            }
            v.iter().map(|l| cut(l)).collect::<Vec<_>>()
        };
        if self.c.rule.screen {
            return trim(t.screen.clone());
        }
        let Some(n) = self.c.rule.lines else {
            return Vec::new();
        };
        let mut all: Vec<String> = if t.alt_screen {
            t.screen.clone()
        } else {
            let upto = (t.cursor_row as usize + 1).min(t.screen.len());
            t.history
                .iter()
                .chain(t.screen[..upto].iter())
                .cloned()
                .collect()
        };
        all = trim(all);
        let skip = all.len().saturating_sub(n);
        all.split_off(skip)
    }

    /// Fill in a row's metadata and add it to the table.
    fn record(&mut self, mut row: api::CollectRow) {
        let topology = self.state.topology.borrow().clone();
        let t = topology
            .and_then(|u| u.topology.clone())
            .unwrap_or_default();
        let pane = t.panes.iter().find(|p| p.id == self.pane);
        if let Some(p) = pane {
            row.window = Some(p.window);
            row.session = t
                .windows
                .iter()
                .find(|w| w.id == p.window)
                .and_then(|w| w.sessions.first())
                .and_then(|s| t.sessions.iter().find(|x| x.id == *s))
                .map(|s| s.name.clone())
                .unwrap_or_default();
        }
        if row.command.is_none() {
            row.command = self
                .command
                .clone()
                .or_else(|| pane.map(|p| p.current_command.clone()))
                .filter(|c| !c.is_empty());
        }
        row.cwd = self
            .last
            .cwd
            .as_deref()
            .map(path_of)
            .or_else(|| self.cwd.as_ref().map(|(c, _)| c.clone()));
        row.collector = self.c.name.clone();
        row.pane = self.pane;
        row.time_unix_ms = now_ms();
        row.dropped = std::mem::take(&mut self.dropped_since);
        if self.c.push(&self.state, row).is_none() {
            self.dropped_since += 1;
        }
    }

    /// Read the pane process's working directory again if the shell does
    /// not report one (OSC 7) and the last read is older than 10 s.
    async fn refresh_cwd(&mut self) {
        if self.last.cwd.is_some()
            || self
                .cwd
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < CWD_EVERY)
        {
            return;
        }
        let pid = self
            .state
            .topology
            .borrow()
            .as_ref()
            .and_then(|u| u.topology.as_ref())
            .and_then(|t| t.panes.iter().find(|p| p.id == self.pane))
            .and_then(|p| p.pid);
        let Some(pid) = pid else { return };
        if let Ok(Some(c)) = tokio::task::spawn_blocking(move || procreg::cwd(pid)).await {
            self.cwd = Some((c, Instant::now()));
        }
    }

    async fn tick(&mut self) {
        let now = Instant::now();
        if self.dirty || !self.queued.is_empty() {
            self.refresh_cwd().await;
            self.look().await;
        }
        for q in std::mem::take(&mut self.queued) {
            if !self.admit() {
                continue;
            }
            let mut row = api::CollectRow {
                lines: self.captured(),
                ..api::CollectRow::default()
            };
            match q {
                Queued::Bell => row.trigger = Trigger::Bell.name().into(),
                Queued::CommandEnd { exit, command, ms } => {
                    row.trigger = Trigger::CommandEnd.name().into();
                    row.exit_status = exit;
                    if let Some(e) = exit {
                        row.fields.push(api::CollectField {
                            name: "exit_status".into(),
                            value: e.to_string(),
                        });
                    }
                    if let Some(ms) = ms {
                        row.fields.push(api::CollectField {
                            name: "duration_ms".into(),
                            value: ms.to_string(),
                        });
                    }
                    row.command = command;
                }
            }
            self.record(row);
        }
        self.flush_pending(false);
        if let Trigger::Idle(idle) = self.c.rule.trigger
            && self.idle_armed
            && now.duration_since(self.last_output) >= idle
        {
            self.idle_armed = false;
            if self.admit() {
                let row = api::CollectRow {
                    trigger: Trigger::Idle(idle).name().into(),
                    lines: self.captured(),
                    fields: vec![api::CollectField {
                        name: "idle_ms".into(),
                        value: idle.as_millis().to_string(),
                    }],
                    ..api::CollectRow::default()
                };
                self.record(row);
            }
        }
        if let (Trigger::Every(every), Some(at)) = (self.c.rule.trigger, self.next_every)
            && now >= at
        {
            let mut next = at + every;
            while next <= now {
                next += every;
            }
            self.next_every = Some(next);
            if self.admit() {
                let row = api::CollectRow {
                    trigger: Trigger::Every(every).name().into(),
                    lines: self.captured(),
                    ..api::CollectRow::default()
                };
                self.record(row);
            }
        }
    }

    /// The pane's process ended: record how, with its last lines.
    async fn exited(&mut self) {
        let generation = self.last.generation;
        // The topology is published before mirrors are dropped.
        let t = self
            .state
            .topology
            .borrow()
            .clone()
            .and_then(|u| u.topology.clone())
            .unwrap_or_default();
        let reason = match t.panes.iter().find(|p| p.id == self.pane) {
            None => "closed",
            Some(p) if p.generation != generation.0 => "respawned",
            Some(p) if p.dead => "dead",
            Some(_) => "ended",
        };
        if !self.admit() {
            return;
        }
        let row = api::CollectRow {
            trigger: Trigger::Exit.name().into(),
            lines: self.captured(),
            fields: vec![api::CollectField {
                name: "reason".into(),
                value: reason.into(),
            }],
            ..api::CollectRow::default()
        };
        self.record(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_spec(pattern: &str) -> api::CollectorSpec {
        api::CollectorSpec {
            name: "t".into(),
            from: Some(api::CollectFrom {
                all: true,
                ..api::CollectFrom::default()
            }),
            when: Some(api::CollectWhen {
                line: Some(pattern.into()),
                ..api::CollectWhen::default()
            }),
            ..api::CollectorSpec::default()
        }
    }

    #[test]
    fn definitions_are_checked_with_clear_errors() {
        let ok = compile(&line_spec("^FAILED (\\S+)")).expect("valid");
        assert_eq!(ok.rows, DEFAULT_ROWS as usize);
        let e = |s: api::CollectorSpec| compile(&s).expect_err("invalid");
        let mut s = line_spec("x");
        s.name = "a b".into();
        assert!(e(s).contains("collector name"));
        let mut s = line_spec("x");
        s.from = None;
        assert!(e(s).contains("from needs one"));
        let mut s = line_spec("x");
        s.from.as_mut().expect("present").panes = vec![3];
        assert!(e(s).contains("exactly one"));
        let mut s = line_spec("x");
        s.when.as_mut().expect("present").bell = true;
        assert!(e(s).contains("when takes exactly one"));
        assert!(e(line_spec("(unclosed")).starts_with("when.line"));
        let mut s = line_spec("^(a)");
        s.capture = Some(api::CollectCapture {
            fields: vec![api::CollectField {
                name: "k".into(),
                value: "$2".into(),
            }],
            ..api::CollectCapture::default()
        });
        assert!(e(s).contains("refers to `2`"));
        let mut s = line_spec("x");
        s.when = Some(api::CollectWhen {
            bell: true,
            ..api::CollectWhen::default()
        });
        s.capture = Some(api::CollectCapture {
            after: 3,
            ..api::CollectCapture::default()
        });
        assert!(e(s).contains("when.line collector only"));
        let mut s = line_spec("x");
        s.keep = Some(api::CollectKeep {
            rows: Some(0),
            max_age_s: None,
        });
        assert!(e(s).contains("keep.rows"));
        let mut s = line_spec("x");
        s.per_second = Some(0);
        assert!(e(s).contains("per_second"));
    }

    #[test]
    fn working_directory_reports_become_paths() {
        assert_eq!(path_of("file://host/tmp/a%20b"), "/tmp/a b");
        assert_eq!(path_of("file:///srv"), "/srv");
        assert_eq!(path_of("/plain"), "/plain");
        assert_eq!(path_of("file://h/x%2"), "/x%2");
    }

    #[test]
    fn groups_become_fields() {
        let mut s = line_spec(r"^(FAILED|ok) (?P<test>\S+)");
        let r = compile(&s).expect("valid");
        let line = "FAILED a::b";
        let g = r
            .line
            .as_ref()
            .expect("present")
            .captures(line)
            .expect("present");
        let f = r.fields_of(line, &g);
        // Named groups only, when there are any.
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].name.as_str(), f[0].value.as_str()), ("test", "a::b"));
        s.capture = Some(api::CollectCapture {
            fields: vec![api::CollectField {
                name: "kind".into(),
                value: "$1 in ${test} $$".into(),
            }],
            ..api::CollectCapture::default()
        });
        let r = compile(&s).expect("valid");
        let f = r.fields_of(line, &g);
        assert_eq!(f[1].value, "FAILED in a::b $");
        // Numbered groups when there are no names and no templates.
        let r = compile(&line_spec(r"^(\w+) (\d+)")).expect("valid");
        let g = r
            .line
            .as_ref()
            .expect("present")
            .captures("x 12")
            .expect("present");
        let f = r.fields_of("x 12", &g);
        assert_eq!(
            f.iter()
                .map(|f| (f.name.as_str(), f.value.as_str()))
                .collect::<Vec<_>>(),
            [("1", "x"), ("2", "12")]
        );
    }

    #[test]
    fn a_json_line_becomes_fields() {
        let mut s = line_spec(r"^\{");
        s.capture = Some(api::CollectCapture {
            json: true,
            ..api::CollectCapture::default()
        });
        let r = compile(&s).expect("valid");
        let line = r#"{"level":"error","code":7}"#;
        let g = r
            .line
            .as_ref()
            .expect("present")
            .captures(line)
            .expect("present");
        let mut f: Vec<(String, String)> = r
            .fields_of(line, &g)
            .into_iter()
            .map(|f| (f.name, f.value))
            .collect();
        f.sort();
        assert_eq!(
            f,
            [
                ("code".to_owned(), "7".to_owned()),
                ("level".to_owned(), "error".to_owned())
            ]
        );
    }
}
