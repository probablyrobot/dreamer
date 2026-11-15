//! The configuration as a whole: what every key of `dreamer.yaml` means
//! ([`SCHEMA`]), and a check of every file Dreamer reads from the
//! configuration directory ([`check`]). Both are what `dream config schema`
//! and `dream config check` print, so a person or an agent can change the
//! configuration with confidence: look the key up, edit, check.

use std::path::{Path, PathBuf};

/// One key of `dreamer.yaml`.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    /// Dotted path: `font.size`.
    pub path: &'static str,
    /// The kind of value and its allowed range or choices.
    pub kind: &'static str,
    /// The value without the key.
    pub default: &'static str,
    /// What it does.
    pub doc: &'static str,
    /// A valid value, as YAML.
    pub example: &'static str,
    /// Whether a change applies to running programs at once (live), or
    /// only to what starts next.
    pub live: bool,
}

/// Every key `dreamer.yaml` accepts. A test checks that all the examples
/// together form a valid file.
pub const SCHEMA: &[Key] = &[
    Key {
        path: "appearance",
        kind: "system | dark | light",
        default: "system",
        doc: "Dark or light for the whole window, terminals and chrome alike. Without terminal.theme it picks dreamer-dark or dreamer-light; system follows the OS.",
        example: "dark",
        live: true,
    },
    Key {
        path: "terminal.theme",
        kind: "theme name (see `dream themes`)",
        default: "dreamer-dark or dreamer-light, by appearance",
        doc: "The color scheme of every terminal; the chrome is drawn from it. A built-in name, or the name of a file in <config dir>/themes/ (alacritty-theme TOML, Ghostty, or kitty format).",
        example: "catppuccin-mocha",
        live: true,
    },
    Key {
        path: "terminal.cursor.style",
        kind: "block | bar | underline",
        default: "block",
        doc: "The cursor a program gets until it sets its own.",
        example: "bar",
        live: true,
    },
    Key {
        path: "terminal.cursor.blink",
        kind: "true | false",
        default: "false",
        doc: "Whether that cursor blinks.",
        example: "false",
        live: true,
    },
    Key {
        path: "font.family",
        kind: "font family name",
        default: "JetBrainsMono Nerd Font Mono",
        doc: "The terminal's font, and the chrome's. A monospace family installed on the system (or bundled).",
        example: "JetBrainsMono Nerd Font Mono",
        live: true,
    },
    Key {
        path: "font.size",
        kind: "number, 6 to 72 (points)",
        default: "13",
        doc: "The terminal's font size.",
        example: "14",
        live: true,
    },
    Key {
        path: "font.line_height",
        kind: "number, 0.8 to 3.0 (times the font size)",
        default: "1.2",
        doc: "The height of a terminal row.",
        example: "1.25",
        live: true,
    },
    Key {
        path: "font.letter_spacing",
        kind: "number, -3 to 10 (pixels)",
        default: "0",
        doc: "Extra space between terminal columns.",
        example: "0",
        live: true,
    },
    Key {
        path: "links.schemes",
        kind: "list of URI schemes",
        default: "[http, https, mailto]",
        doc: "Which hyperlinks a modifier-click may open.",
        example: "[http, https, mailto, file]",
        live: true,
    },
    Key {
        path: "clipboard.write",
        kind: "focused | allow | deny",
        default: "focused",
        doc: "Whether programs may write the system clipboard (OSC 52): only the focused pane's, any pane's, or none.",
        example: "focused",
        live: true,
    },
    Key {
        path: "fallback.launch",
        kind: "argv list with one \"{command}\" element",
        default: "(none)",
        doc: "The terminal to open the attach command in, for what Dreamer does not draw (tmux modes, floating panes).",
        example: "[kitty, -e, \"{command}\"]",
        live: true,
    },
    Key {
        path: "providers.default",
        kind: "claude | codex | claude-api | openai-api",
        default: "(the first ready CLI: claude, then codex)",
        doc: "The agent provider used when a pane or command names none. An API adapter is used only when configured below.",
        example: "claude",
        live: false,
    },
    Key {
        path: "providers.claude_api",
        kind: "{model, key, base_url, max_tokens}",
        default: "(not configured)",
        doc: "The Claude API adapter (metered). model is required; key is env (ANTHROPIC_API_KEY), env:NAME, or keychain, never the key itself; max_tokens 1 to 200000 (default 4096).",
        example: "{model: claude-opus-5-5, key: env}",
        live: false,
    },
    Key {
        path: "providers.openai_api",
        kind: "{model, key, base_url, max_tokens}",
        default: "(not configured)",
        doc: "The OpenAI API adapter (metered), as providers.claude_api (key env is OPENAI_API_KEY).",
        example: "{model: gpt-5, key: env}",
        live: false,
    },
];

/// One file's verdict.
#[derive(Debug)]
pub struct Checked {
    pub file: PathBuf,
    /// `None` when the file is fine (or absent, which means defaults).
    pub problem: Option<String>,
    /// The file exists.
    pub exists: bool,
}

/// Check every file Dreamer reads from configuration directory `dir`:
/// `dreamer.yaml` (including the theme it names), every theme file in
/// `themes/`, and `hosts.yaml`. Each is checked with the same parser the
/// programs use.
pub fn check(dir: &Path) -> Vec<Checked> {
    let mut out = Vec::new();
    let settings = dir.join("dreamer.yaml");
    out.push(match std::fs::read_to_string(&settings) {
        Ok(text) => Checked {
            problem: crate::settings::parse_settings(&text, Some(&settings))
                .err()
                .map(|e| e.to_string()),
            file: settings,
            exists: true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Checked {
            file: settings,
            problem: None,
            exists: false,
        },
        Err(e) => Checked {
            problem: Some(e.to_string()),
            file: settings,
            exists: true,
        },
    });
    let themes = dir.join("themes");
    let (listed, problems) = crate::themes::all(Some(&themes));
    for l in listed.into_iter().filter_map(|l| l.file) {
        out.push(Checked {
            file: l,
            problem: None,
            exists: true,
        });
    }
    for p in problems {
        // "<file>: <why>"
        let (file, why) = p.split_once(": ").unwrap_or((&p, ""));
        out.push(Checked {
            file: PathBuf::from(file),
            problem: Some(why.to_owned()),
            exists: true,
        });
    }
    let hosts = dir.join("hosts.yaml");
    if hosts.exists() {
        out.push(Checked {
            problem: std::fs::read_to_string(&hosts)
                .map_err(|e| e.to_string())
                .and_then(|t| {
                    crate::hosts::parse_hosts(&t, Some(&hosts))
                        .map(drop)
                        .map_err(|e| e.to_string())
                })
                .err(),
            file: hosts,
            exists: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every documented key, at its example value, makes a valid file:
    /// the schema describes keys the parser accepts.
    #[test]
    fn every_documented_key_is_accepted_at_its_example() {
        let mut yaml = String::new();
        for k in SCHEMA {
            let parts: Vec<&str> = k.path.split('.').collect();
            let (last, parents) = parts.split_last().expect("path");
            let mut line = format!("{last}: {}", k.example);
            for p in parents.iter().rev() {
                line = format!("{p}: {{{line}}}");
            }
            yaml.push_str(&line);
            yaml.push('\n');
            crate::settings::parse_settings(&line, None)
                .unwrap_or_else(|e| panic!("{}: {e}", k.path));
        }
        assert!(
            SCHEMA
                .iter()
                .all(|k| !k.doc.is_empty() && !k.kind.is_empty())
        );
    }

    #[test]
    fn a_broken_settings_file_and_a_broken_theme_are_each_reported() {
        let d = std::env::temp_dir().join(format!("drm-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("themes")).expect("dir");
        std::fs::write(d.join("dreamer.yaml"), "font:\n  size: 99\n").expect("write");
        std::fs::write(d.join("themes/half.toml"), "[colors.primary]\n").expect("write");
        let r = check(&d);
        let bad: Vec<&Checked> = r.iter().filter(|c| c.problem.is_some()).collect();
        assert_eq!(bad.len(), 2, "{r:?}");
        assert!(
            bad[0]
                .problem
                .as_deref()
                .unwrap_or("")
                .contains("font.size"),
            "{r:?}"
        );
        std::fs::write(d.join("dreamer.yaml"), "appearance: light\n").expect("write");
        std::fs::remove_file(d.join("themes/half.toml")).expect("rm");
        assert!(check(&d).iter().all(|c| c.problem.is_none()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
