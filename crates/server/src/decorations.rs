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

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use api::Decoration;

/// Decorations kept per pane; the oldest go first.
pub const PER_PANE: usize = 500;

#[derive(Default)]
struct Pane {
    generation: u64,
    cols: u32,
    marks: VecDeque<Decoration>,
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
            .retain(|m| !(m.row == d.row && m.style == d.style && m.text == d.text));
        p.marks.push_back(d);
        while p.marks.len() > PER_PANE {
            p.marks.pop_front();
        }
    }

    /// The pane's marks still valid against what the mirror shows now:
    /// `generation`, `cols`, and `history` (every history line, oldest
    /// first, then the screen). Invalid ones are dropped; the second value
    /// says whether any were.
    pub fn valid(
        &self,
        pane: u32,
        generation: u64,
        cols: u32,
        lines: &[String],
    ) -> (Vec<Decoration>, bool) {
        let Ok(mut panes) = self.panes.lock() else {
            return (Vec::new(), false);
        };
        let Some(p) = panes.get_mut(&pane) else {
            return (Vec::new(), false);
        };
        let before = p.marks.len();
        if p.generation != generation || p.cols != cols {
            p.marks.clear();
        } else {
            p.marks.retain(|m| {
                usize::try_from(m.row)
                    .ok()
                    .and_then(|r| lines.get(r))
                    .is_some_and(|l| *l == m.text)
            });
        }
        let dropped = p.marks.len() != before;
        (p.marks.iter().cloned().collect(), dropped)
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
        let (v, dropped) = d.valid(1, 7, 80, &lines(&["x", "ERR a", "y", "ERR b"]));
        assert_eq!((v.len(), dropped), (2, false));
        // History at its limit shifted by one: both lines moved.
        let (v, dropped) = d.valid(1, 7, 80, &lines(&["ERR a", "y", "ERR b", "z"]));
        assert_eq!((v.len(), dropped), (0, true));
    }

    #[test]
    fn a_respawn_or_reflow_drops_every_mark() {
        let d = Decorations::default();
        d.add(mark(0, "a"));
        let (v, _) = d.valid(1, 7, 100, &lines(&["a"]));
        assert!(v.is_empty(), "resized");
        d.add(mark(0, "a"));
        let (v, _) = d.valid(1, 8, 80, &lines(&["a"]));
        assert!(v.is_empty(), "respawned");
        // A mark for the new generation replaces the old ones.
        d.add(mark(0, "a"));
        let mut newer = mark(1, "b");
        newer.generation = 8;
        d.add(newer);
        let (v, _) = d.valid(1, 8, 80, &lines(&["a", "b"]));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn the_store_is_bounded_per_pane() {
        let d = Decorations::default();
        let all: Vec<String> = (0..PER_PANE + 10).map(|i| format!("l{i}")).collect();
        for (i, l) in all.iter().enumerate() {
            d.add(mark(i as u64, l));
        }
        let (v, _) = d.valid(1, 7, 80, &all);
        assert_eq!(v.len(), PER_PANE);
        assert_eq!(v[0].text, "l10", "the oldest went first");
    }
}
