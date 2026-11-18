//! Act levels and approvals: how far an agent's grant may act on panes.
//!
//! A grant with a level (`observe`, `own`, `elsewhere`) is checked here
//! after the ordinary grant check, for requests that act on panes: input,
//! leases, layout actions, jobs, and runs.
//!
//! - **observe** never acts.
//! - **own** acts in panes this connection created (split, new window,
//!   job, run). Creating a window needs no pane.
//! - **elsewhere** also acts in other panes once the user approves: the
//!   request waits while a pending approval is answered with
//!   `approvals.answer`, and is refused when it is denied or nobody
//!   answers in time. An approval for the pane covers later actions there,
//!   except closing, which always asks.
//!
//! Ownership and approvals belong to the connection and end with it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use api::{ActLevel, ApprovalDecision, Request, RpcError, code};
use tokio::sync::oneshot;

use crate::service::State;

/// How long a request waits for the user.
const TIMEOUT: Duration = Duration::from_secs(60);

fn timeout() -> Duration {
    std::env::var("DREAMER_TEST_APPROVAL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(TIMEOUT, Duration::from_millis)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn forbidden(why: String) -> RpcError {
    RpcError::new(code::FORBIDDEN, why)
}

struct Pending {
    approval: api::Approval,
    conn: u64,
    answer: oneshot::Sender<ApprovalDecision>,
}

#[derive(Default)]
struct Inner {
    /// Panes each connection created.
    owned: HashMap<u64, HashSet<u32>>,
    /// Panes the user approved for each connection ("always for this pane").
    allowed: HashMap<u64, HashSet<u32>>,
    pending: BTreeMap<String, Pending>,
    next: u64,
}

#[derive(Default)]
pub struct Approvals {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Approvals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approvals").finish_non_exhaustive()
    }
}

impl Approvals {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `conn` created `pane`: it owns it from now on.
    pub fn created(&self, conn: u64, pane: u32) {
        self.lock().owned.entry(conn).or_default().insert(pane);
    }

    pub fn owns(&self, conn: u64, pane: u32) -> bool {
        self.lock()
            .owned
            .get(&conn)
            .is_some_and(|o| o.contains(&pane))
    }

    /// A connection closed: what it owned and was approved for ends, and
    /// its waiting requests are denied.
    pub fn drop_conn(&self, conn: u64) {
        let mut g = self.lock();
        g.owned.remove(&conn);
        g.allowed.remove(&conn);
        g.pending.retain(|_, p| p.conn != conn);
    }

    pub fn list(&self) -> api::ApprovalList {
        api::ApprovalList {
            approvals: self
                .lock()
                .pending
                .values()
                .map(|p| p.approval.clone())
                .collect(),
        }
    }

    pub fn answer(
        &self,
        state: &State,
        id: &str,
        decision: ApprovalDecision,
    ) -> Result<api::Approval, RpcError> {
        if decision == ApprovalDecision::Unspecified {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "answer once, pane, or deny",
            ));
        }
        let p = self.lock().pending.remove(id).ok_or_else(|| {
            RpcError::new(
                code::NOT_FOUND,
                format!("no waiting approval `{id}` (answered, timed out, or its requester left)"),
            )
        })?;
        if decision == ApprovalDecision::Pane && !p.approval.closing {
            self.lock()
                .allowed
                .entry(p.conn)
                .or_default()
                .extend(p.approval.panes.iter().copied());
        }
        answered(state, &p.approval, decision.name());
        // The requester may have gone; the answer is recorded either way.
        let _ = p.answer.send(decision);
        Ok(p.approval)
    }

    /// Ask the user and wait for the answer, or the timeout.
    async fn ask(
        &self,
        state: &State,
        conn: u64,
        requester: &str,
        method: &str,
        act: &Act,
        panes: Vec<u32>,
    ) -> ApprovalDecision {
        let wait = timeout();
        let (tx, rx) = oneshot::channel();
        let created = now_ms();
        let approval = {
            let mut g = self.lock();
            g.next += 1;
            let approval = api::Approval {
                id: format!("a{}", g.next),
                requester: requester.to_owned(),
                method: method.to_owned(),
                action: act.summary.clone(),
                panes,
                closing: act.closing,
                created_unix_ms: created,
                expires_unix_ms: created + wait.as_millis() as u64,
            };
            g.pending.insert(
                approval.id.clone(),
                Pending {
                    approval: approval.clone(),
                    conn,
                    answer: tx,
                },
            );
            approval
        };
        state.events.push(api::DaemonEvent {
            kind: crate::events::kind::APPROVAL_REQUESTED.into(),
            pane: approval.panes.first().copied(),
            subject: Some(approval.id.clone()),
            state: Some(approval.action.clone()),
            ..api::DaemonEvent::default()
        });
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(d)) => d,
            // Its connection closed: nobody waits for it any more.
            Ok(Err(_)) => ApprovalDecision::Deny,
            Err(_) => {
                if self.lock().pending.remove(&approval.id).is_some() {
                    answered(state, &approval, "timeout");
                }
                ApprovalDecision::Unspecified
            }
        }
    }
}

fn answered(state: &State, a: &api::Approval, how: &str) {
    state.events.push(api::DaemonEvent {
        kind: crate::events::kind::APPROVAL_ANSWERED.into(),
        pane: a.panes.first().copied(),
        subject: Some(a.id.clone()),
        state: Some(how.to_owned()),
        ..api::DaemonEvent::default()
    });
}

/// What a request does to panes.
#[derive(Debug, PartialEq, Eq)]
struct Act {
    /// The panes it acts on; empty when it only creates.
    panes: Vec<u32>,
    /// It closes panes or ends a program.
    closing: bool,
    /// Taking a lease: allowed at `elsewhere` without asking, since the
    /// input sent with it asks.
    lease: bool,
    summary: String,
}

fn act(panes: Vec<u32>, summary: String) -> Option<Act> {
    Some(Act {
        panes,
        closing: false,
        lease: false,
        summary,
    })
}

/// The panes of every window matching `which`, from the current topology.
async fn panes_of(
    state: &State,
    which: impl Fn(&api::Window) -> bool,
) -> Result<Vec<u32>, RpcError> {
    let topology = match state.endpoint.lock().await.as_ref() {
        Some(ep) => workspace_host::topology(&ep.name, &ep.tracker),
        None => return Err(RpcError::new(code::UNAVAILABLE, "tmux is not connected")),
    };
    Ok(topology
        .windows
        .iter()
        .filter(|w| which(w))
        .flat_map(|w| w.panes.iter().copied())
        .collect())
}

/// A page's address as an approval names it: at most 120 characters.
fn address(url: &str) -> String {
    match url.char_indices().nth(120) {
        Some((at, _)) => format!("{}…", &url[..at]),
        None => url.to_owned(),
    }
}

/// Whether `r` acts on panes, and how. `None` for requests that only
/// observe, or that the level does not govern.
async fn act_of(state: &State, r: &Request) -> Result<Option<Act>, RpcError> {
    use api::action::Kind;
    Ok(match r {
        Request::TerminalInput(p) => act(vec![p.pane], format!("type into pane %{}", p.pane)),
        Request::LeasesAcquire(p) => Some(Act {
            panes: vec![p.pane],
            closing: false,
            lease: true,
            summary: format!("hold the input lease on pane %{}", p.pane),
        }),
        Request::JobsStart(_) => act(Vec::new(), "start a job in a new window".into()),
        Request::AppsLaunch(p) => match p.split {
            Some(pane) => act(
                vec![pane],
                format!("split pane %{pane} to open the app {}", p.app_id),
            ),
            None => act(
                Vec::new(),
                format!("open the app {} in a new window", p.app_id),
            ),
        },
        Request::WebOpen(p) => match p.pane {
            Some(pane) => act(
                vec![pane],
                format!("split pane %{pane} to open {}", address(&p.url)),
            ),
            None => act(
                Vec::new(),
                format!("open {} in a new window", address(&p.url)),
            ),
        },
        Request::WebNavigate(p) => act(
            vec![p.pane],
            format!("load {} in web pane %{}", address(&p.url), p.pane),
        ),
        Request::WebClick(p) => act(
            vec![p.pane],
            format!("click in the page in pane %{}", p.pane),
        ),
        Request::WebType(p) => act(
            vec![p.pane],
            format!("type into the page in pane %{}", p.pane),
        ),
        Request::JobsCancel(j) => {
            let pane = crate::jobs::get(&j.id)?.pane;
            Some(Act {
                panes: vec![pane],
                closing: true,
                lease: false,
                summary: format!("cancel job {} in pane %{pane}", j.id),
            })
        }
        Request::TerminalRun(p) => match p.split {
            Some(pane) => act(
                vec![pane],
                format!("split pane %{pane} to run {}", p.argv.join(" ")),
            ),
            None => act(Vec::new(), "run a program in a new window".into()),
        },
        Request::ActionsSubmit(s) => {
            let Some(kind) = s.action.as_ref().and_then(|a| a.kind.as_ref()) else {
                return Ok(None);
            };
            let one = |pane: u32, what: &str| act(vec![pane], format!("{what} pane %{pane}"));
            match kind {
                Kind::SplitPane(a) => one(a.pane, "split"),
                Kind::ResizePane(a) => one(a.pane, "resize"),
                Kind::SelectPane(a) => one(a.pane, "select"),
                Kind::ZoomPane(a) => one(a.pane, "zoom"),
                Kind::SendInput(a) => one(a.pane, "send input to"),
                Kind::SendKeys(a) => one(a.pane, "send keys to"),
                Kind::KillPane(a) => Some(Act {
                    panes: vec![a.pane],
                    closing: true,
                    lease: false,
                    summary: format!("close pane %{}", a.pane),
                }),
                Kind::MovePane(a) => {
                    let mut panes = vec![a.pane];
                    if let Some(w) = a.window {
                        panes.extend(panes_of(state, |x| x.id == w).await?);
                    }
                    act(panes, format!("move pane %{}", a.pane))
                }
                Kind::NewWindow(_) => act(Vec::new(), "open a new window".into()),
                Kind::NewSession(_) => act(Vec::new(), "start a new session".into()),
                Kind::SelectWindow(a) => act(
                    panes_of(state, |x| x.id == a.window).await?,
                    format!("select window @{}", a.window),
                ),
                Kind::RenameWindow(a) => act(
                    panes_of(state, |x| x.id == a.window).await?,
                    format!("rename window @{} to {:?}", a.window, a.name),
                ),
                Kind::KillWindow(a) => Some(Act {
                    panes: panes_of(state, |x| x.id == a.window).await?,
                    closing: true,
                    lease: false,
                    summary: format!("close window @{}", a.window),
                }),
                Kind::RenameSession(a) => act(
                    panes_of(state, |x| x.sessions.contains(&a.session)).await?,
                    format!("rename session ${} to {:?}", a.session, a.name),
                ),
            }
        }
        _ => None,
    })
}

/// Check `r` against the act level of `grant`, asking the user when the
/// level allows that. `Ok` means it may run.
pub async fn gate(
    state: &State,
    conn: u64,
    requester: &str,
    grant: &api::Grant,
    method: &str,
    r: &Request,
) -> Result<(), RpcError> {
    let level = ActLevel::try_from(grant.level).unwrap_or(ActLevel::Unspecified);
    if grant.owner || level == ActLevel::Unspecified {
        return Ok(());
    }
    let Some(act) = act_of(state, r).await? else {
        return Ok(());
    };
    if level == ActLevel::Observe {
        return Err(forbidden(format!(
            "this connection only observes; it may not {}",
            act.summary
        )));
    }
    let foreign: Vec<u32> = {
        let g = state.approvals.lock();
        let owned = g.owned.get(&conn);
        act.panes
            .iter()
            .copied()
            .filter(|p| !owned.is_some_and(|o| o.contains(p)))
            .collect()
    };
    let Some(&first) = foreign.first() else {
        return Ok(());
    };
    if level == ActLevel::Own {
        return Err(forbidden(format!(
            "pane %{first} was not created by this connection, which acts only in its own panes"
        )));
    }
    if act.lease {
        return Ok(());
    }
    if !act.closing {
        let g = state.approvals.lock();
        let allowed = g.allowed.get(&conn);
        if foreign
            .iter()
            .all(|p| allowed.is_some_and(|a| a.contains(p)))
        {
            return Ok(());
        }
    }
    match state
        .approvals
        .ask(state, conn, requester, method, &act, foreign)
        .await
    {
        ApprovalDecision::Once | ApprovalDecision::Pane => Ok(()),
        ApprovalDecision::Deny => Err(forbidden(format!(
            "the user did not approve: {}",
            act.summary
        ))),
        ApprovalDecision::Unspecified => Err(forbidden(format!(
            "no answer in time, so it is denied: {}",
            act.summary
        ))),
    }
}

/// Whether `wanted`'s level is within `current`'s: a grant may lower its
/// level, never raise it. No level is above every level.
pub fn level_within(current: i32, wanted: i32) -> bool {
    let rank = |l: i32| match ActLevel::try_from(l) {
        Ok(ActLevel::Observe) => 1,
        Ok(ActLevel::Own) => 2,
        Ok(ActLevel::Elsewhere) => 3,
        _ => 4,
    };
    rank(wanted) <= rank(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_lowers_but_never_rises() {
        let (none, observe, own, elsewhere) = (
            ActLevel::Unspecified as i32,
            ActLevel::Observe as i32,
            ActLevel::Own as i32,
            ActLevel::Elsewhere as i32,
        );
        assert!(level_within(none, elsewhere));
        assert!(level_within(elsewhere, own));
        assert!(level_within(own, observe));
        assert!(!level_within(observe, own));
        assert!(!level_within(own, elsewhere));
        assert!(!level_within(elsewhere, none));
    }
}
