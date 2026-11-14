//! Instrumented runs (spec 13.1): what a provider's hooks report about its
//! sessions, tied to the run process they belong to.
//!
//! A hook reports from a short-lived helper process. The daemon finds the
//! run by walking up from that helper to the nearest provider process
//! (once per session), so the helper is never taken for the run. Reports
//! map to states; a session that stops reporting becomes `stale`, and one
//! whose run is gone is not reported as running.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use api::AgentEvent;

/// Without a report for this long, a session that has not ended is stale.
fn stale_after() -> Duration {
    std::env::var("DREAMER_TEST_AGENT_STALE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(600), Duration::from_millis)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Debug, Clone)]
pub struct Session {
    pub provider: String,
    pub session_id: String,
    /// The run: pid and start, once found.
    pub run: Option<(u32, u64)>,
    pub state: &'static str,
    pub last_unix_ms: u64,
    pub cwd: Option<String>,
}

/// The state a report puts a session in; `None` keeps the current one.
pub fn state_of(event: &str, detail: Option<&str>) -> Option<&'static str> {
    Some(match event {
        "SessionStart" => "active",
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
        | "SubagentStart" | "PreCompact" => "working",
        "PermissionRequest" => "waiting_for_permission",
        "Notification" => match detail {
            Some("permission_prompt") => "waiting_for_permission",
            Some("idle_prompt") => "idle",
            _ => return None,
        },
        "Stop" => "idle",
        "SessionEnd" => "ended",
        _ => return None,
    })
}

#[derive(Default)]
pub struct Agents {
    sessions: Mutex<HashMap<(String, String), Session>>,
}

impl std::fmt::Debug for Agents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agents").finish_non_exhaustive()
    }
}

/// The run a hook helper belongs to: its nearest ancestor that is the
/// provider's process.
fn run_of(hook_pid: u32, provider: &str) -> Option<(u32, u64)> {
    let procs = procreg::snapshot().ok()?;
    let by_pid: HashMap<u32, &procreg::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut cur = by_pid.get(&hook_pid)?.ppid;
    for _ in 0..64 {
        let p = by_pid.get(&cur)?;
        if procreg::provider_of(p).is_some_and(|(prov, _, _)| prov.name() == provider) {
            return Some((p.pid, p.started_unix_s * 1000));
        }
        cur = p.ppid;
    }
    None
}

impl Agents {
    /// Record a report. Finding the run reads the process table, so it
    /// happens once per session, off the async threads.
    pub async fn report(&self, e: AgentEvent) {
        let key = (e.provider.clone(), e.session_id.clone());
        let known = self
            .sessions
            .lock()
            .ok()
            .and_then(|s| s.get(&key).and_then(|x| x.run));
        let run = match known {
            Some(r) => Some(r),
            None => {
                let (pid, provider) = (e.hook_pid, e.provider.clone());
                tokio::task::spawn_blocking(move || run_of(pid, &provider))
                    .await
                    .ok()
                    .flatten()
            }
        };
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        let s = sessions.entry(key).or_insert_with(|| Session {
            provider: e.provider.clone(),
            session_id: e.session_id.clone(),
            run: None,
            state: "active",
            last_unix_ms: 0,
            cwd: None,
        });
        s.run = s.run.or(run);
        if let Some(st) = state_of(&e.event, e.detail.as_deref()) {
            s.state = st;
        }
        s.last_unix_ms = now_ms();
        if e.cwd.is_some() {
            s.cwd = e.cwd;
        }
        // Ended sessions are forgotten after a while; a bound on the rest.
        if sessions.len() > 1000 {
            let cutoff = now_ms().saturating_sub(24 * 3600 * 1000);
            sessions.retain(|_, s| s.last_unix_ms > cutoff);
        }
    }

    /// The session reporting for the run (pid, start), with its state as
    /// of now: `stale` when its reports stopped before it ended.
    pub fn for_run(&self, provider: &str, pid: u32, started_unix_ms: u64) -> Option<Session> {
        let sessions = self.sessions.lock().ok()?;
        let mut s = sessions
            .values()
            .filter(|s| {
                s.provider == provider
                    && s.run
                        .is_some_and(|(p, st)| p == pid && st.abs_diff(started_unix_ms) <= 1000)
            })
            .max_by_key(|s| s.last_unix_ms)?
            .clone();
        if s.state != "ended"
            && now_ms().saturating_sub(s.last_unix_ms) > stale_after().as_millis() as u64
        {
            s.state = "stale";
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_map_to_states_and_others_change_nothing() {
        assert_eq!(state_of("SessionStart", None), Some("active"));
        assert_eq!(state_of("PreToolUse", None), Some("working"));
        assert_eq!(
            state_of("Notification", Some("permission_prompt")),
            Some("waiting_for_permission")
        );
        assert_eq!(state_of("Notification", Some("auth_success")), None);
        assert_eq!(state_of("Stop", None), Some("idle"));
        assert_eq!(state_of("SessionEnd", Some("logout")), Some("ended"));
        assert_eq!(state_of("FileChanged", None), None);
    }
}
