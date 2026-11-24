//! Building tmux commands without ever handing user text to tmux's parser
//! unescaped.
//!
//! Every argument is encoded as a double-quoted tmux token. Inside double
//! quotes tmux 3.8 (`cmd-parse.y`) still interprets `\`, `$` (variables),
//! and `~` (home directory at the start of a quoted section), so those are
//! escaped, as are `"` and every control byte (as `\ooo` octal, which also
//! keeps newlines out of the control-mode line). `#`, `;`, `{`, `}`, spaces,
//! and non-ASCII text are literal inside double quotes.
//!
//! Some commands expand formats (`#{...}`) in some arguments after parsing;
//! [`Arg::FormatLiteral`] doubles `#` for those.

use std::fmt::Write as _;

/// How a value is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary text, encoded so tmux sees exactly these characters.
    Text(String),
    /// Text for an argument the command expands as a format: `#` doubled
    /// so it stays literal (for example `rename-window` names). tmux then
    /// stores a literal `#[` as `##[`, its escape for "not a style
    /// directive"; read-back values carry that escape.
    FormatLiteral(String),
    /// A format string, passed through as written (still quoted).
    Format(String),
}

impl Value {
    fn encode_into(&self, out: &mut String) {
        match self {
            Value::Text(t) | Value::Format(t) => quote_into(out, t),
            Value::FormatLiteral(t) => quote_into(out, &t.replace('#', "##")),
        }
    }
}

/// A single tmux command: flags and options first, then `--`, then
/// positional arguments. The `--` keeps a positional value that starts with
/// `-` from being read as a flag; tmux parses flags after tokenizing, so
/// quoting alone does not prevent that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxCommand {
    name: &'static str,
    options: Vec<(&'static str, Option<Value>)>,
    positionals: Vec<Value>,
}

impl TmuxCommand {
    /// `name` must be a tmux command name (lowercase letters and `-`).
    pub fn new(name: &'static str) -> Self {
        debug_assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
        Self {
            name,
            options: Vec::new(),
            positionals: Vec::new(),
        }
    }

    fn check_flag(flag: &str) {
        debug_assert!(
            flag.len() >= 2
                && flag.starts_with('-')
                && flag[1..].bytes().all(|b| b.is_ascii_alphabetic()),
            "bad flag {flag}"
        );
    }

    /// A flag without a value, such as `-d`.
    pub fn flag(mut self, flag: &'static str) -> Self {
        Self::check_flag(flag);
        self.options.push((flag, None));
        self
    }

    /// A flag with a text value, such as `-t %3`.
    pub fn opt(mut self, flag: &'static str, value: impl Into<String>) -> Self {
        Self::check_flag(flag);
        self.options.push((flag, Some(Value::Text(value.into()))));
        self
    }

    /// A flag whose value the command expands as a format, given literally.
    pub fn opt_literal_for_format(mut self, flag: &'static str, value: impl Into<String>) -> Self {
        Self::check_flag(flag);
        self.options
            .push((flag, Some(Value::FormatLiteral(value.into()))));
        self
    }

    /// A flag whose value is a format string, such as `-F '#{pane_id}'`.
    pub fn opt_format(mut self, flag: &'static str, format: impl Into<String>) -> Self {
        Self::check_flag(flag);
        self.options
            .push((flag, Some(Value::Format(format.into()))));
        self
    }

    /// A positional text argument.
    pub fn arg(mut self, text: impl Into<String>) -> Self {
        self.positionals.push(Value::Text(text.into()));
        self
    }

    /// A positional argument the command expands as a format, given literally.
    pub fn literal_for_format(mut self, text: impl Into<String>) -> Self {
        self.positionals.push(Value::FormatLiteral(text.into()));
        self
    }

    /// A positional format string.
    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.positionals.push(Value::Format(format.into()));
        self
    }

    /// The command as one line of tmux command syntax.
    pub fn encode(&self) -> String {
        let mut out = String::from(self.name);
        for (flag, value) in &self.options {
            out.push(' ');
            out.push_str(flag);
            if let Some(v) = value {
                out.push(' ');
                v.encode_into(&mut out);
            }
        }
        if !self.positionals.is_empty() {
            out.push_str(" --");
            for v in &self.positionals {
                out.push(' ');
                v.encode_into(&mut out);
            }
        }
        out
    }
}

/// Encode `text` as one double-quoted tmux token.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    quote_into(&mut out, text);
    out
}

fn quote_into(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' | '"' | '$' | '~' => {
                out.push('\\');
                out.push(ch);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Several commands executed as one non-yielding line. tmux answers each
/// with its own guarded response; a failing command stops the rest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Batch {
    commands: Vec<TmuxCommand>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single(command: TmuxCommand) -> Self {
        Self {
            commands: vec![command],
        }
    }

    pub fn push(mut self, command: TmuxCommand) -> Self {
        self.commands.push(command);
        self
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// The batch as one control-mode line, without the newline.
    pub fn encode(&self) -> String {
        self.commands
            .iter()
            .map(TmuxCommand::encode)
            .collect::<Vec<_>>()
            .join(" ; ")
    }
}

impl From<TmuxCommand> for Batch {
    fn from(command: TmuxCommand) -> Self {
        Batch::single(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_what_tmux_interprets() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a b;c#{x}"), "\"a b;c#{x}\"");
        assert_eq!(quote("$HOME ~/x"), "\"\\$HOME \\~/x\"");
        assert_eq!(quote("q\"\\"), "\"q\\\"\\\\\"");
        assert_eq!(
            quote("line\nbreak\ttab\u{1b}"),
            "\"line\\012break\\011tab\\033\""
        );
        assert_eq!(quote("🚀 é"), "\"🚀 é\"");
    }

    #[test]
    fn encoded_lines_never_contain_newlines() {
        let cmd = TmuxCommand::new("rename-window")
            .opt("-t", "@1")
            .arg("evil\n\rkill-server");
        let line = cmd.encode();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
    }

    #[test]
    fn format_literal_doubles_hashes() {
        let cmd = TmuxCommand::new("display-message")
            .flag("-p")
            .literal_for_format("#{pane_id} #x");
        assert_eq!(cmd.encode(), "display-message -p -- \"##{pane_id} ##x\"");
    }

    #[test]
    fn positionals_follow_an_end_of_flags_marker() {
        let cmd = TmuxCommand::new("set-buffer")
            .opt("-b", "x")
            .arg("-not-a-flag");
        assert_eq!(cmd.encode(), "set-buffer -b \"x\" -- \"-not-a-flag\"");
        assert_eq!(TmuxCommand::new("kill-server").encode(), "kill-server");
    }

    #[test]
    fn batches_join_with_separators() {
        let b = Batch::new()
            .push(TmuxCommand::new("select-pane").opt("-t", "%1"))
            .push(
                TmuxCommand::new("resize-pane")
                    .opt("-t", "%1")
                    .opt("-x", "80"),
            );
        assert_eq!(b.len(), 2);
        assert_eq!(
            b.encode(),
            "select-pane -t \"%1\" ; resize-pane -t \"%1\" -x \"80\""
        );
    }
}
