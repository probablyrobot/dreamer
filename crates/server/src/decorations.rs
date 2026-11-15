//! Decorations (spec 12.8): marks filters put on lines of a pane, for
//! clients to draw as an overlay. They are data in a bounded store, never
//! part of the terminal stream.
//!
//! A decoration names its line by the row counted from the top of the
//! pane's history, with the line's text, the pane generation, and the
//! width. It stays valid while all of these hold: a respawn (new
//! generation) or a resize (reflow) drops every decoration of the pane,
//! and a line that scrolled out of history or moved (the history is at its
//! limit) is dropped when the pane's decorations are next listed.
//!
//! The filter that makes a mark reads the pane's output a moment before the
//! daemon's mirror has applied it. So a fresh mark whose row the mirror has
//! not reached, or whose screen row does not show its line yet, is pending:
//! kept but not listed, for up to [`SETTLE`]. Listing waits that long for
//! it. A line in history no longer changes, so a mismatch there drops the
//! mark at once.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use api::Decoration;

/// Decorations kept per pane; the oldest go first.
pub const PER_PANE: usize = 500;

/// How long a fresh mark may wait for the mirror to show its line.
pub const SETTLE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Pane {
    generation: u64,
    cols: u32,
    /// Each mark with when it was made.
    marks: VecDeque<(Decoration, Instant)>,
}

/// A pane's marks checked against its mirror.
#[derive(Debug, Default)]
pub struct Checked {
    /// The marks whose line the mirror shows, to list.
    pub listed: Vec<Decoration>,
    /// Some marks were dropped: their line changed or went.
    pub dropped: bool,
    /// Some fresh marks wait for the mirror to show their line.
    pub pending: bool,
}

#[derive(Default)]
pub struct Decorations {
    panes: Mutex<HashMap<u32, Pane>>,
    next: AtomicU64,
}

impl std::fmt::Debug for Decorations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decorations").finish_non_exhaustive()
    }
}

impl Decorations {
    /// Add a mark. A different generation or width than the pane's other
    /// marks replaces them all first.
    pub fn add(&self, mut d: Decoration) {
        d.id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let Ok(mut panes) = self.panes.lock() else {
            return;
        };
        let p = panes.entry(d.pane).or_default();
        if p.generation != d.generation || p.cols != d.cols {
            p.marks.clear();
            p.generation = d.generation;
            p.cols = d.cols;
        }
        // One mark per line and style.
        p.marks
            .retain(|(m, _)| !(m.row == d.row && m.style == d.style && m.text == d.text));
        p.marks.push_back((d, Instant::now()));
        while p.marks.len() > PER_PANE {
            p.marks.pop_front();
        }
    }

    /// The pane's marks checked against what the mirror shows now:
    /// `generation`, `cols`, `lines` (every history line, oldest first,
    /// then the screen), and how many of those lines are the screen.
    pub fn check(
        &self,
        pane: u32,
        generation: u64,
        cols: u32,
        lines: &[String],
        screen_rows: usize,
    ) -> Checked {
        self.check_at(pane, generation, cols, lines, screen_rows, Instant::now())
    }

    fn check_at(
        &self,
        pane: u32,
        generation: u64,
        cols: u32,
        lines: &[String],
        screen_rows: usize,
        now: Instant,
    ) -> Checked {
        let Ok(mut panes) = self.panes.lock() else {
            return Checked::default();
        };
        let Some(p) = panes.get_mut(&pane) else {
            return Checked::default();
        };
        let before = p.marks.len();
        if p.generation != generation || p.cols != cols {
            p.marks.clear();
            return Checked {
                dropped: before > 0,
                ..Checked::default()
            };
        }
        let screen_top = lines.len().saturating_sub(screen_rows);
        let mut out = Checked::default();
        p.marks.retain(|(m, made)| {
            let row = usize::try_from(m.row).unwrap_or(usize::MAX);
            if lines.get(row).is_some_and(|l| *l == m.text) {
                out.listed.push(m.clone());
                return true;
            }
            // Not shown (yet): below the mirror's last line, or on a
            // screen row still being drawn. A fresh mark waits; a line in
            // history, or a mark that waited long enough, goes.
            let unsettled = row >= lines.len() || row >= screen_top;
            let fresh = now.saturating_duration_since(*made) < SETTLE;
            if unsettled && fresh {
                out.pending = true;
                true
            } else {
                false
            }
        });
        out.dropped = p.marks.len() != before;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(row: u64, text: &str) -> Decoration {
        Decoration {
            pane: 1,
            generation: 7,
            row,
            text: text.into(),
            style: api::DecorationStyle::Highlight as i32,
            cols: 80,
            ..Decoration::default()
        }
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn marks_follow_their_line_and_go_when_it_does() {
        let d = Decorations::default();
        d.add(mark(1, "ERR a"));
        d.add(mark(3, "ERR b"));
        d.add(mark(3, "ERR b")); // the same mark once
        let c = d.check(1, 7, 80, &lines(&["x", "ERR a", "y", "ERR b"]), 0);
        assert_eq!((c.listed.len(), c.dropped), (2, false));
        // History at its limit shifted by one: both lines moved.
        let c = d.check(1, 7, 80, &lines(&["ERR a", "y", "ERR b", "z"]), 0);
        assert_eq!((c.listed.len(), c.dropped), (0, true));
    }

    #[test]
    fn a_fresh_mark_waits_for_the_mirror_to_show_its_line() {
        let d = Decorations::default();
        d.add(mark(2, "ERR c"));
        // The mirror has not reached row 2 yet: kept, not listed.
        let c = d.check(1, 7, 80, &lines(&["x", "y"]), 2);
        assert_eq!((c.listed.len(), c.dropped, c.pending), (0, false, true));
        // Row 2 is on the screen but still shows the prompt (the mirror is
        // a moment behind the filter): still pending, not dropped.
        let c = d.check(1, 7, 80, &lines(&["x", "y", "$ "]), 2);
        assert_eq!((c.listed.len(), c.dropped, c.pending), (0, false, true));
        let c = d.check(1, 7, 80, &lines(&["x", "y", "ERR c"]), 2);
        assert_eq!((c.listed.len(), c.dropped, c.pending), (1, false, false));
    }

    #[test]
    fn a_mark_that_never_shows_goes_after_settling_and_history_never_waits() {
        let d = Decorations::default();
        d.add(mark(2, "ERR d"));
        let later = Instant::now() + SETTLE;
        let c = d.check_at(1, 7, 80, &lines(&["x", "y", "cleared"]), 2, later);
        assert_eq!((c.listed.len(), c.dropped, c.pending), (0, true, false));
        // In history (above the screen), a different line goes at once.
        d.add(mark(0, "ERR e"));
        let c = d.check(1, 7, 80, &lines(&["other", "y", "z"]), 2);
        assert_eq!((c.listed.len(), c.dropped, c.pending), (0, true, false));
    }

    #[test]
    fn a_respawn_or_reflow_drops_every_mark() {
        let d = Decorations::default();
        d.add(mark(0, "a"));
        let c = d.check(1, 7, 100, &lines(&["a"]), 0);
        assert!(c.listed.is_empty() && c.dropped, "resized");
        d.add(mark(0, "a"));
        let c = d.check(1, 8, 80, &lines(&["a"]), 0);
        assert!(c.listed.is_empty() && c.dropped, "respawned");
        // A mark for the new generation replaces the old ones.
        d.add(mark(0, "a"));
        let mut newer = mark(1, "b");
        newer.generation = 8;
        d.add(newer);
        let c = d.check(1, 8, 80, &lines(&["a", "b"]), 0);
        assert_eq!(c.listed.len(), 1);
    }

    #[test]
    fn the_store_is_bounded_per_pane() {
        let d = Decorations::default();
        let all: Vec<String> = (0..PER_PANE + 10).map(|i| format!("l{i}")).collect();
        for (i, l) in all.iter().enumerate() {
            d.add(mark(i as u64, l));
        }
        let c = d.check(1, 7, 80, &all, 0);
        assert_eq!(c.listed.len(), PER_PANE);
        assert_eq!(c.listed[0].text, "l10", "the oldest went first");
    }
}
