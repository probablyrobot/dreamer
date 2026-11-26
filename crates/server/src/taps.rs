//! Taps: bounded views of panes for monitors, filters, and
//! agents, read after canonical ingestion.
//!
//! - `raw` follows the mirror's record stream. The mirror never waits for a
//!   subscriber: one that falls behind is dropped and told so, and the tap
//!   reports a gap and continues from the journal or from now.
//! - `text` and `meta` look at the mirror every interval, and only read
//!   when its sequence moved, so an idle pane costs nothing and a busy one
//!   a bounded amount. `text` reports lines the cursor has moved past
//!   ("committed"), escapes removed, and changed rows of an alternate
//!   screen.
//!
//! Nothing here can delay a pane: taps only read, at their own pace. A pane
//! with any tap counts as followed and stays out of the Cold tier: `raw`
//! as a subscriber, `text` and `meta` through a [`mirror::Watch`].

use mirror::TailResult;

/// History lines read per look; more scrolling than this between looks is
/// reported as a `window` gap.
pub const WINDOW: usize = 200;

/// Where a text tap has got to in one pane.
#[derive(Debug, Clone)]
pub struct TextState {
    generation: u64,
    /// Lines scrolled into history since the tap started.
    scrolled: i64,
    scrollback: usize,
    cursor_row: i64,
    history: Vec<String>,
    /// The alternate screen's rows while one is active.
    alt: Option<Vec<String>>,
}

/// What one look produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TextStep {
    pub lines: Vec<String>,
    /// Each line's row, counted from the top of the pane's history at this
    /// look (for decorations).
    pub rows: Vec<u64>,
    pub screen: Vec<(u32, String)>,
    /// (reason, lines missed if known)
    pub gap: Option<(&'static str, Option<u64>)>,
}

impl TextState {
    pub fn new(t: &TailResult) -> TextState {
        TextState {
            generation: t.generation.0,
            scrolled: 0,
            scrollback: t.scrollback,
            cursor_row: i64::from(t.cursor_row),
            history: t.history.clone(),
            alt: t.alt_screen.then(|| t.screen.clone()),
        }
    }

    /// Advance to `t`, returning what is new.
    pub fn step(&mut self, t: &TailResult) -> TextStep {
        let mut out = TextStep::default();
        if t.generation.0 != self.generation {
            *self = TextState::new(t);
            out.gap = Some(("restarted", None));
            return out;
        }
        if t.alt_screen {
            let before = self.alt.take().unwrap_or_default();
            for (i, row) in t.screen.iter().enumerate() {
                if before.get(i) != Some(row) && !(before.get(i).is_none() && row.is_empty()) {
                    out.screen.push((i as u32, row.clone()));
                }
            }
            self.alt = Some(t.screen.clone());
            return out;
        }
        self.alt = None;
        // How many lines joined the history since the last look, found by
        // content: the scrollback is limited in bytes, so its row count
        // near the limit says nothing about how much scrolled.
        let (shifted, beyond) = if self.history.is_empty() {
            // Nothing to align with yet: below its limit the row count is
            // exact, so it says how many lines entered the history.
            let grown = t.scrollback.saturating_sub(self.scrollback);
            (grown.max(t.history.len()) as i64, false)
        } else {
            match appended(&self.history, &t.history) {
                Some(n) => (n as i64, false),
                // No overlap: more scrolled than a window holds. Start at
                // the oldest line of the new window.
                None => (t.history.len() as i64 + self.cursor_row, true),
            }
        };
        let before = self.scrolled + self.cursor_row;
        let scrolled = self.scrolled + shifted;
        let now = scrolled + i64::from(t.cursor_row);
        if now < before {
            // The screen was cleared or the cursor moved up: nothing new
            // was committed. Follow from here.
            *self = TextState {
                scrolled,
                ..TextState::new(t)
            };
            return out;
        }
        let window = t.history.len() as i64;
        let mut missed = 0u64;
        for j in before..now {
            let row = j - scrolled;
            let line = if row >= 0 {
                t.screen.get(row as usize)
            } else {
                let i = window + row;
                if i >= 0 {
                    t.history.get(i as usize)
                } else {
                    None
                }
            };
            match line {
                Some(l) => {
                    out.lines.push(l.clone());
                    out.rows.push((t.scrollback as i64 + row).max(0) as u64);
                }
                None => missed += 1,
            }
        }
        if beyond {
            // Lines older than the window are gone; how many is unknown.
            out.gap = Some(("window", None));
        } else if missed > 0 {
            out.gap = Some(("window", Some(missed)));
        }
        self.scrolled = scrolled;
        self.scrollback = t.scrollback;
        self.cursor_row = i64::from(t.cursor_row);
        self.history = t.history.clone();
        out
    }
}

/// How many lines were appended to the history between two windows of
/// its newest lines: the new window's length minus its overlap with the
/// old one, found by content. `None` when they do not overlap at all (more
/// scrolled than a window holds). With no old window, everything is new.
fn appended(before: &[String], now: &[String]) -> Option<usize> {
    if before.is_empty() {
        return Some(now.len());
    }
    for d in 0..before.len() {
        let kept = before.len() - d;
        if kept <= now.len() && before[d..] == now[..kept] {
            return Some(now.len() - kept);
        }
    }
    None
}

/// How a tap delivers: one `taps.data` per call; `false` once the
/// connection is gone.
pub trait Deliver: Clone + Send + Sync + 'static {
    fn send(&self, d: api::TapData) -> impl std::future::Future<Output = bool> + Send;
}

fn data(tap: u64, pane: u32, generation: u64) -> api::TapData {
    api::TapData {
        tap,
        pane,
        generation,
        ..api::TapData::default()
    }
}

/// Where a tap starts on one pane. It is taken before `taps.subscribe`
/// replies, so everything the pane writes after the reply is delivered.
pub enum Begun {
    /// Subscribed to the mirror's records (`None`: the subscription failed).
    Raw(Option<RawStart>),
    /// The first look at the mirror (`None`: not readable yet), and the
    /// watch that keeps the pane out of the Cold tier while the tap looks.
    Look(Option<TailResult>, mirror::Watch),
}

pub struct RawStart {
    started: mirror::Started,
    rx: tokio::sync::mpsc::Receiver<mirror::Live>,
    ended: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

/// Take the starting point of a tap on `pane` at `level`.
pub async fn begin(host: &mirror::MirrorHost, pane: u32, level: api::TapLevel) -> Begun {
    let id = domain::TmuxPaneId(pane);
    match level {
        api::TapLevel::Raw => Begun::Raw(raw_subscribe(host, id, None).await),
        _ => {
            // Taken before the first look, so the pane cannot go Cold
            // between looks once the tap has started.
            let watch = host.watch(id);
            let first = match host.tail(id, history_for(level), None).await {
                Some(Ok(Some(t))) => Some(t),
                _ => None,
            };
            Begun::Look(first, watch)
        }
    }
}

fn history_for(level: api::TapLevel) -> usize {
    if level == api::TapLevel::Text {
        WINDOW
    } else {
        0
    }
}

/// Follow one pane at `level` from `begun` until it is gone or the
/// connection is.
pub async fn follow<D: Deliver>(
    begun: Begun,
    host: mirror::MirrorHost,
    out: D,
    tap: u64,
    pane: u32,
    level: api::TapLevel,
    interval: std::time::Duration,
) {
    match begun {
        Begun::Raw(first) => raw(first, host, out, tap, pane).await,
        Begun::Look(first, _watch) => look(first, host, out, tap, pane, level, interval).await,
    }
}

async fn raw_subscribe(
    host: &mirror::MirrorHost,
    id: domain::TmuxPaneId,
    resume: Option<(domain::PaneGeneration, u64)>,
) -> Option<RawStart> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;
    // Records held for this tap before the mirror drops it as lagging
    // (tests make it small, so falling behind needs little output).
    let capacity = std::env::var("DREAMER_TEST_TAP_QUEUE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256)
        .max(1);
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    let ended = Arc::new(AtomicU8::new(mirror::end::RUNNING));
    let sink = mirror::Sink {
        records: tx,
        client: None,
        ended: ended.clone(),
        stamp: None,
    };
    let started = host.subscribe(id, resume, sink).await.ok()?;
    Some(RawStart { started, rx, ended })
}

async fn raw<D: Deliver>(
    mut first: Option<RawStart>,
    host: mirror::MirrorHost,
    out: D,
    tap: u64,
    pane: u32,
) {
    use std::sync::atomic::Ordering;
    let id = domain::TmuxPaneId(pane);
    let mut resume: Option<(domain::PaneGeneration, u64)> = None;
    loop {
        let start = match first.take() {
            Some(s) => Some(s),
            None => raw_subscribe(&host, id, resume).await,
        };
        let Some(RawStart {
            started,
            mut rx,
            ended,
        }) = start
        else {
            let mut d = data(tap, pane, 0);
            d.ended = true;
            out.send(d).await;
            return;
        };
        let generation = started.generation;
        let mut last = match &started.start {
            mirror::Start::Checkpoint { sequence, .. } => {
                // Records were lost unless this is the first start.
                if resume.is_some() {
                    let mut d = data(tap, pane, generation.0);
                    d.gap = Some(api::TapGap {
                        reason: "lagging".into(),
                        lines: None,
                    });
                    if !out.send(d).await {
                        return;
                    }
                }
                *sequence
            }
            mirror::Start::Replay { after, records } => {
                let mut d = data(tap, pane, generation.0);
                let mut last = *after;
                for (seq, r) in records {
                    if let mirror::Record::Bytes(b) = r {
                        d.raw.extend_from_slice(b);
                    }
                    last = *seq;
                }
                if !d.raw.is_empty() && !out.send(d).await {
                    return;
                }
                last
            }
        };
        while let Some(first) = rx.recv().await {
            let mut d = data(tap, pane, generation.0);
            let mut push = |(seq, r, _): mirror::Live, d: &mut api::TapData| {
                if let mirror::Record::Bytes(b) = r {
                    d.raw.extend_from_slice(&b);
                }
                last = seq;
            };
            push(first, &mut d);
            while d.raw.len() < (64 << 10) {
                match rx.try_recv() {
                    Ok(item) => push(item, &mut d),
                    Err(_) => break,
                }
            }
            if !d.raw.is_empty() && !out.send(d).await {
                return;
            }
        }
        match ended.load(Ordering::SeqCst) {
            // Fell behind: continue from the journal if it still has what
            // was missed, or from now with a gap.
            mirror::end::LAGGED | mirror::end::REATTACHED => {
                resume = Some((generation, last));
            }
            _ => {
                let mut d = data(tap, pane, generation.0);
                d.ended = true;
                out.send(d).await;
                return;
            }
        }
    }
}

async fn look<D: Deliver>(
    mut first: Option<TailResult>,
    host: mirror::MirrorHost,
    out: D,
    tap: u64,
    pane: u32,
    level: api::TapLevel,
    interval: std::time::Duration,
) {
    let id = domain::TmuxPaneId(pane);
    let history = history_for(level);
    let mut after = None;
    let mut text: Option<TextState> = None;
    let mut meta: Option<api::TapMeta> = None;
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let looked = match first.take() {
            Some(t) => Some(Ok(Some(t))),
            None => host.tail(id, history, after).await,
        };
        let t = match looked {
            Some(Ok(Some(t))) => t,
            Some(Ok(None)) => continue,
            // Not readable right now: the mirror may be being rebuilt (a
            // reattach after output outran it). The tap ends only when the
            // pane itself is gone.
            Some(Err(_)) | None => {
                let present = host.status().await.iter().any(|m| m.pane == id);
                if present {
                    continue;
                }
                let mut d = data(tap, pane, 0);
                d.ended = true;
                out.send(d).await;
                return;
            }
        };
        after = Some(t.sequence);
        let mut d = data(tap, pane, t.generation.0);
        if level == api::TapLevel::Text {
            match &mut text {
                None => text = Some(TextState::new(&t)),
                Some(state) => {
                    let step = state.step(&t);
                    if step == TextStep::default() {
                        continue;
                    }
                    d.lines = step.lines;
                    d.rows = step.rows;
                    d.cols = u32::from(t.cols);
                    d.screen = step
                        .screen
                        .into_iter()
                        .map(|(row, text)| api::TapRow { row, text })
                        .collect();
                    d.gap = step.gap.map(|(reason, lines)| api::TapGap {
                        reason: reason.into(),
                        lines,
                    });
                }
            }
            if d.lines.is_empty() && d.screen.is_empty() && d.gap.is_none() {
                continue;
            }
        } else {
            let m = api::TapMeta {
                cols: u32::from(t.cols),
                rows: u32::from(t.rows),
                title: t.title.clone(),
                cwd: t.cwd.clone(),
                sequence: t.sequence,
                alt_screen: t.alt_screen,
            };
            if meta.as_ref() == Some(&m) {
                continue;
            }
            meta = Some(m.clone());
            d.meta = Some(m);
        }
        if !out.send(d).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::PaneGeneration;

    fn tail(scrollback: usize, history: &[&str], screen: &[&str], cursor_row: u16) -> TailResult {
        TailResult {
            generation: PaneGeneration(1),
            sequence: 0,
            scrollback,
            alt_screen: false,
            cols: 80,
            rows: screen.len() as u16,
            title: None,
            cwd: None,
            cursor_row,
            history: history.iter().map(|s| (*s).to_owned()).collect(),
            screen: screen.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn lines_are_committed_when_the_cursor_passes_them() {
        let mut s = TextState::new(&tail(0, &[], &["$ ", "", ""], 0));
        // `echo hi` ran: two lines above the cursor now.
        let step = s.step(&tail(0, &[], &["$ echo hi", "hi", "$ "], 2));
        assert_eq!(step.lines, ["$ echo hi", "hi"]);
        // The prompt line being typed on is not committed yet.
        let step = s.step(&tail(0, &[], &["$ echo hi", "hi", "$ ls"], 2));
        assert!(step.lines.is_empty());
        // Output scrolls the screen: what passed the cursor, once each.
        let step = s.step(&tail(2, &["$ echo hi", "hi"], &["$ ls", "a", "$ "], 2));
        assert_eq!(step.lines, ["$ ls", "a"]);
    }

    #[test]
    fn at_the_scrollback_limit_shifts_are_found_by_content() {
        let mut s = TextState::new(&tail(3, &["a", "b", "c"], &["d", ""], 1));
        // Two more lines scrolled, scrollback still 3.
        let step = s.step(&tail(3, &["c", "d", "e"], &["f", ""], 1));
        assert_eq!(step.lines, ["e", "f"]);
        assert_eq!(step.gap, None);
    }

    #[test]
    fn a_scrollback_count_that_wobbles_at_its_limit_changes_nothing() {
        // Limited in bytes, the row count drops while lines keep coming.
        let mut s = TextState::new(&tail(1649, &["a", "b", "c"], &["d", ""], 1));
        let step = s.step(&tail(1648, &["b", "c", "d"], &["e", ""], 1));
        assert_eq!(step.lines, ["e"]);
        assert_eq!(step.gap, None);
        // More than a window scrolled: what is there, and a gap of unknown size.
        let step = s.step(&tail(1650, &["x", "y", "z"], &["w", ""], 1));
        assert_eq!(step.lines, ["x", "y", "z", "w"]);
        assert_eq!(step.gap, Some(("window", None)));
    }

    #[test]
    fn more_than_a_window_between_looks_is_a_gap() {
        let mut s = TextState::new(&tail(0, &[], &["", ""], 0));
        let step = s.step(&tail(10, &["7", "8", "9"], &["10", ""], 1));
        assert_eq!(step.lines, ["7", "8", "9", "10"]);
        assert_eq!(step.gap, Some(("window", Some(7))));
    }

    #[test]
    fn clearing_and_restarts_are_not_output() {
        let mut s = TextState::new(&tail(0, &[], &["x", "y", ""], 2));
        let step = s.step(&tail(0, &[], &["$ ", "", ""], 0));
        assert_eq!(step, TextStep::default(), "clear");
        let step = s.step(&tail(0, &[], &["$ a", "$ ", ""], 1));
        assert_eq!(step.lines, ["$ a"]);
        let mut t = tail(0, &[], &["", ""], 0);
        t.generation = PaneGeneration(2);
        assert_eq!(s.step(&t).gap, Some(("restarted", None)));
    }

    #[test]
    fn an_alternate_screen_reports_changed_rows() {
        let mut s = TextState::new(&tail(0, &[], &["$ top", ""], 1));
        let mut t = tail(0, &[], &["cpu 3%", "mem 1G"], 0);
        t.alt_screen = true;
        assert_eq!(
            s.step(&t).screen,
            [(0, "cpu 3%".to_owned()), (1, "mem 1G".to_owned())]
        );
        t.screen[0] = "cpu 9%".into();
        assert_eq!(s.step(&t).screen, [(0, "cpu 9%".to_owned())]);
        // Back on the primary screen nothing was committed by the alt app.
        let step = s.step(&tail(0, &[], &["$ top", "$ "], 1));
        assert!(step.lines.is_empty() && step.screen.is_empty());
    }
}
