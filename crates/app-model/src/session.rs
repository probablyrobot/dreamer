//! A tree's `transcript` and `composer` nodes in the model: which agent
//! session each is bound to, what the composer holds (its text, history,
//! and attachments), how the transcript is shown (thinking folded or not,
//! scrolled back), and what their keys and clicks ask of the daemon
//! ([`Ask`]). The session's events never pass through here: renderers
//! follow the session themselves (`crate::bound`).

use std::collections::BTreeMap;

use serde_json::Value;
use view_tree::{Bind, Kind, Node};

use crate::manifest::{NodeSpec, NodeType, field};
use crate::model::{App, text};

/// Prompts a composer remembers.
pub const HISTORY: usize = 100;

/// A composer's own state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    /// Prompts sent, oldest first.
    pub history: Vec<String>,
    /// The history entry shown (Up and Down), if any, and the text that
    /// was being written before.
    pub at: Option<usize>,
    pub draft: String,
    /// What goes with the next prompt: a label and its text.
    pub attached: Vec<(String, String)>,
}

/// How a transcript is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shown {
    pub thinking: bool,
    pub back: u32,
}

/// What the transcripts and composers keep, by node key, and what they
/// asked for since the host last took it.
#[derive(Debug, Clone, Default)]
pub struct SessionNodes {
    pub composers: BTreeMap<String, Composer>,
    pub shown: BTreeMap<String, Shown>,
    pub asks: Vec<Ask>,
}

/// What a transcript's or composer's key or click asks the daemon for,
/// on the app's own connection (so under its grant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Send a prompt: its text, `@%N` and `@name` references expanded,
    /// and the composer's attachments after it.
    Send {
        session: String,
        text: String,
        attached: Vec<(String, String)>,
    },
    /// Stop the running turn.
    Cancel { session: String },
    /// Answer the oldest approval request waiting (the host knows which,
    /// from the stream): allow (with an option, by index) or deny.
    Answer {
        session: String,
        allow: bool,
        option: Option<usize>,
    },
    /// Attach the selection (tmux's newest buffer, which a finished
    /// selection in Dreamer becomes) to the composer `node`.
    AttachSelection { node: String },
}

/// Clicks on a composer, by index: focus it, send, stop, attach the
/// selection, and (from [`CLICK_DETACH`]) remove attachment `i`.
pub const CLICK_SEND: usize = 1;
pub const CLICK_STOP: usize = 2;
pub const CLICK_ATTACH: usize = 3;
pub const CLICK_DETACH: usize = 100;
/// A click on a transcript's thinking folds or unfolds it; on an approval
/// card's buttons, denies or allows the oldest request waiting (from
/// [`CLICK_ALLOW`]: with option `i`, or with none when it offers none).
pub const CLICK_THINKING: usize = 1;
pub const CLICK_DENY: usize = 2;
pub const CLICK_ALLOW: usize = 10;

/// Text as a composer takes it: line breaks kept (as `\n`), tabs as
/// spaces, other control characters dropped.
pub fn composer_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The `@` references in a prompt, each once, in order: `@%N` (a pane) and
/// `@name` (a collector). An `@` counts at the start, or after a space, a
/// quote, or an opening bracket, so mail addresses are left alone.
pub fn references(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i] == '@'
            && (i == 0 || chars[i - 1].is_whitespace() || "([{\"'".contains(chars[i - 1]));
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let pane = chars.get(j) == Some(&'%');
        if pane {
            j += 1;
            while chars.get(j).is_some_and(char::is_ascii_digit) {
                j += 1;
            }
        } else if chars
            .get(j)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            while chars
                .get(j)
                .is_some_and(|c| c.is_ascii_alphanumeric() || "_.-".contains(*c))
            {
                j += 1;
            }
        }
        let mut word: String = chars[i + 1..j].iter().collect();
        while word.ends_with('.') {
            word.pop();
        }
        if !word.is_empty() && word != "%" {
            let r = format!("@{word}");
            if !out.contains(&r) {
                out.push(r);
            }
        }
        i = j.max(i + 1);
    }
    out
}

impl App {
    /// The session a transcript or composer node is bound to: its
    /// `session`, or the first row's `field` of its `source`.
    pub fn session_of(&self, key: &str) -> Option<String> {
        let n = self.manifest.nodes.get(key)?;
        if let Some(s) = &n.session {
            return Some(s.clone());
        }
        let row = self.sources.get(&n.source)?.first()?;
        let v = match &n.field {
            Some(p) => field(row, p).map(text),
            None => Some(text(row)),
        }?;
        (!v.is_empty()).then_some(v)
    }

    /// A transcript, composer, or tapped log as a bound node.
    pub(crate) fn session_node(&self, key: &str, spec: &NodeSpec) -> Node {
        let shown = self.sessions.shown.get(key).copied().unwrap_or_default();
        let kind = match spec.kind {
            NodeType::Transcript => Kind::Transcript {
                thinking: shown.thinking,
                back: shown.back,
            },
            NodeType::Composer => Kind::Composer {
                label: spec.label.clone().unwrap_or_default(),
                value: self.values.get(key).cloned().unwrap_or_default(),
                placeholder: spec.placeholder.clone().unwrap_or_default(),
                attachments: self.attachments(key),
            },
            _ => Kind::Log,
        };
        let bind = match spec.tap {
            Some(pane) => Some(Bind::Tap(pane)),
            None => self.session_of(key).map(Bind::Session),
        };
        Node {
            bind,
            ..Node::new(key, kind)
        }
    }

    /// A composer's attachments as shown: those attached, then the
    /// references in its text.
    fn attachments(&self, key: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .sessions
            .composers
            .get(key)
            .map(|c| c.attached.iter().map(|(l, _)| l.clone()).collect())
            .unwrap_or_default();
        if let Some(v) = self.values.get(key) {
            out.extend(references(v));
        }
        out
    }

    /// The focused node is a composer: text goes into it.
    pub fn in_composer(&self) -> bool {
        self.focused
            .as_deref()
            .and_then(|k| self.manifest.nodes.get(k))
            .is_some_and(|n| n.kind == NodeType::Composer)
    }

    fn focused_session_node(&self) -> Option<(String, NodeType)> {
        let k = self.focused.clone()?;
        let kind = self.manifest.nodes.get(&k)?.kind;
        kind.is_session().then_some((k, kind))
    }

    /// Text typed or pasted into the focused composer, line breaks kept.
    pub(crate) fn composer_insert(&mut self, text: &str) -> bool {
        let Some((key, NodeType::Composer)) = self.focused_session_node() else {
            return false;
        };
        let clean = composer_text(text);
        self.values.entry(key.clone()).or_default().push_str(&clean);
        if let Some(c) = self.sessions.composers.get_mut(&key) {
            c.at = None;
        }
        true
    }

    /// The asks since the last call, oldest first.
    pub fn take_asks(&mut self) -> Vec<Ask> {
        std::mem::take(&mut self.sessions.asks)
    }

    /// Attach `text` to composer `node` as `label`.
    pub fn attach(&mut self, node: &str, label: &str, text: String) {
        self.sessions
            .composers
            .entry(node.to_owned())
            .or_default()
            .attached
            .push((label.to_owned(), text));
    }

    /// A key for the focused transcript or composer. Returns whether it
    /// was taken.
    ///
    /// - Composer: letters, digits, Space, and Backspace edit; Enter
    ///   sends; Ctrl-j (or Shift-Enter, Alt-Enter) starts a new line; Up
    ///   and Down step through the prompts sent; Esc stops the running
    ///   turn; Ctrl-s attaches the selection; Ctrl-x removes the last
    ///   attachment.
    /// - Transcript: `y` allows the oldest waiting approval (with its first
    ///   option), `1` to `9` allow it with that option, `n` denies it; `t`
    ///   folds or unfolds thinking; Up, Down, PageUp, PageDown, Home, and
    ///   End scroll.
    pub(crate) fn session_key(&mut self, name: &str) -> bool {
        let Some((key, kind)) = self.focused_session_node() else {
            return false;
        };
        let session = self.session_of(&key);
        match kind {
            NodeType::Composer => self.composer_key(&key, session, name),
            _ => self.transcript_key(&key, session, name),
        }
    }

    fn composer_key(&mut self, key: &str, session: Option<String>, name: &str) -> bool {
        let value = self.values.entry(key.to_owned()).or_default();
        let c = self.sessions.composers.entry(key.to_owned()).or_default();
        match name {
            "Enter" => {
                let text = std::mem::take(value);
                let attached = std::mem::take(&mut c.attached);
                c.at = None;
                if text.trim().is_empty() && attached.is_empty() {
                    return true;
                }
                if !text.trim().is_empty() && c.history.last() != Some(&text) {
                    c.history.push(text.clone());
                    if c.history.len() > HISTORY {
                        c.history.remove(0);
                    }
                }
                match session {
                    Some(session) => self.sessions.asks.push(Ask::Send {
                        session,
                        text,
                        attached,
                    }),
                    None => self.status = Some("no agent session to send to".into()),
                }
            }
            "Ctrl-j" | "Shift-Enter" | "Alt-Enter" => value.push('\n'),
            "Backspace" => {
                value.pop();
                c.at = None;
            }
            " " | "Space" => value.push(' '),
            "Up" => {
                let at = match c.at {
                    None if c.history.is_empty() => return true,
                    None => {
                        c.draft = value.clone();
                        c.history.len() - 1
                    }
                    Some(i) => i.saturating_sub(1),
                };
                c.at = Some(at);
                value.clone_from(&c.history[at]);
            }
            "Down" => match c.at {
                Some(i) if i + 1 < c.history.len() => {
                    c.at = Some(i + 1);
                    value.clone_from(&c.history[i + 1]);
                }
                Some(_) => {
                    c.at = None;
                    *value = std::mem::take(&mut c.draft);
                }
                None => {}
            },
            "Esc" => match session {
                Some(session) => self.sessions.asks.push(Ask::Cancel { session }),
                None => return false,
            },
            "Ctrl-s" => self.sessions.asks.push(Ask::AttachSelection {
                node: key.to_owned(),
            }),
            "Ctrl-x" => {
                c.attached.pop();
            }
            k if k.chars().count() == 1 && !k.chars().all(char::is_control) => {
                value.push_str(k);
                c.at = None;
            }
            _ => return false,
        }
        true
    }

    fn transcript_key(&mut self, key: &str, session: Option<String>, name: &str) -> bool {
        let shown = self.sessions.shown.entry(key.to_owned()).or_default();
        let answer = |allow: bool, option: Option<usize>| {
            session.clone().map(|session| Ask::Answer {
                session,
                allow,
                option,
            })
        };
        let ask = match name {
            "y" => answer(true, None),
            "n" => answer(false, None),
            d if d.len() == 1 && ('1'..='9').contains(&d.chars().next().unwrap_or('0')) => {
                answer(true, d.parse::<usize>().ok().map(|i| i - 1))
            }
            "t" => {
                shown.thinking = !shown.thinking;
                None
            }
            "Up" => {
                shown.back = shown.back.saturating_add(1);
                None
            }
            "Down" => {
                shown.back = shown.back.saturating_sub(1);
                None
            }
            "PageUp" => {
                shown.back = shown.back.saturating_add(10);
                None
            }
            "PageDown" => {
                shown.back = shown.back.saturating_sub(10);
                None
            }
            "Home" => {
                shown.back = u32::MAX / 2;
                None
            }
            "End" => {
                shown.back = 0;
                None
            }
            _ => return false,
        };
        self.sessions.asks.extend(ask);
        true
    }

    /// A click on the focused transcript or composer (`index`: see
    /// [`CLICK_SEND`] and the others). Whether it was one.
    pub(crate) fn session_click(&mut self, index: usize) -> bool {
        let Some((key, kind)) = self.focused_session_node() else {
            return false;
        };
        let session = self.session_of(&key);
        match (kind, index) {
            (NodeType::Composer, CLICK_SEND) => {
                self.composer_key(&key, session, "Enter");
            }
            (NodeType::Composer, CLICK_STOP) => {
                self.composer_key(&key, session, "Esc");
            }
            (NodeType::Composer, CLICK_ATTACH) => {
                self.composer_key(&key, session, "Ctrl-s");
            }
            (NodeType::Composer, i) if i >= CLICK_DETACH => {
                if let Some(c) = self.sessions.composers.get_mut(&key)
                    && i - CLICK_DETACH < c.attached.len()
                {
                    c.attached.remove(i - CLICK_DETACH);
                }
            }
            (NodeType::Transcript, CLICK_THINKING) => {
                self.transcript_key(&key, session, "t");
            }
            (NodeType::Transcript, CLICK_DENY) => {
                self.transcript_key(&key, session, "n");
            }
            (NodeType::Transcript, i) if (CLICK_ALLOW..CLICK_ALLOW + 9).contains(&i) => {
                if let Some(session) = session {
                    self.sessions.asks.push(Ask::Answer {
                        session,
                        allow: true,
                        option: Some(i - CLICK_ALLOW),
                    });
                }
            }
            _ => {}
        }
        true
    }
}

/// A composer's prompt as sent: its text, then each attachment as a
/// labelled fenced block (the same form the pi extension uses), at most
/// the last 200 lines and 16 KB of each.
pub fn prompt(text: &str, attached: &[(String, String)]) -> String {
    let mut out = text.trim_end().to_owned();
    for (label, body) in attached {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&attachment(label, body));
    }
    out
}

/// Lines and bytes an attachment keeps at most.
pub const ATTACH_LINES: usize = 200;
pub const ATTACH_BYTES: usize = 16 * 1024;

/// One attachment: `Label:` then the text in a fence longer than any run
/// of backticks inside it, cut to its last lines.
pub fn attachment(label: &str, body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();
    let mut kept: Vec<&str> = lines[total.saturating_sub(ATTACH_LINES)..].to_vec();
    while kept.iter().map(|l| l.len() + 1).sum::<usize>() > ATTACH_BYTES && kept.len() > 1 {
        kept.remove(0);
    }
    let text = kept.join("\n");
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let note = if kept.len() < total {
        format!(" (cut to the last {} of {total} lines)", kept.len())
    } else {
        String::new()
    };
    format!("{label}{note}:\n{fence}text\n{text}\n{fence}")
}

/// The pane a reference names (`@%3` is 3), if it names one.
pub fn reference_pane(r: &str) -> Option<u32> {
    r.strip_prefix("@%")?.parse().ok()
}

/// A row of a collector, for an attachment: its pane, its command, and
/// its line or lines.
pub fn row_text(r: &Value) -> String {
    let mut head = Vec::new();
    for (k, prefix) in [("id", "#"), ("pane", "%")] {
        if let Some(v) = r.get(k) {
            head.push(format!("{prefix}{}", text(v)));
        }
    }
    if let Some(c) = r.get("command").map(text).filter(|c| !c.is_empty()) {
        head.push(format!("[{c}]"));
    }
    let mut out = vec![head.join(" ")];
    if let Some(l) = r.get("line").map(text).filter(|l| !l.is_empty()) {
        out.push(format!("  > {l}"));
    } else if let Some(Value::Array(lines)) = r.get("lines") {
        out.extend(lines.iter().map(|l| format!("    {}", text(l))));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    const CHAT: &str = r#"
schema_version = 2
id = "test.chat"
version = "0.1.0"
title = "Chat"
capabilities = ["agent.sessions.list"]
root = "main"

[data.sessions]
method = "agent.sessions.list"
path = "sessions"

[nodes.main]
type = "column"
children = ["chat", "prompt", "out"]

[nodes.chat]
type = "transcript"
source = "sessions"
field = "id"

[nodes.prompt]
type = "composer"
source = "sessions"
field = "id"
placeholder = "Ask"

[nodes.out]
type = "log"
tap = 4
"#;

    fn chat() -> App {
        let mut a = App::new(Manifest::parse(CHAT).expect("manifest"));
        a.set_source("sessions", vec![serde_json::json!({"id": "s1"})], 0);
        a
    }

    #[test]
    fn bound_nodes_name_their_stream_and_the_grant_follows_them() {
        let a = chat();
        let tree = a.frame().expect("tree");
        assert_eq!(
            tree.root.binds(),
            vec![Bind::Session("s1".into()), Bind::Tap(4)]
        );
        let json = serde_json::to_string(&tree).expect("json");
        assert!(json.contains(r#""bind":{"session":"s1"}"#), "{json}");
        assert!(json.contains(r#""bind":{"tap":4}"#), "{json}");
        let needs = a.manifest.needs();
        for m in [
            "agent.sessions.events",
            "agent.sessions.send",
            "agent.sessions.cancel",
            "agent.sessions.approve",
            "agent.sessions.deny",
            "taps.subscribe",
        ] {
            assert!(needs.methods.iter().any(|x| x == m), "{m}: {needs:?}");
        }
        assert_eq!(needs.panes, vec![4]);
        // A transcript with neither a session nor a source, and a log with
        // both a tap and a source, are refused with their node.
        let bad = CHAT
            .replace(
                "source = \"sessions\"\nfield = \"id\"\n\n[nodes.prompt]",
                "\n[nodes.prompt]",
            )
            .replace("tap = 4", "tap = 4\nsource = \"sessions\"");
        let errors = Manifest::parse(&bad).expect_err("invalid");
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("nodes.chat: a transcript needs")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("nodes.out: a log has `tap` or a `source`")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_composer_edits_sends_remembers_and_stops() {
        let mut a = chat();
        a.focus_next();
        assert_eq!(a.focused.as_deref(), Some("prompt"));
        assert!(a.takes_text());
        // Pasted text keeps its line breaks; Ctrl-j adds one.
        a.insert_text("look at @%3\r\nplease");
        a.control_key("Ctrl-j");
        a.control_key("q");
        let Some(Kind::Composer {
            value, attachments, ..
        }) = a
            .frame()
            .and_then(|f| f.root.find("prompt").map(|n| n.kind.clone()))
        else {
            panic!("no composer");
        };
        assert_eq!(value, "look at @%3\nplease\nq");
        assert_eq!(attachments, vec!["@%3"]);
        a.control_key("Ctrl-s");
        a.attach("prompt", "Selection", "picked".into());
        a.control_key("Enter");
        assert_eq!(
            a.take_asks(),
            vec![
                Ask::AttachSelection {
                    node: "prompt".into()
                },
                Ask::Send {
                    session: "s1".into(),
                    text: "look at @%3\nplease\nq".into(),
                    attached: vec![("Selection".into(), "picked".into())],
                }
            ]
        );
        assert_eq!(a.values["prompt"], "");
        // Up brings the prompt back, Down the draft.
        a.insert_text("draft");
        a.control_key("Up");
        assert_eq!(a.values["prompt"], "look at @%3\nplease\nq");
        a.control_key("Down");
        assert_eq!(a.values["prompt"], "draft");
        a.control_key("Esc");
        assert_eq!(
            a.take_asks(),
            vec![Ask::Cancel {
                session: "s1".into()
            }]
        );
        // A click on Send sends as Enter does.
        a.select_at(Some("prompt"), CLICK_SEND);
        assert!(matches!(&a.take_asks()[..], [Ask::Send { text, .. }] if text == "draft"));
    }

    #[test]
    fn a_transcript_answers_approvals_and_folds_thinking() {
        let mut a = chat();
        assert_eq!(a.focused.as_deref(), Some("chat"));
        a.control_key("y");
        a.control_key("2");
        a.control_key("n");
        a.select_at(Some("chat"), CLICK_ALLOW);
        let answer = |allow, option| Ask::Answer {
            session: "s1".into(),
            allow,
            option,
        };
        assert_eq!(
            a.take_asks(),
            vec![
                answer(true, None),
                answer(true, Some(1)),
                answer(false, None),
                answer(true, Some(0))
            ]
        );
        a.control_key("t");
        a.control_key("PageUp");
        let node = a.frame().and_then(|f| f.root.find("chat").cloned());
        assert_eq!(
            node.map(|n| n.kind),
            Some(Kind::Transcript {
                thinking: true,
                back: 10
            })
        );
    }

    #[test]
    fn references_skip_mail_addresses_and_trailing_dots() {
        assert_eq!(
            references("what failed in @%3? see @errors. not me@example.com, (@%12)"),
            vec!["@%3", "@errors", "@%12"]
        );
        assert_eq!(reference_pane("@%12"), Some(12));
        assert_eq!(reference_pane("@errors"), None);
    }

    #[test]
    fn an_attachment_is_fenced_longer_than_its_backticks_and_cut() {
        let a = attachment("Selection", "a ``` b\nc");
        assert_eq!(a, "Selection:\n````text\na ``` b\nc\n````");
        let long: String = (0..300).map(|i| format!("line {i}\n")).collect();
        let a = attachment("Pane %1", &long);
        assert!(a.starts_with("Pane %1 (cut to the last 200 of 300 lines):"));
        assert!(a.contains("line 299") && !a.contains("line 99\n"));
        assert_eq!(
            prompt("hi\n", &[("Selection".into(), "x".into())]),
            "hi\n\nSelection:\n```text\nx\n```"
        );
    }
}
