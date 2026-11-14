//! The match stage of queries, taps, and filters (spec 12.8): it runs in
//! Rust, never in a script, and its cost is bounded by construction.
//!
//! - **Literals:** one or more substrings, optionally ignoring case.
//! - **Regular expressions:** the `regex` crate's engine, which runs in
//!   time linear in the input (no backtracking), compiled under size limits
//!   so a pathological pattern is refused at compile time instead of
//!   consuming memory.
//!
//! Patterns are also bounded in length. Lines longer than
//! [`MAX_LINE`] are matched on their first [`MAX_LINE`] bytes.

use regex::{Regex, RegexBuilder};

/// The longest pattern accepted, in bytes.
pub const MAX_PATTERN: usize = 4096;
/// The most literals in one set.
pub const MAX_LITERALS: usize = 256;
/// Compiled program size limit.
const SIZE_LIMIT: usize = 1 << 20;
/// Lazy DFA cache limit.
const DFA_LIMIT: usize = 2 << 20;
/// Bytes of a line considered.
pub const MAX_LINE: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// A regular expression, or with `literals`, one literal.
    pub pattern: String,
    /// More literals: a match is any of them (`pattern` included when not
    /// empty). Treated as plain text, never as a pattern.
    pub literals: Vec<String>,
    /// `pattern` is plain text too.
    pub literal: bool,
    pub ignore_case: bool,
}

#[derive(Debug, Clone)]
pub struct Matcher(Regex);

/// Where a line matched, as byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub start: usize,
    pub end: usize,
}

impl Matcher {
    pub fn new(spec: &Spec) -> Result<Matcher, String> {
        let total = spec.pattern.len() + spec.literals.iter().map(String::len).sum::<usize>();
        if total > MAX_PATTERN {
            return Err(format!(
                "the pattern is {total} bytes; the limit is {MAX_PATTERN}"
            ));
        }
        if spec.literals.len() > MAX_LITERALS {
            return Err(format!(
                "{} literals; the limit is {MAX_LITERALS}",
                spec.literals.len()
            ));
        }
        let mut alternatives: Vec<String> = Vec::new();
        if !spec.pattern.is_empty() {
            alternatives.push(if spec.literal {
                regex::escape(&spec.pattern)
            } else {
                format!("(?:{})", spec.pattern)
            });
        }
        alternatives.extend(
            spec.literals
                .iter()
                .filter(|l| !l.is_empty())
                .map(|l| regex::escape(l)),
        );
        if alternatives.is_empty() {
            return Err("an empty pattern matches everything; give one".into());
        }
        RegexBuilder::new(&alternatives.join("|"))
            .case_insensitive(spec.ignore_case)
            .size_limit(SIZE_LIMIT)
            .dfa_size_limit(DFA_LIMIT)
            .build()
            .map(Matcher)
            .map_err(|e| match e {
                regex::Error::CompiledTooBig(_) => {
                    "the pattern is too complex (it compiles beyond the size limit)".to_owned()
                }
                other => other.to_string(),
            })
    }

    /// The first match in `line`.
    pub fn find(&self, line: &str) -> Option<Found> {
        let mut cut = line.len().min(MAX_LINE);
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        self.0.find(&line[..cut]).map(|m| Found {
            start: m.start(),
            end: m.end(),
        })
    }

    pub fn is_match(&self, line: &str) -> bool {
        self.find(line).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(pattern: &str) -> Spec {
        Spec {
            pattern: pattern.into(),
            literals: Vec::new(),
            literal: false,
            ignore_case: false,
        }
    }

    #[test]
    fn regexes_literals_and_case() {
        let m = Matcher::new(&spec(r"\b(error|panic)\b")).expect("regex");
        assert_eq!(m.find("a panic here"), Some(Found { start: 2, end: 7 }));
        assert!(!m.is_match("errors"));
        let lit = Matcher::new(&Spec {
            pattern: "a.b".into(),
            literal: true,
            ..spec("")
        })
        .expect("literal");
        assert!(lit.is_match("xa.by") && !lit.is_match("axb"));
        let set = Matcher::new(&Spec {
            literals: vec!["FAILED".into(), "(oops)".into()],
            ignore_case: true,
            ..spec("")
        })
        .expect("set");
        assert!(set.is_match("test failed") && set.is_match("(OOPS)!"));
    }

    #[test]
    fn pathological_and_oversized_patterns_are_refused() {
        let e = Matcher::new(&spec("(a{1000}){1000}")).expect_err("too big");
        assert!(e.contains("too complex"), "{e}");
        assert!(Matcher::new(&spec(&"a".repeat(MAX_PATTERN + 1))).is_err());
        assert!(Matcher::new(&spec("")).is_err());
        assert!(Matcher::new(&spec("(unclosed")).is_err());
        // Linear time: a classic backtracking trap is just a fast miss.
        let m = Matcher::new(&spec("(a+)+$")).expect("regex");
        let started = std::time::Instant::now();
        assert!(!m.is_match(&format!("{}!", "a".repeat(5000))));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let m = Matcher::new(&spec("tail")).expect("regex");
        let line = format!("{}tail", "é".repeat(MAX_LINE));
        assert!(!m.is_match(&line), "beyond the limit");
        assert!(m.is_match("short tail"));
    }
}
