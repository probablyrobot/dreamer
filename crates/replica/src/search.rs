//! Search in a pane's local mirror (spec 15.2): the text of its history
//! and screen, matched by the bounded match stage, off the thread that
//! forwards input.
//!
//! Bounds: the newest [`MAX_LINES`] rows and [`MAX_BYTES`] of text are
//! searched, at most [`MAX_HITS`] matches are kept, and a search a newer
//! one replaced stops at its next row. A result that hit a bound says so.
//! Highlights are view state; nothing the terminal holds changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use unicode_width::UnicodeWidthStr;

/// Rows searched at most, newest first kept.
pub const MAX_LINES: usize = 100_000;
/// Text searched at most, in bytes.
pub const MAX_BYTES: usize = 16 << 20;
/// Matches kept at most.
pub const MAX_HITS: usize = 5_000;

/// What to find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    /// A regular expression rather than plain text.
    pub regex: bool,
}

impl Query {
    /// Case matters only when the query has a capital letter.
    fn ignore_case(&self) -> bool {
        !self.text.chars().any(char::is_uppercase)
    }
}

/// One match: a row counted from the top of history (as the viewport's
/// offset is), and its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub row: u64,
    pub start_col: u16,
    /// One past the last cell.
    pub end_col: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub hits: Vec<Hit>,
    /// Rows in the text searched (history and screen).
    pub rows: u64,
    /// A bound cut the search short: older rows, or more matches, exist.
    pub truncated: bool,
}

/// Searches for one view: each new search supersedes the ones before it.
#[derive(Debug, Clone, Default)]
pub struct Searches(Arc<AtomicU64>);

impl Searches {
    /// A ticket for a new search; earlier ones stop.
    pub fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current(&self, ticket: u64) -> bool {
        self.0.load(Ordering::SeqCst) == ticket
    }

    /// Stop every search in progress.
    pub fn cancel(&self) {
        self.begin();
    }
}

/// Search `text` (one row per line, as the engine formats it). `None`
/// when a newer search replaced this one before it finished; an error for
/// a pattern the match stage refuses.
pub fn search(
    text: &str,
    query: &Query,
    searches: &Searches,
    ticket: u64,
) -> Result<Option<Found>, String> {
    let m = matcher::Matcher::new(&matcher::Spec {
        pattern: query.text.clone(),
        literals: Vec::new(),
        literal: !query.regex,
        ignore_case: query.ignore_case(),
    })?;
    let lines: Vec<&str> = text.split('\n').collect();
    let rows = lines.len() as u64;
    // The newest rows within both bounds.
    let mut first = lines.len().saturating_sub(MAX_LINES);
    let mut bytes = 0usize;
    for (i, l) in lines.iter().enumerate().rev() {
        bytes += l.len() + 1;
        if bytes > MAX_BYTES {
            first = first.max(i + 1);
            break;
        }
    }
    let mut found = Found {
        hits: Vec::new(),
        rows,
        truncated: first > 0,
    };
    for (i, line) in lines.iter().enumerate().skip(first) {
        if i % 256 == 0 && !searches.current(ticket) {
            return Ok(None);
        }
        let room = MAX_HITS - found.hits.len();
        for f in m.find_all(line, room) {
            let col = |at: usize| u16::try_from(line[..at].width()).unwrap_or(u16::MAX);
            found.hits.push(Hit {
                row: i as u64,
                start_col: col(f.start),
                end_col: col(f.end),
            });
        }
        if found.hits.len() >= MAX_HITS {
            found.truncated = true;
            break;
        }
    }
    Ok(searches.current(ticket).then_some(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Query {
        Query {
            text: text.into(),
            regex: false,
        }
    }

    #[test]
    fn hits_are_rows_and_cells_with_smart_case() {
        let s = Searches::default();
        let text = "error one\n日本 error\nnothing\nError, ERROR";
        let t = s.begin();
        let f = search(text, &q("error"), &s, t)
            .expect("ok")
            .expect("current");
        assert_eq!(f.rows, 4);
        assert_eq!(
            f.hits,
            [
                Hit {
                    row: 0,
                    start_col: 0,
                    end_col: 5
                },
                // Wide characters take two cells each.
                Hit {
                    row: 1,
                    start_col: 5,
                    end_col: 10
                },
                Hit {
                    row: 3,
                    start_col: 0,
                    end_col: 5
                },
                Hit {
                    row: 3,
                    start_col: 7,
                    end_col: 12
                },
            ]
        );
        let t = s.begin();
        let f = search(text, &q("Error"), &s, t)
            .expect("ok")
            .expect("current");
        assert_eq!(f.hits.len(), 1, "a capital makes case matter");
    }

    #[test]
    fn a_superseded_search_stops_and_a_huge_pattern_is_refused() {
        let s = Searches::default();
        let old = s.begin();
        let _newer = s.begin();
        let text = "x\n".repeat(10_000);
        assert_eq!(search(&text, &q("x"), &s, old).expect("ok"), None);
        let t = s.begin();
        let big = Query {
            text: "a".repeat(matcher::MAX_PATTERN + 1),
            regex: true,
        };
        assert!(search("a", &big, &s, t).is_err());
        // A regex that would explode a backtracking engine is linear here.
        let t = s.begin();
        let evil = Query {
            text: "(a+)+$".into(),
            regex: true,
        };
        let line = format!("{}b\n", "a".repeat(5000));
        let started = std::time::Instant::now();
        search(&line.repeat(200), &evil, &s, t).expect("ok");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn bounds_keep_the_newest_rows_and_say_so() {
        let s = Searches::default();
        let t = s.begin();
        let text = "hit\n".repeat(MAX_HITS + 10);
        let f = search(&text, &q("hit"), &s, t)
            .expect("ok")
            .expect("current");
        assert_eq!(f.hits.len(), MAX_HITS);
        assert!(f.truncated);
        let t = s.begin();
        let f = search(&"a\n".repeat(MAX_LINES + 5), &q("zzz"), &s, t)
            .expect("ok")
            .expect("current");
        assert!(f.truncated, "older rows were not searched");
    }
}
