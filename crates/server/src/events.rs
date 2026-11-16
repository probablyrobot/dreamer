//! The daemon's event log (spec 12.1 `events`): small metadata events in
//! a bounded ring, and the subscriptions that follow it.
//!
//! Producers never wait: recording an event takes a short lock and wakes
//! the subscribers. A subscriber reads the ring at its own pace. One that
//! falls further behind than its quota, or behind the ring itself, skips
//! ahead and is told so with a gap naming the sequence range it missed;
//! it never slows the daemon or another subscriber. Events carry metadata
//! only: never terminal content.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use api::{DaemonEvent, EventGap, EventGapReason};

/// Events the ring keeps.
const CAPACITY: usize = 4096;
/// Events one batch carries at most.
const BATCH: usize = 256;
/// The quota of a subscription that names none.
pub const DEFAULT_QUOTA: u32 = 1024;

fn capacity() -> usize {
    if std::env::var_os("DREAMER_TEST_SMALL_QUEUES").is_some() {
        16
    } else {
        CAPACITY
    }
}

/// Kinds recorded so far.
pub mod kind {
    /// The local tmux topology changed; `revision` is its new revision.
    pub const TOPOLOGY: &str = "tmux.topology";
    /// An action finished; `subject` is its id, `state` its outcome.
    pub const ACTION: &str = "actions.outcome";
    /// A remote host's connection changed state; `host` names it.
    pub const HOST: &str = "hosts.state";
    /// A pane app registered; `subject` is its launch id.
    pub const APP: &str = "apps.registered";
    /// A script instance was terminated; `subject` is the script, `state`
    /// why (a budget overrun, or its connection ending).
    pub const SCRIPT_FAILED: &str = "scripts.failed";
    /// A filter with `event` matched; `subject` is the filter, `pane`, and
    /// `state` the line (at most 512 bytes).
    pub const FILTER_MATCHED: &str = "filters.matched";
    /// A filter's circuit breaker tripped; `subject` is the filter, `state`
    /// why.
    pub const FILTER_TRIPPED: &str = "filters.tripped";
    /// A pane's decorations changed; `pane`. List them again.
    pub const DECORATIONS: &str = "decorations.changed";
    /// A job started or ended; `subject` is its id, `state` its state
    /// (`running`, `succeeded`, `failed`, `killed`, `lost`), `pane`.
    pub const JOB: &str = "jobs.state";
    /// The configuration directory changed. `state` is `applied` (and
    /// `subject` the terminal theme in force) or `refused` (and `subject`
    /// why: the settings in force are kept).
    pub const CONFIG: &str = "config.changed";
}

struct Ring {
    buf: VecDeque<Arc<DaemonEvent>>,
    /// The sequence the next event gets; sequences start at 1.
    next: u64,
}

pub struct Events {
    ring: Mutex<Ring>,
    /// The newest sequence, for subscribers waiting on new events.
    latest: tokio::sync::watch::Sender<u64>,
    capacity: usize,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

impl Default for Events {
    fn default() -> Self {
        Events::with_capacity(capacity())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn lock(m: &Mutex<Ring>) -> std::sync::MutexGuard<'_, Ring> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Events {
    fn with_capacity(capacity: usize) -> Events {
        Events {
            ring: Mutex::new(Ring {
                buf: VecDeque::with_capacity(capacity),
                next: 1,
            }),
            latest: tokio::sync::watch::channel(0).0,
            capacity: capacity.max(1),
        }
    }

    /// Record `event`; its sequence and time are assigned here.
    pub fn push(&self, mut event: DaemonEvent) -> u64 {
        let seq = {
            let mut r = lock(&self.ring);
            let seq = r.next;
            r.next += 1;
            event.sequence = seq;
            event.time_unix_ms = now_ms();
            if r.buf.len() == self.capacity {
                r.buf.pop_front();
            }
            r.buf.push_back(Arc::new(event));
            seq
        };
        self.latest.send_replace(seq);
        seq
    }

    /// The oldest sequence still kept and the newest recorded. With
    /// nothing kept, the oldest is one past the newest.
    pub fn bounds(&self) -> (u64, u64) {
        let r = lock(&self.ring);
        let oldest = r.buf.front().map_or(r.next, |e| e.sequence);
        (oldest, r.next - 1)
    }

    /// Up to `max` events after `cursor`, oldest first.
    fn after(&self, cursor: u64, max: usize) -> Vec<Arc<DaemonEvent>> {
        let r = lock(&self.ring);
        let Some(first) = r.buf.front().map(|e| e.sequence) else {
            return Vec::new();
        };
        let skip = (cursor + 1).saturating_sub(first) as usize;
        r.buf.iter().skip(skip).take(max).cloned().collect()
    }

    /// Whether any kept event in `first..=last` passes `wanted`.
    fn any_in(&self, first: u64, last: u64, wanted: &dyn Fn(&DaemonEvent) -> bool) -> bool {
        let r = lock(&self.ring);
        r.buf
            .iter()
            .filter(|e| e.sequence >= first && e.sequence <= last)
            .any(|e| wanted(e))
    }

    pub fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.latest.subscribe()
    }

    pub fn topology(&self, revision: u64) {
        self.push(DaemonEvent {
            kind: kind::TOPOLOGY.into(),
            revision: Some(revision),
            ..DaemonEvent::default()
        });
    }

    pub fn action(&self, r: &api::ActionRecord, pane: Option<u32>) {
        self.push(DaemonEvent {
            kind: kind::ACTION.into(),
            subject: Some(r.id.clone()),
            state: Some(workspace_host::ledger::state_name(r.state).to_owned()),
            pane,
            ..DaemonEvent::default()
        });
    }

    pub fn host(&self, name: &str, state: api::LinkState) {
        self.push(DaemonEvent {
            kind: kind::HOST.into(),
            host: Some(name.to_owned()),
            state: Some(state.name().to_owned()),
            ..DaemonEvent::default()
        });
    }

    pub fn script_failed(&self, name: &str, why: &str) {
        self.push(DaemonEvent {
            kind: kind::SCRIPT_FAILED.into(),
            subject: Some(name.to_owned()),
            state: Some(why.to_owned()),
            ..DaemonEvent::default()
        });
    }

    pub fn filter_matched(&self, name: &str, pane: u32, line: &str) {
        let mut cut = line.len().min(512);
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        self.push(DaemonEvent {
            kind: kind::FILTER_MATCHED.into(),
            subject: Some(name.to_owned()),
            pane: Some(pane),
            state: Some(line[..cut].to_owned()),
            ..DaemonEvent::default()
        });
    }

    pub fn filter_tripped(&self, name: &str, why: &str) {
        self.push(DaemonEvent {
            kind: kind::FILTER_TRIPPED.into(),
            subject: Some(name.to_owned()),
            state: Some(why.to_owned()),
            ..DaemonEvent::default()
        });
    }

    pub fn job(&self, id: &str, state: &str, pane: Option<u32>) {
        self.push(DaemonEvent {
            kind: kind::JOB.into(),
            subject: Some(id.to_owned()),
            state: Some(state.to_owned()),
            pane,
            ..DaemonEvent::default()
        });
    }

    pub fn decorations_changed(&self, pane: u32) {
        self.push(DaemonEvent {
            kind: kind::DECORATIONS.into(),
            pane: Some(pane),
            ..DaemonEvent::default()
        });
    }

    pub fn config(&self, applied: bool, subject: &str) {
        self.push(DaemonEvent {
            kind: kind::CONFIG.into(),
            subject: Some(subject.to_owned()),
            state: Some(if applied { "applied" } else { "refused" }.into()),
            ..DaemonEvent::default()
        });
    }

    pub fn app(&self, launch: &str, pane: u32) {
        self.push(DaemonEvent {
            kind: kind::APP.into(),
            subject: Some(launch.to_owned()),
            pane: Some(pane),
            ..DaemonEvent::default()
        });
    }
}

/// What a subscription wants.
#[derive(Debug, Clone)]
pub struct Filter {
    /// Kind prefixes; empty means every kind.
    pub kinds: Vec<String>,
    /// Panes; empty means every pane and events with no pane.
    pub panes: Vec<u32>,
}

impl Filter {
    fn matches(&self, e: &DaemonEvent) -> bool {
        let kind = self.kinds.is_empty()
            || self.kinds.iter().any(|k| {
                e.kind == *k
                    || (k.ends_with('.') && e.kind.starts_with(k.as_str()))
                    || e.kind
                        .strip_prefix(k.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
            });
        let pane = self.panes.is_empty() || e.pane.is_some_and(|p| self.panes.contains(&p));
        kind && pane
    }
}

/// Whether a connection with `grant` may see `e`: its own panes only,
/// unless it holds every pane.
pub fn visible(grant: &api::Grant, e: &DaemonEvent) -> bool {
    grant.owner || grant.all_panes || e.pane.is_some_and(|p| grant.panes.contains(&p))
}

/// Where a new subscription starts, and the gap it opens with, if any.
pub fn start(
    events: &Events,
    instance: &str,
    after: Option<u64>,
    from_instance: Option<&str>,
) -> (u64, Option<EventGap>) {
    let (oldest, latest) = events.bounds();
    let Some(after) = after else {
        return (latest, None);
    };
    if from_instance.is_some_and(|i| i != instance) {
        // The cursor counts another daemon's events: whatever followed it
        // there is gone. Replay what this daemon still has.
        let gap = EventGap {
            first: 0,
            last: 0,
            reason: EventGapReason::Restarted as i32,
        };
        return (oldest - 1, Some(gap));
    }
    if after + 1 < oldest {
        let gap = EventGap {
            first: after + 1,
            last: oldest - 1,
            reason: EventGapReason::Expired as i32,
        };
        return (oldest - 1, Some(gap));
    }
    (after.min(latest), None)
}

/// One step of a subscription at `cursor`: the gap to report and the
/// events to deliver, and the new cursor. `None` when there is nothing to
/// send yet.
pub fn step(
    events: &Events,
    cursor: u64,
    quota: u32,
    filter: &Filter,
    grant: &api::Grant,
) -> (u64, Option<EventGap>, Vec<DaemonEvent>) {
    let (oldest, latest) = events.bounds();
    let wanted = |e: &DaemonEvent| filter.matches(e) && visible(grant, e);
    let mut cursor = cursor;
    let mut gap: Option<EventGap> = None;
    let mut widen = |first: u64, last: u64| {
        let g = gap.get_or_insert(EventGap {
            first,
            last,
            reason: EventGapReason::Lagging as i32,
        });
        g.first = g.first.min(first);
        g.last = g.last.max(last);
    };
    // Fell behind the ring: those events are gone, wanted or not.
    if cursor + 1 < oldest {
        widen(cursor + 1, oldest - 1);
        cursor = oldest - 1;
    }
    // Further behind than the quota: skip ahead, and report it only if a
    // skipped event was one this subscriber wanted.
    let quota = u64::from(quota.max(1));
    if latest.saturating_sub(cursor) > quota {
        let skip_to = latest - quota;
        if events.any_in(cursor + 1, skip_to, &wanted) {
            widen(cursor + 1, skip_to);
        }
        cursor = skip_to;
    }
    let batch = events.after(cursor, BATCH);
    if let Some(last) = batch.last() {
        cursor = last.sequence;
    }
    let delivered = batch
        .into_iter()
        .filter(|e| wanted(e))
        .map(|e| (*e).clone())
        .collect();
    (cursor, gap, delivered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, pane: Option<u32>) -> DaemonEvent {
        DaemonEvent {
            kind: kind.into(),
            pane,
            ..DaemonEvent::default()
        }
    }

    fn all() -> Filter {
        Filter {
            kinds: Vec::new(),
            panes: Vec::new(),
        }
    }

    fn owner() -> api::Grant {
        api::Grant {
            owner: true,
            ..api::Grant::default()
        }
    }

    #[test]
    fn a_resumed_cursor_replays_and_an_old_one_gets_an_expired_gap() {
        let log = Events::with_capacity(4);
        for _ in 0..6 {
            log.push(ev(kind::TOPOLOGY, None));
        }
        assert_eq!(log.bounds(), (3, 6));
        // Still in the ring: no gap, replay from 5.
        let (cursor, gap) = start(&log, "i", Some(4), Some("i"));
        assert_eq!((cursor, gap), (4, None));
        let (_, _, got) = step(&log, cursor, DEFAULT_QUOTA, &all(), &owner());
        assert_eq!(got.iter().map(|e| e.sequence).collect::<Vec<_>>(), [5, 6]);
        // Older than the ring: 2 is gone.
        let (cursor, gap) = start(&log, "i", Some(1), Some("i"));
        assert_eq!(cursor, 2);
        let gap = gap.expect("gap");
        assert_eq!((gap.first, gap.last), (2, 2));
        assert_eq!(gap.reason, EventGapReason::Expired as i32);
        // Another daemon's cursor: restarted, replay everything kept.
        let (cursor, gap) = start(&log, "i", Some(99), Some("other"));
        assert_eq!(cursor, 2);
        assert_eq!(gap.expect("gap").reason, EventGapReason::Restarted as i32);
    }

    #[test]
    fn a_lagging_subscriber_gets_a_gap_then_live_events() {
        let log = Events::with_capacity(8);
        for _ in 0..20 {
            log.push(ev(kind::ACTION, Some(1)));
        }
        // The subscriber last saw 2; the ring keeps 13..=20.
        let (cursor, gap, got) = step(&log, 2, 4, &all(), &owner());
        let gap = gap.expect("gap");
        assert_eq!((gap.first, gap.last), (3, 16));
        assert_eq!(gap.reason, EventGapReason::Lagging as i32);
        assert_eq!(
            got.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [17, 18, 19, 20]
        );
        assert_eq!(cursor, 20);
        log.push(ev(kind::ACTION, Some(1)));
        let (_, gap, got) = step(&log, cursor, 4, &all(), &owner());
        assert!(gap.is_none());
        assert_eq!(got[0].sequence, 21);
    }

    #[test]
    fn skipping_events_nobody_wanted_is_not_a_gap() {
        let log = Events::with_capacity(64);
        for _ in 0..10 {
            log.push(ev(kind::TOPOLOGY, None));
        }
        log.push(ev(kind::ACTION, Some(1)));
        let only_actions = Filter {
            kinds: vec!["actions".into()],
            panes: Vec::new(),
        };
        let (_, gap, got) = step(&log, 0, 2, &only_actions, &owner());
        assert!(gap.is_none());
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn filters_and_grants_choose_what_is_delivered() {
        let log = Events::with_capacity(64);
        log.push(ev(kind::ACTION, Some(3)));
        log.push(ev(kind::ACTION, Some(4)));
        log.push(ev(kind::TOPOLOGY, None));
        log.push(ev(kind::APP, Some(3)));
        let narrow = api::Grant {
            owner: false,
            panes: vec![3],
            ..api::Grant::default()
        };
        let (_, _, got) = step(&log, 0, DEFAULT_QUOTA, &all(), &narrow);
        assert_eq!(got.iter().map(|e| e.sequence).collect::<Vec<_>>(), [1, 4]);
        let f = Filter {
            kinds: vec!["actions.".into(), "tmux.topology".into()],
            panes: Vec::new(),
        };
        let (_, _, got) = step(&log, 0, DEFAULT_QUOTA, &f, &owner());
        assert_eq!(
            got.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        // "app" is not a prefix of "apps.registered" at a dot.
        let f = Filter {
            kinds: vec!["app".into()],
            panes: Vec::new(),
        };
        assert!(step(&log, 0, DEFAULT_QUOTA, &f, &owner()).2.is_empty());
    }
}
