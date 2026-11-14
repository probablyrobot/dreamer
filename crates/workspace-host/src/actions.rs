//! Running typed actions against one tmux endpoint (spec 12.4).
//!
//! Each action is validated against the current inventory, recorded as
//! `prepared`, recorded as `dispatching` (synced) before the command goes
//! to tmux, and then settled from what tmux shows afterwards: the reported
//! outcome is observed, not assumed from the command's success.

use std::sync::atomic::{AtomicU64, Ordering};

use api::{ActionRecord, ActionState, RpcError, SubmitAction, action::Kind, code};
use domain::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use tmux_backend::ops::{self, Launch, Size, Split};
use tmux_backend::{CommandError, Connection, InventoryTracker};

use crate::ledger::{Ledger, now_ms};

/// One tmux server the host drives.
#[derive(Debug)]
pub struct Endpoint {
    pub name: String,
    pub conn: Connection,
    pub tracker: InventoryTracker,
}

/// Fault injection for crash tests. Off in normal operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hooks {
    /// After the command reached tmux, never finish (the caller drops the
    /// future, as a crash would).
    pub hang_after_dispatch: bool,
    /// After the command reached tmux, abort the process.
    pub abort_after_dispatch: bool,
}

#[derive(Debug)]
pub struct Actions {
    ledger: Ledger,
    hooks: Hooks,
}

enum Outcome {
    Completed {
        observed: String,
        new_pane: Option<u32>,
    },
    Failed(String),
    Unknown(String),
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_id() -> String {
    format!(
        "a{:x}-{:x}-{}",
        now_ms(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn ledger_error(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(code::INTERNAL, e.to_string())
}

impl Actions {
    pub fn new(ledger: Ledger, hooks: Hooks) -> Actions {
        Actions { ledger, hooks }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn get(&self, id: &str) -> Result<ActionRecord, RpcError> {
        self.ledger
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::new(code::NOT_FOUND, format!("no action `{id}`")))
    }

    pub fn list(&self, limit: usize) -> Vec<ActionRecord> {
        self.ledger.list(limit)
    }

    /// Run one action to a settled state. Errors are for requests that are
    /// not actions at all (no kind, idempotency conflict, ledger failure);
    /// an action that fails is a record in state `failed`.
    pub async fn submit(
        &mut self,
        ep: &mut Endpoint,
        req: SubmitAction,
    ) -> Result<ActionRecord, RpcError> {
        let action = req
            .action
            .clone()
            .filter(|a| a.kind.is_some())
            .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "an action needs a kind"))?;
        if let Some(key) = &req.idempotency_key
            && let Some(prev) = self.ledger.by_key(key)
        {
            if prev.action.as_ref() == Some(&action) && prev.endpoint == ep.name {
                return Ok(prev.clone());
            }
            return Err(RpcError::new(
                code::CONFLICT,
                format!(
                    "idempotency key `{key}` was used for a different action ({})",
                    prev.id
                ),
            ));
        }
        let now = now_ms();
        let mut rec = ActionRecord {
            id: new_id(),
            action: Some(action.clone()),
            state: ActionState::Accepted as i32,
            endpoint: ep.name.clone(),
            idempotency_key: req.idempotency_key.clone(),
            created_unix_ms: now,
            updated_unix_ms: now,
            ..ActionRecord::default()
        };
        self.ledger.insert(&rec).map_err(ledger_error)?;

        let kind = action
            .kind
            .as_ref()
            .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "no kind"))?;
        if let Err(e) = ep.tracker.refresh().await {
            return self.settle(
                rec,
                Outcome::Failed(format!("could not read tmux before dispatch: {e}")),
            );
        }
        if let Err(reason) = validate(ep, kind) {
            return self.settle(rec, Outcome::Failed(reason));
        }
        rec.state = ActionState::Prepared as i32;
        rec.updated_unix_ms = now_ms();
        self.ledger.transition(&rec).map_err(ledger_error)?;
        rec.state = ActionState::Dispatching as i32;
        rec.updated_unix_ms = now_ms();
        self.ledger.transition(&rec).map_err(ledger_error)?;

        let sent = dispatch(&ep.conn, kind).await;
        if self.hooks.abort_after_dispatch {
            std::process::abort();
        }
        if self.hooks.hang_after_dispatch {
            std::future::pending::<()>().await;
        }
        let outcome = match sent {
            Ok(new_pane) => observe(ep, kind, new_pane).await,
            Err(e @ CommandError::Failed { .. }) => Outcome::Failed(ops::failure_message(&e)),
            Err(CommandError::Busy) => {
                Outcome::Failed("the tmux connection is busy; nothing was sent".into())
            }
            Err(e @ (CommandError::Timeout { .. } | CommandError::Closed)) => Outcome::Unknown(
                format!("{e}; the command may or may not have run and will not be retried"),
            ),
        };
        self.settle(rec, outcome)
    }

    fn settle(
        &mut self,
        mut rec: ActionRecord,
        outcome: Outcome,
    ) -> Result<ActionRecord, RpcError> {
        match outcome {
            Outcome::Completed { observed, new_pane } => {
                rec.state = ActionState::Completed as i32;
                rec.observed = Some(observed);
                rec.new_pane = new_pane;
            }
            Outcome::Failed(reason) => {
                rec.state = ActionState::Failed as i32;
                rec.reason = Some(reason);
            }
            Outcome::Unknown(reason) => {
                rec.state = ActionState::OutcomeUnknown as i32;
                rec.reason = Some(reason);
            }
        }
        rec.updated_unix_ms = now_ms();
        self.ledger.transition(&rec).map_err(ledger_error)?;
        Ok(rec)
    }
}

fn has_pane(ep: &Endpoint, p: u32) -> Result<(), String> {
    ep.tracker
        .inventory()
        .panes
        .contains_key(&TmuxPaneId(p))
        .then_some(())
        .ok_or_else(|| format!("pane %{p} does not exist on endpoint `{}`", ep.name))
}

fn has_window(ep: &Endpoint, w: u32) -> Result<(), String> {
    ep.tracker
        .inventory()
        .windows
        .contains_key(&TmuxWindowId(w))
        .then_some(())
        .ok_or_else(|| format!("window @{w} does not exist on endpoint `{}`", ep.name))
}

fn generation_holds(ep: &Endpoint, pane: u32, expected: Option<u64>) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let now = ep.tracker.generation(TmuxPaneId(pane)).map(|g| g.0);
    if now == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "pane %{pane} now runs generation {}, not {expected}; its process was replaced, so the input was not sent",
            now.map_or("none".into(), |g| g.to_string())
        ))
    }
}

/// Checks that need no tmux round trip. Anything refused here sent nothing.
fn validate(ep: &Endpoint, kind: &Kind) -> Result<(), String> {
    match kind {
        Kind::SplitPane(a) => {
            has_pane(ep, a.pane)?;
            if a.direction == api::SplitDirection::Unspecified as i32 {
                return Err("a split needs a direction (horizontal or vertical)".into());
            }
            if a.percent.is_some() && a.cells.is_some() {
                return Err("give a split size in percent or in cells, not both".into());
            }
            if a.percent.is_some_and(|p| p == 0 || p >= 100) {
                return Err("a split percentage must be between 1 and 99".into());
            }
            if a.argv.first().is_some_and(String::is_empty) {
                return Err("the program name is empty".into());
            }
            Ok(())
        }
        Kind::ResizePane(a) => {
            has_pane(ep, a.pane)?;
            if a.width.is_none() && a.height.is_none() {
                return Err("a resize needs a width, a height, or both".into());
            }
            Ok(())
        }
        Kind::SelectPane(a) => has_pane(ep, a.pane),
        Kind::ZoomPane(a) => has_pane(ep, a.pane),
        Kind::KillPane(a) => has_pane(ep, a.pane),
        Kind::SelectWindow(a) => has_window(ep, a.window),
        Kind::KillWindow(a) => has_window(ep, a.window),
        Kind::NewWindow(a) => {
            if !ep
                .tracker
                .inventory()
                .sessions
                .contains_key(&TmuxSessionId(a.session))
            {
                return Err(format!(
                    "session ${} does not exist on endpoint `{}`",
                    a.session, ep.name
                ));
            }
            if a.name.as_deref().is_some_and(str::is_empty) {
                return Err("a window name cannot be empty".into());
            }
            if a.argv.first().is_some_and(String::is_empty) {
                return Err("the program name is empty".into());
            }
            Ok(())
        }
        Kind::RenameWindow(a) => {
            has_window(ep, a.window)?;
            if a.name.is_empty() {
                return Err("a window name cannot be empty".into());
            }
            Ok(())
        }
        Kind::RenameSession(a) => {
            if !ep
                .tracker
                .inventory()
                .sessions
                .contains_key(&TmuxSessionId(a.session))
            {
                return Err(format!(
                    "session ${} does not exist on endpoint `{}`",
                    a.session, ep.name
                ));
            }
            if a.name.is_empty() || a.name.contains([':', '.']) {
                return Err("a session name cannot be empty or contain `:` or `.`".into());
            }
            Ok(())
        }
        Kind::SendInput(a) => {
            has_pane(ep, a.pane)?;
            generation_holds(ep, a.pane, a.generation)
        }
        Kind::SendKeys(a) => {
            has_pane(ep, a.pane)?;
            if a.keys.is_empty()
                || a.keys
                    .iter()
                    .any(|k| k.is_empty() || k.contains(char::is_whitespace))
            {
                return Err("keys must be non-empty tmux key names such as Enter or C-c".into());
            }
            generation_holds(ep, a.pane, a.generation)
        }
    }
}

/// Send the typed operation. Returns a created pane, if any.
async fn dispatch(conn: &Connection, kind: &Kind) -> Result<Option<u32>, CommandError> {
    match kind {
        Kind::SplitPane(a) => {
            let split = if a.direction == api::SplitDirection::Horizontal as i32 {
                Split::Horizontal
            } else {
                Split::Vertical
            };
            let size = a.percent.map(Size::Percent).or(a.cells.map(Size::Cells));
            let launch = Launch {
                argv: a.argv.clone(),
                cwd: a.cwd.clone(),
                env: Vec::new(),
                background: false,
            };
            ops::split_pane(conn, TmuxPaneId(a.pane), split, size, &launch)
                .await
                .map(|p| Some(p.0))
        }
        Kind::ResizePane(a) => ops::resize_pane(conn, TmuxPaneId(a.pane), a.width, a.height)
            .await
            .map(|()| None),
        Kind::SelectPane(a) => ops::select_pane(conn, TmuxPaneId(a.pane))
            .await
            .map(|()| None),
        Kind::SelectWindow(a) => ops::select_window(conn, TmuxWindowId(a.window))
            .await
            .map(|()| None),
        Kind::ZoomPane(a) => ops::toggle_zoom(conn, TmuxPaneId(a.pane))
            .await
            .map(|()| None),
        Kind::RenameWindow(a) => ops::rename_window(conn, TmuxWindowId(a.window), &a.name)
            .await
            .map(|()| None),
        Kind::RenameSession(a) => ops::rename_session(conn, TmuxSessionId(a.session), &a.name)
            .await
            .map(|()| None),
        Kind::SendInput(a) => ops::send_bytes(conn, TmuxPaneId(a.pane), &a.data)
            .await
            .map(|_| None),
        Kind::SendKeys(a) => ops::send_keys(conn, TmuxPaneId(a.pane), &a.keys)
            .await
            .map(|()| None),
        Kind::KillPane(a) => ops::kill_pane(conn, TmuxPaneId(a.pane))
            .await
            .map(|()| None),
        Kind::KillWindow(a) => ops::kill_window(conn, TmuxWindowId(a.window))
            .await
            .map(|()| None),
        Kind::NewWindow(a) => {
            let launch = Launch {
                argv: a.argv.clone(),
                cwd: a.cwd.clone(),
                env: Vec::new(),
                background: false,
            };
            ops::new_window(conn, TmuxSessionId(a.session), a.name.as_deref(), &launch)
                .await
                .map(|p| Some(p.0))
        }
    }
}

fn done(observed: String) -> Outcome {
    Outcome::Completed {
        observed,
        new_pane: None,
    }
}

fn not_seen(what: String) -> Outcome {
    Outcome::Unknown(format!("tmux accepted the command but {what}"))
}

/// Read tmux back and decide what happened.
async fn observe(ep: &mut Endpoint, kind: &Kind, new_pane: Option<u32>) -> Outcome {
    if let Err(e) = ep.tracker.refresh().await {
        return Outcome::Unknown(format!("dispatched, but tmux could not be read back: {e}"));
    }
    let inv = ep.tracker.inventory();
    match kind {
        Kind::SplitPane(a) => {
            match new_pane.and_then(|p| inv.panes.get(&TmuxPaneId(p)).map(|i| (p, i))) {
                Some((p, info)) => Outcome::Completed {
                    observed: format!(
                        "created pane %{p} ({}x{}) in window @{} next to %{}",
                        info.cols, info.rows, info.window.0, a.pane
                    ),
                    new_pane: Some(p),
                },
                None => not_seen("the new pane is not in the inventory".into()),
            }
        }
        Kind::NewWindow(a) => {
            match new_pane.and_then(|p| inv.panes.get(&TmuxPaneId(p)).map(|i| (p, i))) {
                Some((p, info)) => Outcome::Completed {
                    observed: format!(
                        "created window @{} with pane %{p} in session ${}",
                        info.window.0, a.session
                    ),
                    new_pane: Some(p),
                },
                None => not_seen("the new window's pane is not in the inventory".into()),
            }
        }
        Kind::ResizePane(a) => match inv.panes.get(&TmuxPaneId(a.pane)) {
            Some(i) => done(format!("pane %{} is {}x{}", a.pane, i.cols, i.rows)),
            None => not_seen(format!("pane %{} is gone", a.pane)),
        },
        Kind::SelectPane(a) => {
            let window = inv.panes.get(&TmuxPaneId(a.pane)).map(|p| p.window);
            let active = window
                .and_then(|w| inv.windows.get(&w))
                .and_then(|w| w.active_pane);
            if active == Some(TmuxPaneId(a.pane)) {
                done(format!("pane %{} is active", a.pane))
            } else {
                not_seen(format!("pane %{} is not the active pane", a.pane))
            }
        }
        Kind::SelectWindow(a) => {
            let w = TmuxWindowId(a.window);
            if inv.sessions.values().any(|s| s.active_window == Some(w)) {
                done(format!("window @{} is active", a.window))
            } else {
                not_seen(format!("window @{} is not active in any session", a.window))
            }
        }
        Kind::ZoomPane(a) => {
            let Some(w) = inv.panes.get(&TmuxPaneId(a.pane)).map(|p| p.window) else {
                return not_seen(format!("pane %{} is gone", a.pane));
            };
            match ops::window_zoomed(&ep.conn, w).await {
                Ok(true) => done(format!("window @{} is zoomed onto %{}", w.0, a.pane)),
                Ok(false) => done(format!("window @{} is not zoomed", w.0)),
                Err(e) => Outcome::Unknown(format!(
                    "dispatched, but the zoom state could not be read: {e}"
                )),
            }
        }
        Kind::RenameWindow(a) => match inv.windows.get(&TmuxWindowId(a.window)) {
            Some(w) if w.name == a.name => {
                done(format!("window @{} is named {:?}", a.window, a.name))
            }
            Some(w) => not_seen(format!("window @{} is named {:?}", a.window, w.name)),
            None => not_seen(format!("window @{} is gone", a.window)),
        },
        Kind::RenameSession(a) => match inv.sessions.get(&TmuxSessionId(a.session)) {
            Some(s) if s.name == a.name => {
                done(format!("session ${} is named {:?}", a.session, a.name))
            }
            Some(s) => not_seen(format!("session ${} is named {:?}", a.session, s.name)),
            None => not_seen(format!("session ${} is gone", a.session)),
        },
        Kind::SendInput(a) => done(format!(
            "delivered {} bytes to pane %{}; what the program does with them is not observed",
            a.data.len(),
            a.pane
        )),
        Kind::SendKeys(a) => done(format!(
            "delivered {} keys to pane %{}; what the program does with them is not observed",
            a.keys.len(),
            a.pane
        )),
        Kind::KillPane(a) => {
            if inv.panes.contains_key(&TmuxPaneId(a.pane)) {
                not_seen(format!("pane %{} still exists", a.pane))
            } else {
                done(format!("pane %{} is gone", a.pane))
            }
        }
        Kind::KillWindow(a) => {
            if inv.windows.contains_key(&TmuxWindowId(a.window)) {
                not_seen(format!("window @{} still exists", a.window))
            } else {
                done(format!("window @{} is gone", a.window))
            }
        }
    }
}
