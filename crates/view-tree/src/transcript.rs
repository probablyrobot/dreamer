//! A `transcript` node's content: an agent session's events folded into
//! what is drawn. Both renderers keep one [`Transcript`] per bound session
//! and feed it the session's events as they arrive; neither re-reads the
//! whole conversation per token.
//!
//! - **Streaming markdown.** An answer's text is parsed incrementally
//!   ([`Markdown`]): blocks that are finished (ended by a blank line or a
//!   closing code fence) are parsed once and kept, and only the block still
//!   being written is parsed again when a delta arrives. A finished answer
//!   is never parsed again.
//! - **Entries.** Your prompts, answers, thinking, tool calls (one card per
//!   call id, updated in place), approval requests (one card per request,
//!   answered in place), errors, and notes about the session's state.
//! - **Plan and usage** are the latest the session reported.

use crate::rich::{self, Line};

/// A session's state, as the transcript last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Unknown,
    Starting,
    Idle,
    /// A turn is running.
    Working,
    /// The harness waits for an approval.
    Waiting,
    /// No harness runs; sending starts it again.
    Detached,
    Closed,
}

impl State {
    /// A turn is running (or waiting for an answer): stop applies.
    pub fn busy(self) -> bool {
        matches!(self, State::Working | State::Waiting)
    }

    pub fn name(self) -> &'static str {
        match self {
            State::Unknown => "connecting",
            State::Starting => "starting",
            State::Idle => "idle",
            State::Working => "working",
            State::Waiting => "waiting for approval",
            State::Detached => "detached",
            State::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Started,
    Updated,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    Cancelled,
    Error,
}

/// One plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub content: String,
    /// `pending`, `in_progress`, or `completed`.
    pub status: String,
}

/// Tokens of the latest response, and its cost if the harness said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_micro_usd: Option<u64>,
}

impl Usage {
    /// `1.2k in · 340 out · $0.0012`.
    pub fn line(&self) -> String {
        let n = |v: u64| {
            if v >= 10_000 {
                format!("{}k", v / 1000)
            } else if v >= 1000 {
                format!("{:.1}k", v as f64 / 1000.0)
            } else {
                v.to_string()
            }
        };
        let mut s = format!("{} in · {} out", n(self.input), n(self.output));
        if self.cache_read > 0 {
            s.push_str(&format!(" · {} cached", n(self.cache_read)));
        }
        if let Some(c) = self.cost_micro_usd {
            s.push_str(&format!(" · ${:.4}", c as f64 / 1e6));
        }
        s
    }
}

/// One event of a session, as the transcript takes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    User(String),
    Text(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        status: ToolStatus,
        input: Option<String>,
        output: Option<String>,
    },
    Approval {
        id: String,
        title: String,
        message: Option<String>,
        options: Vec<String>,
    },
    Answer {
        id: String,
        allowed: bool,
        option: Option<String>,
        by: String,
    },
    Plan(Vec<Step>),
    Usage(Usage),
    TurnEnd(Stop),
    Error(String),
    Status {
        state: State,
        detail: Option<String>,
    },
}

/// A tool call's card.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    /// The input as JSON.
    pub input: Option<String>,
    pub output: Option<String>,
}

impl Tool {
    /// What the call runs or touches, in one line: a `command`, `path`,
    /// `file_path`, `pattern`, or `url` from its input, else its input.
    pub fn summary(&self) -> String {
        let Some(input) = &self.input else {
            return String::new();
        };
        if let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(input) {
            for k in [
                "command",
                "cmd",
                "path",
                "file_path",
                "pattern",
                "url",
                "query",
            ] {
                if let Some(serde_json::Value::String(v)) = m.get(k) {
                    return v.clone();
                }
            }
        }
        input.clone()
    }

    /// Whether the call is a command (its input has a `command`).
    pub fn is_command(&self) -> bool {
        self.input.as_deref().is_some_and(|i| {
            serde_json::from_str::<serde_json::Value>(i)
                .ok()
                .is_some_and(|v| v.get("command").is_some_and(serde_json::Value::is_string))
        })
    }

    /// The output as drawn: a unified diff (its lines marked), or text.
    pub fn output_lines(&self) -> Vec<Line> {
        let Some(out) = &self.output else {
            return Vec::new();
        };
        if looks_like_diff(out) {
            rich::diff(out)
        } else {
            rich::code(out, "")
        }
    }
}

/// A unified diff: it has a hunk header, or file headers.
fn looks_like_diff(text: &str) -> bool {
    let mut minus = false;
    for l in text.lines().take(200) {
        if l.starts_with("@@ ") {
            return true;
        }
        if l.starts_with("--- ") {
            minus = true;
        } else if minus && l.starts_with("+++ ") {
            return true;
        }
    }
    false
}

/// An approval request's card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub id: String,
    pub title: String,
    pub message: Option<String>,
    pub options: Vec<String>,
    /// How it was answered (allowed, the option, by whom), once it is.
    pub answer: Option<(bool, Option<String>, String)>,
}

/// One thing in the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A prompt sent.
    User(String),
    /// The model's answer, as markdown.
    Answer(Markdown),
    Thinking(String),
    Tool(Tool),
    Approval(Approval),
    Error(String),
    /// The session's state changed, or a turn ended other than by itself.
    Note(String),
}

/// Entries kept at most; the oldest go first.
pub const KEPT: usize = 2000;

/// A session's conversation, as drawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    pub plan: Vec<Step>,
    pub usage: Option<Usage>,
    pub state: State,
    /// The last event's sequence number: following again starts after it.
    pub seq: u64,
    /// Grows with every change, so a renderer can tell it has something
    /// new to draw.
    pub revision: u64,
    /// Entries dropped from the front (oldest), in all.
    pub dropped: u64,
}

impl Transcript {
    /// Take event `seq`. One already taken (a replay after following again)
    /// is ignored. Whether anything changed.
    pub fn take(&mut self, seq: u64, e: Event) -> bool {
        if seq != 0 && seq <= self.seq {
            return false;
        }
        if seq != 0 {
            self.seq = seq;
        }
        self.apply(e);
        self.revision += 1;
        if self.entries.len() > KEPT {
            let n = self.entries.len() - KEPT;
            self.entries.drain(..n);
            self.dropped += n as u64;
        }
        true
    }

    fn apply(&mut self, e: Event) {
        match e {
            Event::User(t) => {
                self.finish();
                self.entries.push(Entry::User(t));
                self.state = State::Working;
            }
            Event::Text(t) => match self.entries.last_mut() {
                Some(Entry::Answer(md)) if !md.done => md.push(&t),
                _ => {
                    self.finish();
                    let mut md = Markdown::default();
                    md.push(&t);
                    self.entries.push(Entry::Answer(md));
                }
            },
            Event::Thinking(t) => match self.entries.last_mut() {
                Some(Entry::Thinking(s)) => s.push_str(&t),
                _ => {
                    self.finish();
                    self.entries.push(Entry::Thinking(t));
                }
            },
            Event::Tool {
                id,
                name,
                status,
                input,
                output,
            } => {
                let found = self.entries.iter_mut().rev().find_map(|en| match en {
                    Entry::Tool(t) if t.id == id => Some(t),
                    _ => None,
                });
                match found {
                    Some(t) => {
                        t.status = status;
                        if !name.is_empty() {
                            t.name = name;
                        }
                        if input.is_some() {
                            t.input = input;
                        }
                        if output.is_some() {
                            t.output = output;
                        }
                    }
                    None => {
                        self.finish();
                        self.entries.push(Entry::Tool(Tool {
                            id,
                            name,
                            status,
                            input,
                            output,
                        }));
                    }
                }
            }
            Event::Approval {
                id,
                title,
                message,
                options,
            } => {
                self.finish();
                self.entries.push(Entry::Approval(Approval {
                    id,
                    title,
                    message,
                    options,
                    answer: None,
                }));
                self.state = State::Waiting;
            }
            Event::Answer {
                id,
                allowed,
                option,
                by,
            } => {
                if let Some(a) = self.entries.iter_mut().rev().find_map(|en| match en {
                    Entry::Approval(a) if a.id == id => Some(a),
                    _ => None,
                }) {
                    a.answer = Some((allowed, option, by));
                }
                if self.state == State::Waiting && self.pending().is_none() {
                    self.state = State::Working;
                }
            }
            Event::Plan(steps) => self.plan = steps,
            Event::Usage(u) => self.usage = Some(u),
            Event::TurnEnd(stop) => {
                self.finish();
                match stop {
                    Stop::EndTurn => {}
                    Stop::Cancelled => self.entries.push(Entry::Note("stopped".into())),
                    Stop::Error => self
                        .entries
                        .push(Entry::Note("the turn ended with an error".into())),
                }
                self.state = State::Idle;
            }
            Event::Error(m) => {
                self.finish();
                self.entries.push(Entry::Error(m));
            }
            Event::Status { state, detail } => {
                self.state = state;
                if matches!(state, State::Detached | State::Closed) {
                    self.finish();
                    let mut note = state.name().to_owned();
                    if let Some(d) = detail.filter(|d| !d.is_empty()) {
                        note.push_str(": ");
                        note.push_str(&d);
                    }
                    self.entries.push(Entry::Note(note));
                }
            }
        }
    }

    /// The answer being written is done: its last block is parsed for good.
    fn finish(&mut self) {
        if let Some(Entry::Answer(md)) = self.entries.last_mut() {
            md.finish();
        }
    }

    /// The oldest approval request still waiting for an answer.
    pub fn pending(&self) -> Option<&Approval> {
        self.entries.iter().find_map(|e| match e {
            Entry::Approval(a) if a.answer.is_none() => Some(a),
            _ => None,
        })
    }

    /// The conversation as plain text, one line per drawn line (for copy
    /// and for tests).
    pub fn text(&self, thinking: bool) -> String {
        let mut out = String::new();
        for e in &self.entries {
            for l in entry_text(e, thinking) {
                out.push_str(&l);
                out.push('\n');
            }
        }
        out
    }
}

/// An entry as plain lines, the way a terminal draws it.
pub fn entry_text(e: &Entry, thinking: bool) -> Vec<String> {
    match e {
        Entry::User(t) => t
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{}{l}", if i == 0 { "> " } else { "  " }))
            .collect(),
        Entry::Answer(md) => md.lines().map(Line::text).collect(),
        Entry::Thinking(t) if thinking => t.lines().map(|l| format!("┊ {l}")).collect(),
        Entry::Thinking(t) => vec![format!("┊ thinking ({} lines)", t.lines().count().max(1))],
        Entry::Tool(t) => {
            let mut v = vec![format!(
                "{} {} {}",
                tool_mark(t.status),
                t.name,
                t.summary()
            )];
            v.extend(t.output_lines().iter().map(|l| format!("  {}", l.text())));
            v
        }
        Entry::Approval(a) => {
            let mut v = vec![format!("? {}", a.title)];
            if let Some(m) = &a.message {
                v.extend(m.lines().map(|l| format!("  {l}")));
            }
            v.push(match &a.answer {
                None if a.options.is_empty() => "  [y] allow  [n] deny".to_owned(),
                None => format!(
                    "  {}  [n] deny",
                    a.options
                        .iter()
                        .enumerate()
                        .map(|(i, o)| format!("[{}] {o}", i + 1))
                        .collect::<Vec<_>>()
                        .join("  ")
                ),
                Some((ok, opt, by)) => format!(
                    "  {}{} by {by}",
                    if *ok { "allowed" } else { "denied" },
                    opt.as_deref()
                        .map(|o| format!(" ({o})"))
                        .unwrap_or_default()
                ),
            });
            v
        }
        Entry::Error(m) => vec![format!("✗ {m}")],
        Entry::Note(n) => vec![format!("· {n}")],
    }
}

/// A tool card's status mark.
pub fn tool_mark(s: ToolStatus) -> &'static str {
    match s {
        ToolStatus::Started | ToolStatus::Updated => "…",
        ToolStatus::Finished => "✓",
        ToolStatus::Failed => "✗",
    }
}

/// Markdown that grows: the finished blocks' lines, parsed once, and the
/// block still being written, parsed again on each delta.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Markdown {
    text: String,
    /// Where the unfinished part starts, and where scanning for the next
    /// block's end goes on from.
    stable: usize,
    scan: usize,
    /// Inside a fenced code block at `scan`, and its marker.
    fence: Option<String>,
    /// The finished blocks' lines; a blank line belongs between them and
    /// the next block.
    kept: Vec<Line>,
    gap: bool,
    tail: Vec<Line>,
    /// No more text will come.
    pub done: bool,
}

impl Markdown {
    /// Markdown of `text`, finished.
    pub fn of(text: &str) -> Markdown {
        let mut m = Markdown::default();
        m.push(text);
        m.finish();
        m
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Append a delta. Costs the length of the unfinished block, not of the
    /// whole text.
    pub fn push(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        // Complete lines past `scan` may end a block.
        let mut end = None;
        while let Some(nl) = self.text[self.scan..].find('\n') {
            let line_end = self.scan + nl + 1;
            let line = &self.text[self.scan..line_end - 1];
            let trimmed = line.trim_start();
            match &self.fence {
                Some(marker) => {
                    if trimmed.starts_with(marker.as_str()) {
                        self.fence = None;
                        end = Some((line_end, false));
                    }
                }
                None => {
                    if let Some(m) = ["```", "~~~"].iter().find(|m| trimmed.starts_with(**m)) {
                        self.fence = Some((*m).to_owned());
                    } else if trimmed.is_empty() {
                        end = Some((line_end, true));
                    }
                }
            }
            self.scan = line_end;
        }
        if let Some((end, blank)) = end {
            let block = rich::markdown(&self.text[self.stable..end]);
            self.append_done(block);
            self.stable = end;
            self.gap |= blank;
        }
        self.tail = rich::markdown(&self.text[self.stable..]);
    }

    fn append_done(&mut self, block: Vec<Line>) {
        if block.is_empty() {
            return;
        }
        if self.gap && self.kept.last().is_some_and(|l| !l.spans.is_empty()) {
            self.kept.push(blank());
        }
        self.gap = false;
        self.kept.extend(block);
    }

    /// No more text comes: the last block is kept as finished.
    pub fn finish(&mut self) {
        if self.done {
            return;
        }
        let tail = std::mem::take(&mut self.tail);
        self.append_done(tail);
        self.stable = self.text.len();
        self.scan = self.text.len();
        self.done = true;
    }

    /// Every line, in order.
    pub fn lines(&self) -> impl Iterator<Item = &Line> + Clone {
        let gap = (self.gap
            && self.kept.last().is_some_and(|l| !l.spans.is_empty())
            && !self.tail.is_empty())
        .then_some(&BLANK);
        self.kept.iter().chain(gap).chain(self.tail.iter())
    }

    /// How many lines [`Markdown::lines`] yields.
    pub fn len(&self) -> usize {
        self.lines().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

static BLANK: Line = Line {
    block: rich::Block::Text,
    spans: Vec::new(),
};

fn blank() -> Line {
    BLANK.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "# Plan\n\nFirst, **read** the `config` and check it.\nIt spans two lines.\n\n- one\n- two with *emphasis*\n\n```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```\nAfter the code.\n\n> a quote\n\n---\n\nThe end.";

    #[test]
    fn streamed_markdown_parses_the_same_as_the_whole_text() {
        for chunk in [1, 2, 3, 7, 16, 1000] {
            let mut md = Markdown::default();
            let chars: Vec<char> = ANSWER.chars().collect();
            for c in chars.chunks(chunk) {
                md.push(&c.iter().collect::<String>());
                // While it streams, it reads as the text so far does.
                let so_far: Vec<Line> = md.lines().cloned().collect();
                assert_eq!(so_far.len(), md.len());
            }
            let streamed: Vec<Line> = md.lines().cloned().collect();
            assert_eq!(streamed, rich::markdown(ANSWER), "chunks of {chunk}");
            md.finish();
            let finished: Vec<Line> = md.lines().cloned().collect();
            assert_eq!(
                finished,
                rich::markdown(ANSWER),
                "finished, chunks of {chunk}"
            );
        }
    }

    #[test]
    fn only_the_unfinished_block_is_parsed_again() {
        let mut md = Markdown::default();
        md.push("para one\n\npara two\n\n");
        let done = md.kept.len();
        assert_eq!(md.stable, md.text.len(), "both paragraphs are finished");
        md.push("the third, still being");
        assert_eq!(md.kept.len(), done, "finished blocks are kept as they were");
        assert_eq!(md.tail.len(), 1);
        md.push(" written");
        assert_eq!(md.tail[0].text(), "the third, still being written");
    }

    fn tool(status: ToolStatus, output: Option<&str>) -> Event {
        Event::Tool {
            id: "call-1".into(),
            name: "bash".into(),
            status,
            input: Some(r#"{"command":"ls"}"#.into()),
            output: output.map(str::to_owned),
        }
    }

    #[test]
    fn events_become_cards_updated_in_place() {
        let mut t = Transcript::default();
        let mut seq = 0;
        let mut take = |t: &mut Transcript, e| {
            seq += 1;
            t.take(seq, e)
        };
        take(&mut t, Event::User("use a tool".into()));
        take(&mut t, Event::Thinking("Hmm.".into()));
        take(&mut t, Event::Text("Let me ".into()));
        take(&mut t, Event::Text("look.".into()));
        take(&mut t, tool(ToolStatus::Started, None));
        take(
            &mut t,
            Event::Approval {
                id: "ui-1".into(),
                title: "Allow bash?".into(),
                message: Some("ls".into()),
                options: vec![],
            },
        );
        assert_eq!(t.state, State::Waiting);
        assert_eq!(t.pending().map(|a| a.id.as_str()), Some("ui-1"));
        take(
            &mut t,
            Event::Answer {
                id: "ui-1".into(),
                allowed: true,
                option: None,
                by: "owner:1".into(),
            },
        );
        take(&mut t, tool(ToolStatus::Finished, Some("file-a")));
        take(&mut t, Event::Text("Done.".into()));
        take(&mut t, Event::TurnEnd(Stop::EndTurn));
        assert_eq!(t.state, State::Idle);
        assert!(t.pending().is_none());
        assert_eq!(
            t.text(false),
            "> use a tool\n┊ thinking (1 lines)\nLet me look.\n✓ bash ls\n  file-a\n? Allow bash?\n  ls\n  allowed by owner:1\nDone.\n"
        );
        // A replayed event changes nothing.
        let before = t.clone();
        assert!(!t.take(3, Event::Text("again".into())));
        assert_eq!(t, before);
    }

    #[test]
    fn a_diff_output_is_drawn_as_a_diff() {
        let t = Tool {
            id: "1".into(),
            name: "edit".into(),
            status: ToolStatus::Finished,
            input: Some(r#"{"path":"a.rs"}"#.into()),
            output: Some("--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new".into()),
        };
        assert_eq!(t.summary(), "a.rs");
        assert!(!t.is_command());
        let roles: Vec<rich::Role> = t.output_lines().iter().map(|l| l.spans[0].role).collect();
        assert_eq!(roles[3], rich::Role::Removed);
        assert_eq!(roles[4], rich::Role::Added);
    }
}
