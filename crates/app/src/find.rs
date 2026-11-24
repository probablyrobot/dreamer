//! Find in a pane: the pane's own local mirror, searched off
//! the UI thread and off the thread that forwards input, with matches
//! highlighted over the grid. Typing narrows after a short pause, and a new
//! search replaces the one in progress. While the bar is open, keys go to
//! it, never to the program.
//!
//! The matches are view state, like a selection: nothing the terminal
//! holds changes. They name rows from the top of history, so they stay on
//! their lines while the view scrolls; when the history changes (new
//! output, a resize, a fresh checkpoint) the search runs again and the
//! current match is kept where it still exists.

use replica::search::{Found, Hit, Query};

/// How long typing rests before a search starts.
pub(crate) const SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

#[derive(Debug, Clone, Default)]
pub(crate) struct Find {
    pub query: String,
    /// A regular expression rather than plain text (Alt-R).
    pub regex: bool,
    pub found: Option<Found>,
    /// The current match, by index into `found.hits`.
    pub at: Option<usize>,
    /// Why the last search was refused (a bad pattern).
    pub error: Option<String>,
    /// The history's size when last searched; a change searches again.
    pub searched_rows: Option<u64>,
}

impl Find {
    pub(crate) fn query(&self) -> Query {
        Query {
            text: self.query.clone(),
            regex: self.regex,
        }
    }

    pub(crate) fn current(&self) -> Option<Hit> {
        self.found.as_ref()?.hits.get(self.at?).copied()
    }

    /// Take a search's result. The current match stays the same one if it
    /// is still there; otherwise, the newest match at or above `bottom`
    /// (the last row in view), so the first jump is to what was just seen.
    /// The match to bring into view, if it changed.
    pub(crate) fn take(&mut self, found: Found, bottom: u64) -> Option<Hit> {
        let before = self.current();
        self.error = None;
        self.searched_rows = Some(found.rows);
        let keep = before.and_then(|b| found.hits.iter().position(|h| *h == b));
        self.at = keep.or_else(|| {
            found
                .hits
                .iter()
                .rposition(|h| h.row <= bottom)
                .or((!found.hits.is_empty()).then_some(0))
        });
        self.found = Some(found);
        let now = self.current();
        (now != before).then_some(now).flatten()
    }

    /// Move to the next match: `older` goes up through history, wrapping.
    pub(crate) fn step(&mut self, older: bool) -> Option<Hit> {
        let n = self.found.as_ref()?.hits.len();
        if n == 0 {
            return None;
        }
        self.at = Some(match self.at {
            None => n - 1,
            Some(i) if older => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
        });
        self.current()
    }

    /// What the bar says after the query.
    pub(crate) fn status(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        if self.query.is_empty() {
            return String::new();
        }
        match &self.found {
            None => "…".into(),
            Some(f) if f.hits.is_empty() => {
                if f.truncated {
                    "no matches in the newest lines".into()
                } else {
                    "no matches".into()
                }
            }
            Some(f) => {
                let more = if f.truncated { "+" } else { "" };
                match self.at {
                    Some(i) => format!("{} of {}{more}", i + 1, f.hits.len()),
                    None => format!("{}{more}", f.hits.len()),
                }
            }
        }
    }

    /// The query changed: what was found no longer applies.
    pub(crate) fn edited(&mut self) {
        self.found = None;
        self.at = None;
        self.error = None;
        self.searched_rows = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(row: u64, col: u16) -> Hit {
        Hit {
            row,
            start_col: col,
            end_col: col + 3,
        }
    }

    fn found(hits: &[Hit]) -> Found {
        Found {
            hits: hits.to_vec(),
            rows: 100,
            truncated: false,
        }
    }

    #[test]
    fn the_first_match_is_the_newest_in_view_and_steps_wrap() {
        let mut f = Find {
            query: "x".into(),
            ..Find::default()
        };
        let hits = [hit(10, 0), hit(40, 2), hit(90, 1)];
        assert_eq!(f.take(found(&hits), 50), Some(hit(40, 2)));
        assert_eq!(f.status(), "2 of 3");
        assert_eq!(f.step(true), Some(hit(10, 0)));
        assert_eq!(f.step(true), Some(hit(90, 1)), "wraps to the newest");
        assert_eq!(f.step(false), Some(hit(10, 0)));
    }

    #[test]
    fn a_search_again_keeps_the_current_match_where_it_still_is() {
        let mut f = Find {
            query: "x".into(),
            ..Find::default()
        };
        f.take(found(&[hit(10, 0), hit(40, 2)]), 50);
        f.step(true);
        assert_eq!(f.current(), Some(hit(10, 0)));
        // New output: more matches, the current one unmoved.
        assert_eq!(
            f.take(found(&[hit(10, 0), hit(40, 2), hit(95, 0)]), 99),
            None,
            "no jump when the current match is still there"
        );
        assert_eq!(f.current(), Some(hit(10, 0)));
        // It scrolled out of history: the newest in view instead.
        assert_eq!(
            f.take(found(&[hit(40, 2), hit(95, 0)]), 60),
            Some(hit(40, 2))
        );
        assert_eq!(f.take(found(&[]), 60), None);
        assert_eq!(f.status(), "no matches");
    }
}
