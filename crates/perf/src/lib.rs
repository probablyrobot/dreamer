//! Opt-in performance diagnostics (spec 16.1, 16.4): how long each stage of
//! the terminal path takes, and counters for the traffic through it.
//!
//! Everything is off until a process turns it on ([`set_enabled`]). While
//! it is off, every hook costs one relaxed atomic load and takes no clock
//! reading. While it is on, a timing is one clock reading at each end and a
//! few relaxed atomic adds into a fixed histogram: no lock, no allocation,
//! and nothing per byte. Only durations and sizes are kept, never terminal
//! content.
//!
//! The statics are per process: the daemon's stages are collected in
//! `dreamerd`, a replica's in the process that runs it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

static ON: AtomicBool = AtomicBool::new(false);
/// When collection last started (turned on or reset), in [`now_ns`] time.
static SINCE: AtomicU64 = AtomicU64::new(0);

/// Whether diagnostics are being collected.
#[inline]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// Turn collection on or off. Turning it on keeps what was collected
/// before; [`reset`] starts again from zero.
pub fn set_enabled(enabled: bool) {
    if enabled && !ON.swap(true, Relaxed) {
        SINCE.store(now_ns(), Relaxed);
    } else if !enabled {
        ON.store(false, Relaxed);
    }
}

/// Zero every timing and counter.
pub fn reset() {
    for h in &HISTOGRAMS {
        h.clear();
    }
    for c in &COUNTERS {
        c.store(0, Relaxed);
    }
    SINCE.store(now_ns(), Relaxed);
}

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

/// Nanoseconds since the first use in this process; never 0.
pub fn now_ns() -> u64 {
    u64::try_from(base().elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

/// A stage of the terminal path, in path order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Daemon: an input request received until its bytes go to tmux
    /// (encoding against the canonical engine and the pane's input lane).
    Input,
    /// Daemon: input sent to tmux until the next read of that pane's
    /// output. tmux, the pty, and the program are all in it.
    Tmux,
    /// Daemon: one read of tmux's control output decoded into
    /// notifications.
    Decode,
    /// Daemon: decoded output until the pane's VT owner starts on it (the
    /// router, any held backlog, and the owner's queue).
    Route,
    /// Daemon: the canonical VT parse of one output chunk.
    Parse,
    /// Daemon: handing a parsed record to subscribers and the journal.
    Publish,
    /// Daemon: a record published until its batch is handed to the
    /// client connection's writer.
    Transfer,
    /// Client: applying a received record batch to the replica's engine.
    ReplicaParse,
    /// Client: extracting a frame from the replica's engine.
    Frame,
}

impl Stage {
    pub const ALL: [Stage; 9] = [
        Stage::Input,
        Stage::Tmux,
        Stage::Decode,
        Stage::Route,
        Stage::Parse,
        Stage::Publish,
        Stage::Transfer,
        Stage::ReplicaParse,
        Stage::Frame,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Input => "input",
            Stage::Tmux => "tmux",
            Stage::Decode => "decode",
            Stage::Route => "route",
            Stage::Parse => "parse",
            Stage::Publish => "publish",
            Stage::Transfer => "transfer",
            Stage::ReplicaParse => "replica_parse",
            Stage::Frame => "frame",
        }
    }
}

/// Traffic counters, counted only while collection is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// Bytes read from tmux's control output.
    TmuxBytes,
    /// Output bytes parsed by canonical mirrors.
    ParsedBytes,
    /// Records (output chunks and resizes) applied by canonical mirrors.
    Records,
    /// Terminal record bytes sent to clients, and the batches they went in.
    TransferredBytes,
    TransferredBatches,
    /// Subscriptions started from a checkpoint, and the checkpoints' bytes.
    Checkpoints,
    CheckpointBytes,
    /// Subscriptions started by replaying the journal.
    Replays,
    /// Subscriptions ended because their queue was full (dropped
    /// subscriber events).
    Lagged,
    /// Mirrors rebuilt because a pane's output outran them.
    Reattaches,
    /// Output of Cold panes, dropped before any VT work.
    ColdDroppedBytes,
}

const COUNTER_COUNT: usize = 11;

impl Counter {
    pub const ALL: [Counter; COUNTER_COUNT] = [
        Counter::TmuxBytes,
        Counter::ParsedBytes,
        Counter::Records,
        Counter::TransferredBytes,
        Counter::TransferredBatches,
        Counter::Checkpoints,
        Counter::CheckpointBytes,
        Counter::Replays,
        Counter::Lagged,
        Counter::Reattaches,
        Counter::ColdDroppedBytes,
    ];
}

static COUNTERS: [AtomicU64; COUNTER_COUNT] = [const { AtomicU64::new(0) }; COUNTER_COUNT];

/// Add `n` to a counter, if collection is on.
#[inline]
pub fn count(c: Counter, n: u64) {
    if on() {
        COUNTERS[c as usize].fetch_add(n, Relaxed);
    }
}

pub fn counter(c: Counter) -> u64 {
    COUNTERS[c as usize].load(Relaxed)
}

/// The start of a timing: a clock reading while collection is on, else
/// nothing (and no clock reading).
#[inline]
pub fn start() -> Option<Instant> {
    on().then(Instant::now)
}

/// End a timing begun with [`start`].
#[inline]
pub fn finish(stage: Stage, started: Option<Instant>) {
    if let Some(t) = started {
        record(stage, t.elapsed());
    }
}

/// Record one duration for a stage.
pub fn record(stage: Stage, d: Duration) {
    HISTOGRAMS[stage as usize].add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
}

/// When a record was handed on, for a later stage in another task to time
/// itself from. Holds the oldest mark not yet taken.
#[derive(Debug, Default)]
pub struct Stamp(AtomicU64);

impl Stamp {
    /// Mark now, unless an earlier mark is still waiting.
    #[inline]
    pub fn mark(&self) {
        if on() {
            let _ = self.0.compare_exchange(0, now_ns(), Relaxed, Relaxed);
        }
    }

    /// The time since the waiting mark, clearing it.
    #[inline]
    pub fn take(&self) -> Option<Duration> {
        if !on() {
            return None;
        }
        let t = self.0.swap(0, Relaxed);
        (t != 0).then(|| Duration::from_nanos(now_ns().saturating_sub(t)))
    }
}

/// Eight buckets for each power of two: a bucket's width is at most an
/// eighth of its lower bound, so a percentile is within 6 % of the truth.
const SUB_BITS: u32 = 3;
const SUB: u64 = 1 << SUB_BITS;
const BUCKETS: usize = ((64 - SUB_BITS + 1) * SUB as u32) as usize;

fn bucket(ns: u64) -> usize {
    if ns < SUB {
        return ns as usize;
    }
    let exp = 63 - ns.leading_zeros();
    let sub = (ns >> (exp - SUB_BITS)) & (SUB - 1);
    ((exp - SUB_BITS + 1) as u64 * SUB + sub) as usize
}

/// The lowest value in a bucket, and its width.
fn bounds(b: usize) -> (u64, u64) {
    let b = b as u64;
    if b < SUB {
        return (b, 1);
    }
    let exp = b / SUB + u64::from(SUB_BITS) - 1;
    let sub = b % SUB;
    let shift = exp - u64::from(SUB_BITS);
    ((SUB + sub) << shift, 1 << shift)
}

#[derive(Debug)]
struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    total: AtomicU64,
    max: AtomicU64,
}

static HISTOGRAMS: [Histogram; 9] = [const { Histogram::new() }; 9];

impl Histogram {
    const fn new() -> Histogram {
        Histogram {
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
            count: AtomicU64::new(0),
            total: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }

    fn add(&self, ns: u64) {
        self.buckets[bucket(ns)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.total.fetch_add(ns, Relaxed);
        self.max.fetch_max(ns, Relaxed);
    }

    fn clear(&self) {
        for b in &self.buckets {
            b.store(0, Relaxed);
        }
        self.count.store(0, Relaxed);
        self.total.store(0, Relaxed);
        self.max.store(0, Relaxed);
    }

    fn summary(&self, stage: Stage) -> Summary {
        let counts: Vec<u64> = self.buckets.iter().map(|b| b.load(Relaxed)).collect();
        let max = self.max.load(Relaxed);
        let n: u64 = counts.iter().sum();
        let at = |p: f64| -> u64 {
            if n == 0 {
                return 0;
            }
            // The rank of the p-th value, 1-based.
            let rank = ((n as f64 * p).ceil() as u64).clamp(1, n);
            let mut seen = 0;
            for (i, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= rank {
                    let (low, width) = bounds(i);
                    return (low + (width - 1) / 2).min(max);
                }
            }
            max
        };
        Summary {
            stage,
            count: n,
            total_ns: self.total.load(Relaxed),
            p50_ns: at(0.50),
            p95_ns: at(0.95),
            p99_ns: at(0.99),
            max_ns: max,
        }
    }
}

/// One stage's timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub stage: Stage,
    pub count: u64,
    pub total_ns: u64,
    /// Percentiles, from the bucket the value fell in (within 6 %).
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// Everything collected so far.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub enabled: bool,
    /// Since collection was last turned on or reset; zero if never on.
    pub collecting: Duration,
    /// Every stage, in path order; a stage with no samples has count 0.
    pub stages: Vec<Summary>,
    pub counters: Vec<(Counter, u64)>,
}

impl Snapshot {
    pub fn stage(&self, s: Stage) -> Option<&Summary> {
        self.stages.iter().find(|x| x.stage == s)
    }

    pub fn counter(&self, c: Counter) -> u64 {
        self.counters
            .iter()
            .find(|(k, _)| *k == c)
            .map_or(0, |(_, v)| *v)
    }
}

pub fn snapshot() -> Snapshot {
    let since = SINCE.load(Relaxed);
    Snapshot {
        enabled: on(),
        collecting: if since == 0 {
            Duration::ZERO
        } else {
            Duration::from_nanos(now_ns().saturating_sub(since))
        },
        stages: Stage::ALL
            .iter()
            .map(|s| HISTOGRAMS[*s as usize].summary(*s))
            .collect(),
        counters: Counter::ALL.iter().map(|c| (*c, counter(*c))).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_every_value_and_stay_within_an_eighth() {
        let mut last = 0;
        for ns in (0..100_000u64).chain([u64::MAX / 3, u64::MAX - 1, u64::MAX]) {
            let b = bucket(ns);
            assert!(b < BUCKETS, "{ns}");
            assert!(b >= last || ns > 100_000, "monotonic at {ns}");
            last = b;
            let (low, width) = bounds(b);
            assert!(low <= ns && ns - low < width, "{ns} in {low}+{width}");
            assert!(
                width == 1 || width * SUB <= low,
                "{ns}: width {width} of {low}"
            );
        }
    }

    #[test]
    fn percentiles_come_from_the_recorded_distribution() {
        let h = Histogram::new();
        // 90 fast samples around 10 µs, 10 slow ones around 1 ms.
        for i in 0..90 {
            h.add(10_000 + i);
        }
        for i in 0..10 {
            h.add(1_000_000 + i * 10);
        }
        let s = h.summary(Stage::Parse);
        assert_eq!(s.count, 100);
        let near = |v: u64, want: u64| v.abs_diff(want) * 16 <= want;
        assert!(near(s.p50_ns, 10_000), "{s:?}");
        assert!(near(s.p95_ns, 1_000_000), "{s:?}");
        assert_eq!(s.max_ns, 1_000_090);
        h.clear();
        assert_eq!(h.summary(Stage::Parse).count, 0);
        assert_eq!(h.summary(Stage::Parse).p95_ns, 0);
    }

    // The only test that touches the process-wide switch.
    #[test]
    fn nothing_is_collected_while_off() {
        assert!(!on());
        assert!(start().is_none());
        count(Counter::ParsedBytes, 10);
        let stamp = Stamp::default();
        stamp.mark();
        set_enabled(true);
        assert!(stamp.take().is_none(), "a mark made while off");
        assert_eq!(counter(Counter::ParsedBytes), 0);
        count(Counter::ParsedBytes, 10);
        let t = start();
        assert!(t.is_some());
        finish(Stage::Parse, t);
        stamp.mark();
        assert!(stamp.take().is_some());
        assert!(stamp.take().is_none(), "taken once");
        let s = snapshot();
        assert!(s.enabled);
        assert_eq!(s.counter(Counter::ParsedBytes), 10);
        assert_eq!(s.stage(Stage::Parse).map(|s| s.count), Some(1));
        set_enabled(false);
        count(Counter::ParsedBytes, 10);
        assert_eq!(counter(Counter::ParsedBytes), 10);
        reset();
        assert_eq!(snapshot().stage(Stage::Parse).map(|s| s.count), Some(0));
    }
}
