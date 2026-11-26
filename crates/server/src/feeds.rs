//! When a view subscription is fed screen frames instead of the byte
//! stream (`docs/mirrors.md`, "Screen frames").
//!
//! A subscription the client made `automatic` goes to frames once its
//! pane writes more than `above` bytes a second over `dwell`, and back to
//! the stream once it writes less than `below`. Two thresholds and a least
//! time on each feed keep a pane at the threshold from flipping. A client
//! working with the pane's history holds it on the stream, and for `hold`
//! after it lets go. Every subscription is listed for the diagnostics with
//! its feed and how often it switched.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;

/// The switching policy, from the `render.frames_*` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub above: u64,
    pub below: u64,
    pub dwell: Duration,
    pub hold: Duration,
}

impl Policy {
    pub fn of(r: &workspace_config::RenderSettings) -> Policy {
        Policy {
            above: r.frames_above,
            below: r.frames_below,
            dwell: r.frames_dwell,
            hold: r.frames_hold,
        }
    }

    /// How often a subscription that may switch looks at its rate.
    pub fn tick(&self) -> Duration {
        (self.dwell / 4).max(Duration::from_millis(5))
    }
}

impl Default for Policy {
    fn default() -> Self {
        Policy::of(&workspace_config::RenderSettings::default())
    }
}

/// The policy in force, changed live by the configuration.
pub type Shared = Arc<std::sync::RwLock<Policy>>;

pub fn current(p: &Shared) -> Policy {
    p.read().map(|p| *p).unwrap_or_default()
}

/// A byte counter's samples, to tell its rate over a window.
#[derive(Debug, Default)]
pub struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    /// Note the counter's `total` at `now`, keeping what a rate over
    /// `window` needs.
    pub fn sample(&mut self, now: Instant, total: u64, window: Duration) {
        self.samples.push_back((now, total));
        // Keep the newest sample at or before the window's start.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|(t, _)| now.saturating_duration_since(*t) >= window)
        {
            self.samples.pop_front();
        }
    }

    /// Bytes a second over the last `window` (at least), once the samples
    /// cover it.
    pub fn over(&self, window: Duration) -> Option<u64> {
        let (t1, c1) = *self.samples.back()?;
        let (t0, c0) = *self.samples.front()?;
        let span = t1.saturating_duration_since(t0);
        if span < window || span.is_zero() {
            return None;
        }
        Some((u128::from(c1.saturating_sub(c0)) * 1_000_000 / span.as_micros().max(1)) as u64)
    }
}

/// Where a subscription is, for [`decide`].
#[derive(Debug, Clone, Copy)]
pub struct Where {
    pub on_frames: bool,
    /// When it came to that feed.
    pub since: Instant,
    /// Held, or within the policy's hold of the release.
    pub held: bool,
    /// The daemon switches it by rate.
    pub automatic: bool,
    /// Not automatic: the feed it asked for (`true`: frames).
    pub asked: bool,
}

/// Which feed a subscription should be on now: `Some(true)` frames,
/// `Some(false)` the stream, `None` where it is.
///
/// - Held (or within `hold` of the release): the stream.
/// - Not automatic: the feed it asked for.
/// - Automatic: frames once `rate` is above `above`, the stream once it
///   is below `below`, and never before `dwell` on the current feed.
pub fn decide(p: &Policy, w: Where, now: Instant, rate: Option<u64>) -> Option<bool> {
    let want = if w.held {
        false
    } else if !w.automatic {
        w.asked
    } else {
        if now.saturating_duration_since(w.since) < p.dwell {
            return None;
        }
        match rate {
            Some(r) if !w.on_frames && r > p.above => true,
            Some(r) if w.on_frames && r < p.below => false,
            _ => w.on_frames,
        }
    };
    (want != w.on_frames).then_some(want)
}

/// One view subscription, as the diagnostics list it.
#[derive(Debug, Default)]
pub struct View {
    pub connection: u64,
    pub subscription: u64,
    pub pane: u32,
    pub frames: AtomicBool,
    pub automatic: AtomicBool,
    pub held: AtomicBool,
    pub switches: AtomicU64,
    pub to_frames: AtomicU64,
    pub rate: AtomicU64,
}

impl View {
    pub fn switched(&self, frames: bool) {
        self.frames.store(frames, Ordering::Relaxed);
        self.switches.fetch_add(1, Ordering::Relaxed);
        if frames {
            self.to_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
}

static VIEWS: Mutex<Vec<Weak<View>>> = Mutex::new(Vec::new());

/// List a subscription for the diagnostics while the returned handle lives.
pub fn register(view: View) -> Arc<View> {
    let v = Arc::new(view);
    if let Ok(mut all) = VIEWS.lock() {
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&v));
    }
    v
}

/// Every live view subscription.
pub fn views() -> Vec<api::ViewFeed> {
    let Ok(mut all) = VIEWS.lock() else {
        return Vec::new();
    };
    all.retain(|w| w.strong_count() > 0);
    let mut out: Vec<api::ViewFeed> = all
        .iter()
        .filter_map(Weak::upgrade)
        .map(|v| api::ViewFeed {
            subscription: v.subscription,
            connection: v.connection,
            pane: v.pane,
            feed: if v.frames.load(Ordering::Relaxed) {
                api::TerminalFeed::Frames
            } else {
                api::TerminalFeed::Stream
            } as i32,
            automatic: v.automatic.load(Ordering::Relaxed),
            held: v.held.load(Ordering::Relaxed),
            switches: v.switches.load(Ordering::Relaxed),
            to_frames: v.to_frames.load(Ordering::Relaxed),
            rate: v.rate.load(Ordering::Relaxed),
        })
        .collect();
    out.sort_by_key(|v| (v.connection, v.subscription));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(on_frames: bool, since: Instant, held: bool, automatic: bool, asked: bool) -> Where {
        Where {
            on_frames,
            since,
            held,
            automatic,
            asked,
        }
    }

    fn policy() -> Policy {
        Policy {
            above: 1 << 20,
            below: 256 << 10,
            dwell: Duration::from_secs(1),
            hold: Duration::from_secs(2),
        }
    }

    /// Output at a steady rate, sampled every tick: the feed the policy
    /// leaves the subscription on after `secs`, and how often it switched.
    fn run(rates: &[(f64, u64)]) -> (bool, u32) {
        let p = policy();
        let start = Instant::now();
        let mut now = start;
        let mut since = start;
        let mut on = false;
        let mut total = 0u64;
        let mut rate = Rate::default();
        let mut switches = 0;
        for &(secs, per_second) in rates {
            let end = now + Duration::from_secs_f64(secs);
            while now < end {
                now += p.tick();
                total += per_second * p.tick().as_millis() as u64 / 1000;
                rate.sample(now, total, p.dwell);
                if let Some(to) = decide(
                    &p,
                    at(on, since, false, true, false),
                    now,
                    rate.over(p.dwell),
                ) {
                    on = to;
                    since = now;
                    switches += 1;
                }
            }
        }
        (on, switches)
    }

    #[test]
    fn a_busy_pane_goes_to_frames_and_back_when_it_calms() {
        // `cat` of a large file: about 12 MiB/s.
        assert_eq!(run(&[(3.0, 12 << 20)]), (true, 1));
        assert_eq!(run(&[(3.0, 12 << 20), (3.0, 0)]), (false, 2));
        // Typing, a build log, a full-screen program: never.
        assert_eq!(run(&[(10.0, 300)]), (false, 0));
        assert_eq!(run(&[(10.0, 115 << 10)]), (false, 0));
    }

    #[test]
    fn a_pane_at_the_threshold_does_not_flip() {
        // Output hovering around the upper threshold: in once, and it
        // stays, since it never falls under the lower one.
        let around: Vec<(f64, u64)> = (0..40)
            .map(|i| (0.25, if i % 2 == 0 { 1_300_000 } else { 900_000 }))
            .collect();
        let (on, switches) = run(&around);
        assert!(on && switches == 1, "{switches} switches");
        // Bursts shorter than the window never switch.
        let bursts: Vec<(f64, u64)> = (0..20)
            .map(|i| (0.25, if i % 4 == 0 { 3 << 20 } else { 0 }))
            .collect();
        assert_eq!(run(&bursts), (false, 0));
    }

    #[test]
    fn a_held_pane_stays_on_the_stream_and_a_fixed_one_on_its_feed() {
        let p = policy();
        let t = Instant::now();
        let later = t + Duration::from_secs(10);
        // Held: back to the stream at once, whatever the rate or time.
        assert_eq!(
            decide(
                &p,
                at(true, later, true, true, false),
                later,
                Some(50 << 20)
            ),
            Some(false)
        );
        assert_eq!(
            decide(&p, at(false, t, true, true, false), later, Some(50 << 20)),
            None
        );
        // Not automatic: the feed asked for, once released.
        assert_eq!(
            decide(&p, at(false, later, false, false, true), later, None),
            Some(true)
        );
        assert_eq!(
            decide(&p, at(true, t, false, false, true), later, Some(0)),
            None
        );
    }
}
