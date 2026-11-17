//! Styled text for the text components: markdown, highlighted code, and
//! unified diffs, turned into lines of spans that each carry a [`Role`].
//! The renderers pick a colour or a weight for every role from their
//! theme, so both draw the same structure in the one scheme's colours.
//!
//! This is a small, dependency-free subset on purpose:
//! - markdown: ATX headings, paragraphs, `-`/`*`/`+` and numbered lists,
//!   quotes, rules, fenced code blocks, and inline code, strong, emphasis,
//!   and links (shown as their text);
//! - code: a lexical highlighter (keywords, strings, numbers, comments,
//!   types) for a handful of languages, plain for the rest;
//! - diff: file, hunk, added, and removed lines.

/// What a span of text is, which decides how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Plain,
    Strong,
    Emphasis,
    /// Inline code in prose. (Code with no colour of its own is plain.)
    Code,
    /// A link's text.
    Link,
    /// A list's bullet or number, a quote's bar.
    Marker,
    Keyword,
    String,
    Number,
    Comment,
    /// A type or another capitalised name.
    Type,
    /// A diff's added line.
    Added,
    /// A diff's removed line.
    Removed,
    /// A diff's hunk header (`@@ ... @@`).
    Hunk,
    /// A diff's file header (`diff`, `---`, `+++`, `index`).
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub role: Role,
    pub text: String,
}

/// What kind of line a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// Prose: a paragraph or a list item.
    Text,
    /// A heading, level 1 to 6.
    Heading(u8),
    Quote,
    /// A line of code (a code block, a `code` node, or a diff).
    Code,
    /// A horizontal rule.
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub block: Block,
    pub spans: Vec<Span>,
}

impl Line {
    fn new(block: Block) -> Line {
        Line {
            block,
            spans: Vec::new(),
        }
    }

    /// The line's text, without roles.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn push(&mut self, role: Role, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.role == role => last.text.push_str(text),
            _ => self.spans.push(Span {
                role,
                text: text.to_owned(),
            }),
        }
    }
}

/// Markdown as lines. A paragraph's lines are joined into one line (the
/// renderers wrap it); blank lines between blocks are kept, one at most.
pub fn markdown(text: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut fence: Option<(String, Lang)> = None;
    let mut open_paragraph = false;
    let mut in_comment = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some((marker, _)) = &fence {
            if trimmed.starts_with(marker.as_str()) {
                fence = None;
            } else {
                out.push(code_line(raw, lang_of_fence(&fence), &mut in_comment));
            }
            continue;
        }
        if let Some(m) = ["```", "~~~"].iter().find(|m| trimmed.starts_with(**m)) {
            let lang = Lang::named(trimmed[m.len()..].trim());
            fence = Some(((*m).to_owned(), lang));
            in_comment = false;
            open_paragraph = false;
            continue;
        }
        if trimmed.is_empty() {
            open_paragraph = false;
            if out.last().is_some_and(|l| !l.spans.is_empty()) {
                out.push(Line::new(Block::Text));
            }
            continue;
        }
        if let Some((level, rest)) = heading(trimmed) {
            open_paragraph = false;
            let mut l = Line::new(Block::Heading(level));
            inline(rest, &mut l);
            out.push(l);
            continue;
        }
        if is_rule(trimmed) {
            open_paragraph = false;
            out.push(Line::new(Block::Rule));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            open_paragraph = false;
            let mut l = Line::new(Block::Quote);
            inline(rest.trim_start(), &mut l);
            out.push(l);
            continue;
        }
        let indent = raw.len() - trimmed.len();
        if let Some((mark, rest)) = list_item(trimmed) {
            open_paragraph = false;
            let mut l = Line::new(Block::Text);
            l.push(Role::Plain, &" ".repeat(indent.min(12)));
            l.push(Role::Marker, &mark);
            inline(rest, &mut l);
            out.push(l);
            continue;
        }
        // A paragraph, or the next line of one (a list item's too).
        match out.last_mut() {
            Some(l) if open_paragraph || (l.block == Block::Text && indent > 0) => {
                l.push(Role::Plain, " ");
                inline(trimmed, l);
            }
            _ => {
                let mut l = Line::new(Block::Text);
                inline(trimmed, &mut l);
                out.push(l);
            }
        }
        open_paragraph = true;
    }
    while out
        .last()
        .is_some_and(|l| l.spans.is_empty() && l.block == Block::Text)
    {
        out.pop();
    }
    out
}

fn lang_of_fence(fence: &Option<(String, Lang)>) -> Lang {
    fence.as_ref().map_or(Lang::PLAIN, |(_, l)| *l)
}

/// `# Title` to `###### Title`.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((hashes as u8, rest.trim().trim_end_matches('#').trim_end()))
}

/// Three or more of `-`, `*`, or `_`, and nothing else but spaces.
fn is_rule(line: &str) -> bool {
    let chars: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3 && ['-', '*', '_'].iter().any(|m| chars.iter().all(|c| c == m))
}

/// A list item's marker as drawn (`• ` or `3. `), and its text.
fn list_item(line: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(m) {
            // A task list's box stays as text.
            return Some(("• ".to_owned(), rest));
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &line[digits..];
        for sep in [". ", ") "] {
            if let Some(text) = rest.strip_prefix(sep) {
                return Some((format!("{}. ", &line[..digits]), text));
            }
        }
    }
    None
}

/// Inline markdown into `out`: code spans, strong, emphasis, links (their
/// text), and backslash escapes. A marker with no closing one is text.
fn inline(text: &str, out: &mut Line) {
    let chars: Vec<char> = text.chars().collect();
    let (mut strong, mut emphasis) = (false, false);
    let role = |strong: bool, emphasis: bool| {
        if strong {
            Role::Strong
        } else if emphasis {
            Role::Emphasis
        } else {
            Role::Plain
        }
    };
    let rest = |i: usize| chars[i..].iter().collect::<String>();
    let mut i = 0;
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Line, r: Role| {
        out.push(r, buf);
        buf.clear();
    };
    while i < chars.len() {
        let c = chars[i];
        let here = role(strong, emphasis);
        match c {
            '\\' if i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
                let fence: String = "`".repeat(ticks);
                let after = rest(i + ticks);
                if let Some(end) = after.find(&fence) {
                    flush(&mut buf, out, here);
                    out.push(Role::Code, after[..end].trim());
                    i += ticks + after[..end].chars().count() + ticks;
                } else {
                    buf.push_str(&fence);
                    i += ticks;
                }
            }
            '*' | '_' if i + 1 < chars.len() && chars[i + 1] == c => {
                let pair: String = [c, c].iter().collect();
                if strong || rest(i + 2).contains(&pair) {
                    flush(&mut buf, out, here);
                    strong = !strong;
                } else {
                    buf.push_str(&pair);
                }
                i += 2;
            }
            '*' | '_' => {
                // `_` inside a word (snake_case) is text.
                let inside_word = c == '_'
                    && i > 0
                    && chars[i - 1].is_alphanumeric()
                    && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
                if !inside_word && (emphasis || rest(i + 1).contains(c)) {
                    flush(&mut buf, out, here);
                    emphasis = !emphasis;
                } else {
                    buf.push(c);
                }
                i += 1;
            }
            '[' => {
                let after = rest(i + 1);
                let link = after.find("](").and_then(|close| {
                    let tail = &after[close + 2..];
                    tail.find(')').map(|end| (close, end))
                });
                match link {
                    Some((close, end)) => {
                        flush(&mut buf, out, here);
                        out.push(Role::Link, &after[..close]);
                        let used = after[..close + 2 + end + 1].chars().count();
                        i += 1 + used;
                    }
                    None => {
                        buf.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    let last = role(strong, emphasis);
    out.push(last, &buf);
}

/// What the highlighter knows about a language.
#[derive(Debug, Clone, Copy)]
struct Lang {
    keywords: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static str,
    /// Capitalised names are types.
    types: bool,
    /// Plain: no highlighting at all.
    plain: bool,
}

impl Lang {
    const PLAIN: Lang = Lang {
        keywords: &[],
        line_comment: &[],
        block_comment: None,
        quotes: "",
        types: false,
        plain: true,
    };

    /// The language a fence or a `code` node names; plain when unknown.
    fn named(name: &str) -> Lang {
        let c_like = |keywords| Lang {
            keywords,
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            quotes: "\"'`",
            types: true,
            plain: false,
        };
        let hash = |keywords, quotes| Lang {
            keywords,
            line_comment: &["#"],
            block_comment: None,
            quotes,
            types: false,
            plain: false,
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Lang {
                quotes: "\"",
                ..c_like(RUST)
            },
            "ts" | "typescript" | "tsx" | "js" | "javascript" | "jsx" => c_like(TS),
            "go" => c_like(GO),
            "c" | "h" | "cpp" | "c++" | "cc" | "hpp" => c_like(C),
            "json" => c_like(LITERALS),
            "python" | "py" => Lang {
                types: true,
                ..hash(PYTHON, "\"'")
            },
            "sh" | "bash" | "zsh" | "shell" | "console" => hash(SHELL, "\"'"),
            "toml" | "yaml" | "yml" => hash(LITERALS, "\"'"),
            "lua" | "luau" => Lang {
                keywords: LUA,
                line_comment: &["--"],
                block_comment: None,
                quotes: "\"'",
                types: false,
                plain: false,
            },
            _ => Lang::PLAIN,
        }
    }
}

const LITERALS: &[&str] = &["true", "false", "null"];
const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const TS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "return",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];
const GO: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const C: &[&str] = &[
    "break",
    "case",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "float",
    "for",
    "if",
    "include",
    "int",
    "long",
    "namespace",
    "return",
    "short",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "unsigned",
    "void",
    "while",
];
const PYTHON: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "elif", "else",
    "except", "False", "finally", "for", "from", "if", "import", "in", "is", "lambda", "None",
    "not", "or", "pass", "raise", "return", "True", "try", "while", "with", "yield",
];
const SHELL: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in",
    "local", "return", "then", "until", "while",
];
const LUA: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Code as lines, highlighted by `language` (`rust`, `ts`, `python`,
/// `sh`, `go`, `c`, `json`, `toml`, `yaml`, `lua`, and their usual short
/// names); any other language is drawn plain.
pub fn code(text: &str, language: &str) -> Vec<Line> {
    let lang = Lang::named(language);
    let mut in_comment = false;
    text.lines()
        .map(|l| code_line(l, lang, &mut in_comment))
        .collect()
}

/// One line of code; `in_comment` carries a block comment across lines.
fn code_line(line: &str, lang: Lang, in_comment: &mut bool) -> Line {
    let mut out = Line::new(Block::Code);
    if lang.plain {
        out.push(Role::Plain, line);
        return out;
    }
    let mut rest = line;
    while !rest.is_empty() {
        if *in_comment {
            let Some((_, close)) = lang.block_comment else {
                *in_comment = false;
                continue;
            };
            match rest.find(close) {
                Some(end) => {
                    out.push(Role::Comment, &rest[..end + close.len()]);
                    rest = &rest[end + close.len()..];
                    *in_comment = false;
                }
                None => {
                    out.push(Role::Comment, rest);
                    rest = "";
                }
            }
            continue;
        }
        if lang.line_comment.iter().any(|m| rest.starts_with(m)) {
            out.push(Role::Comment, rest);
            break;
        }
        if let Some((open, _)) = lang.block_comment
            && rest.starts_with(open)
        {
            out.push(Role::Comment, open);
            rest = &rest[open.len()..];
            *in_comment = true;
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        if lang.quotes.contains(c) {
            let mut end = c.len_utf8();
            let mut escaped = false;
            for (i, ch) in rest[end..].char_indices() {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == c {
                    end += i + ch.len_utf8();
                    break;
                }
                if i + ch.len_utf8() == rest.len() - c.len_utf8() {
                    end = rest.len();
                }
            }
            out.push(Role::String, &rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if c.is_ascii_digit() {
            let end = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                .unwrap_or(rest.len());
            out.push(Role::Number, &rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let end = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            let word = &rest[..end];
            let role = if lang.keywords.contains(&word) {
                Role::Keyword
            } else if lang.types && word.starts_with(|ch: char| ch.is_uppercase()) {
                Role::Type
            } else {
                Role::Plain
            };
            out.push(role, word);
            rest = &rest[end..];
            continue;
        }
        out.push(Role::Plain, &rest[..c.len_utf8()]);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A unified diff as lines: file headers, hunk headers, added and removed
/// lines, and context.
pub fn diff(text: &str) -> Vec<Line> {
    text.lines()
        .map(|l| {
            let role = if l.starts_with("+++")
                || l.starts_with("---")
                || l.starts_with("diff ")
                || l.starts_with("index ")
            {
                Role::Meta
            } else if l.starts_with("@@") {
                Role::Hunk
            } else if l.starts_with('+') {
                Role::Added
            } else if l.starts_with('-') {
                Role::Removed
            } else {
                Role::Plain
            };
            let mut line = Line::new(Block::Code);
            line.push(role, l);
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(l: &Line) -> Vec<(Role, &str)> {
        l.spans.iter().map(|s| (s.role, s.text.as_str())).collect()
    }

    #[test]
    fn markdown_blocks_and_inline_styles() {
        let md = "# Title\n\nSome **bold** and *it* with `code` and a [link](https://x.y).\nnext line\n\n- one\n  - two\n3. three\n> quoted\n---\n```rust\nfn main() {}\n```\nsnake_case_name stays";
        let lines = markdown(md);
        assert_eq!(lines[0].block, Block::Heading(1));
        assert_eq!(lines[0].text(), "Title");
        assert_eq!(lines[1].spans, vec![], "one blank line between blocks");
        assert_eq!(
            roles(&lines[2]),
            vec![
                (Role::Plain, "Some "),
                (Role::Strong, "bold"),
                (Role::Plain, " and "),
                (Role::Emphasis, "it"),
                (Role::Plain, " with "),
                (Role::Code, "code"),
                (Role::Plain, " and a "),
                (Role::Link, "link"),
                (Role::Plain, ". next line"),
            ]
        );
        assert_eq!(lines[4].text(), "• one");
        assert_eq!(lines[5].text(), "  • two");
        assert_eq!(roles(&lines[6])[0], (Role::Marker, "3. "));
        assert_eq!(lines[7].block, Block::Quote);
        assert_eq!(lines[8].block, Block::Rule);
        assert_eq!(lines[9].block, Block::Code);
        assert_eq!(roles(&lines[9])[0], (Role::Keyword, "fn"));
        assert_eq!(lines[10].text(), "snake_case_name stays");
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn an_unclosed_marker_is_text() {
        let lines = markdown("2 * 3 and a ` tick");
        assert_eq!(roles(&lines[0]), vec![(Role::Plain, "2 * 3 and a ` tick")]);
    }

    #[test]
    fn code_is_highlighted_by_language_and_plain_otherwise() {
        let lines = code(
            "let n = 42; // the answer\n/* a\nblock */ let s = \"hi \\\" there\";",
            "rust",
        );
        let first = roles(&lines[0]);
        assert!(first.contains(&(Role::Keyword, "let")), "{first:?}");
        assert!(first.contains(&(Role::Number, "42")), "{first:?}");
        assert!(
            first.contains(&(Role::Comment, "// the answer")),
            "{first:?}"
        );
        assert_eq!(roles(&lines[1]), vec![(Role::Comment, "/* a")]);
        let third = roles(&lines[2]);
        assert_eq!(third[0], (Role::Comment, "block */"));
        assert!(
            third.contains(&(Role::String, "\"hi \\\" there\"")),
            "{third:?}"
        );
        let plain = code("let n = 1", "cobol");
        assert_eq!(roles(&plain[0]), vec![(Role::Plain, "let n = 1")]);
        let unterminated = code("x = 'open", "python");
        assert!(roles(&unterminated[0]).contains(&(Role::String, "'open")));
    }

    #[test]
    fn a_diff_marks_its_lines() {
        let lines = diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n same");
        let r: Vec<Role> = lines.iter().map(|l| l.spans[0].role).collect();
        assert_eq!(
            r,
            [
                Role::Meta,
                Role::Meta,
                Role::Hunk,
                Role::Removed,
                Role::Added,
                Role::Plain
            ]
        );
    }
}
