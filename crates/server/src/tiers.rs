//! The tier policy (spec 16.6): panes that nothing follows go Cold on
//! their own when they have been quiet for `tiers.cold_after`, or when
//! there are more than `tiers.warm_limit` of them, or when the host's
//! mirrors together hold more than `tiers.warm_memory`. One check for the
//! whole host, never a timer per pane.

use std::sync::Arc;
use std::time::Duration;

use crate::log;
use crate::service::State;

/// How often the policy runs: every 10 s, or more often when the quiet
/// time is shorter, so a pane goes Cold within half its quiet time late.
fn every(policy: &mirror::TierPolicy) -> Duration {
    let ten = Duration::from_secs(10);
    policy
        .cold_after
        .map_or(ten, |d| (d / 2).clamp(Duration::from_millis(500), ten))
}

pub fn describe(p: &mirror::TierPolicy) -> String {
    let quiet = p.cold_after.map_or_else(
        || "never for quiet panes".to_owned(),
        |d| format!("Cold after {} s quiet", d.as_secs()),
    );
    let limit = p.warm_limit.map_or_else(
        || "no Warm limit".to_owned(),
        |n| format!("at most {n} unfollowed Warm panes"),
    );
    let memory = p.warm_memory.map_or_else(
        || "no Warm memory ceiling".to_owned(),
        |b| {
            if b >= 1 << 20 {
                format!("mirrors within {} MiB", b >> 20)
            } else {
                format!("mirrors within {} KiB", b >> 10)
            }
        },
    );
    format!("{quiet}, {limit}, {memory}")
}

pub async fn run(state: Arc<State>) {
    loop {
        let policy = *state.tiers.lock().await;
        tokio::time::sleep(every(&policy)).await;
        if state
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let policy = *state.tiers.lock().await;
        if policy.cold_after.is_none()
            && policy.warm_limit.is_none()
            && policy.warm_memory.is_none()
        {
            continue;
        }
        let host = state.mirrors.read().await.clone();
        let Some(host) = host else {
            continue;
        };
        // Harness drawers count as followed while they exist, so a hidden
        // one is never put to sleep. When the endpoint is busy, skip this
        // round rather than demote without knowing them.
        let keep = match state.endpoint.try_lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|ep| {
                    ep.tracker
                        .inventory()
                        .panes
                        .iter()
                        .filter(|(_, p)| p.drawer_for.is_some())
                        .map(|(id, _)| *id)
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => continue,
        };
        let d = host.apply_policy(policy, &keep).await;
        if d.quiet + d.over_limit + d.over_memory > 0 {
            log::info(format!(
                "tiers: {} quiet, {} over the Warm limit, and {} over the Warm memory went Cold; mirrors now hold about {} MiB ({})",
                d.quiet,
                d.over_limit,
                d.over_memory,
                d.memory >> 20,
                describe(&policy)
            ));
        }
    }
}
