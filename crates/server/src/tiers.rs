//! The tier policy (spec 16.6): panes that nothing follows go Cold on
//! their own when they have been quiet for `tiers.cold_after`, or when
//! there are more than `tiers.warm_limit` of them. One check for the whole
//! host, never a timer per pane.

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
    format!("{quiet}, {limit}")
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
        if policy.cold_after.is_none() && policy.warm_limit.is_none() {
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
        if d.quiet + d.over_limit > 0 {
            log::info(format!(
                "tiers: {} quiet and {} over the Warm limit went Cold ({})",
                d.quiet,
                d.over_limit,
                describe(&policy)
            ));
        }
    }
}
