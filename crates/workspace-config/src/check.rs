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
        path: "clipboard.tmux",
        kind: "sync | from_tmux | to_tmux | off",
        default: "sync",
        doc: "How tmux's paste buffers and the system clipboard follow each other: a copy in tmux reaches the clipboard (under clipboard.write), and a copy in Dreamer becomes tmux's newest buffer.",
        example: "sync",
        live: true,
    },
    Key {
        path: "clipboard.copy_on_select",
        kind: "bool",
        default: "false",
        doc: "A finished mouse selection also goes to the system clipboard. It always goes to tmux's newest buffer (with clipboard.tmux sync or to_tmux), as marking text in tmux does.",
        example: "true",
        live: true,
    },
    Key {
        path: "clipboard.middle_click",
        kind: "paste | off",
        default: "paste",
        doc: "The middle button in a terminal pane that does not track the mouse pastes tmux's newest buffer.",
        example: "paste",
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
    Key {
        path: "agents.default",
        kind: "pi | claude | codex | opencode | name of a harness under agents",
        default: "pi",
        doc: "The agent harness a new session uses. A running session keeps the harness it started with.",
        example: "pi",
        live: false,
    },
    Key {
        path: "agents.pi.version",
        kind: "latest | a release such as 1.5.2",
        default: "latest",
        doc: "The pi release Dreamer installs under its data directory (`dream agents update`). The previous release is kept for `dream agents rollback`.",
        example: "latest",
        live: false,
    },
    Key {
        path: "agents.pi.command",
        kind: "argv list",
        default: "(Dreamer's managed install)",
        doc: "Run this pi instead of the one Dreamer installs, such as your own `[pi]` on PATH.",
        example: "[pi]",
        live: false,
    },
    Key {
        path: "agents.pi.model",
        kind: "model name",
        default: "(the pi profile's provider, openai-codex, and its default model)",
        doc: "The model pi sessions ask for, as pi names it (`provider/model`).",
        example: "openai-codex/gpt-5.5",
        live: false,
    },
    Key {
        path: "agents.pi.agent_dir",
        kind: "absolute path, or ~/...",
        default: "(Dreamer's pi profile in its data directory)",
        doc: "The pi agent directory (PI_CODING_AGENT_DIR) for Dreamer's pi sessions, such as your own ~/.pi/agent.",
        example: "~/.pi/agent",
        live: false,
    },
    Key {
        path: "agents.pi.delegates",
        kind: "list of claude, codex",
        default: "[claude, codex]",
        doc: "The CLIs pi's Dreamer extension may hand tasks to (its claude_code and codex tools), each on its own login. An empty list turns delegation off.",
        example: "[codex]",
        live: false,
    },
    Key {
        path: "agents.claude.command",
        kind: "argv list",
        default: "[claude] (found on PATH)",
        doc: "How Claude Code is started.",
        example: "[claude]",
        live: false,
    },
    Key {
        path: "agents.claude.model",
        kind: "model name",
        default: "(Claude Code's own default)",
        doc: "The model Claude Code sessions ask for.",
        example: "opus",
        live: false,
    },
    Key {
        path: "agents.codex.command",
        kind: "argv list",
        default: "[codex] (found on PATH)",
        doc: "How the Codex CLI is started.",
        example: "[codex]",
        live: false,
    },
    Key {
        path: "agents.codex.model",
        kind: "model name",
        default: "(Codex's own default)",
        doc: "The model Codex sessions ask for.",
        example: "gpt-5.5",
        live: false,
    },
    Key {
        path: "agents.opencode.command",
        kind: "argv list",
        default: "[opencode] (found on PATH)",
        doc: "How opencode is started. Sessions add `acp` to run its ACP server; the drawer runs it as given.",
        example: "[opencode]",
        live: false,
    },
    Key {
        path: "agents.opencode.mode",
        kind: "session mode id",
        default: "(opencode's own default)",
        doc: "The session mode, which for opencode is its primary agent, set with session/set_mode when a session starts. Mode ids are opencode's own.",
        example: "build",
        live: false,
    },
    Key {
        path: "agents.NAME",
        kind: "{protocol: pi | claude | codex | opencode | acp, command, model, mode}",
        default: "(none)",
        doc: "Another harness: an agent that speaks the Agent Client Protocol on its standard streams (protocol: acp, with its command), or another configuration of pi, Claude Code, Codex, or opencode. NAME is letters, digits, - and _; set agents.default to NAME to use it.",
        example: "{protocol: acp, command: [gemini, --experimental-acp]}",
        live: false,
    },
    Key {
        path: "drawer.side",
        kind: "bottom | right | left",
        default: "bottom",
        doc: "Where the harness drawer docks in the window it is shown in. Applies the next time it is shown.",
        example: "bottom",
        live: true,
    },
    Key {
        path: "drawer.size",
        kind: "percent, 10 to 90",
        default: "35",
        doc: "How much of the window's height (bottom) or width (left, right) the drawer takes. A window too small to leave room for the other panes shows the drawer zoomed instead.",
        example: "35",
        live: true,
    },
    Key {
        path: "tiers.cold_after",
        kind: "a wait (90s, 30m, 2h) | never",
        default: "30m",
        doc: "A pane that nothing follows (no window shows it, no tap or filter reads it) and that has written nothing for this long goes Cold: its mirror is dropped until something asks for it again.",
        example: "1h",
        live: true,
    },
    Key {
        path: "tiers.warm_limit",
        kind: "whole number of panes | none",
        default: "128",
        doc: "At most this many Warm panes that nothing follows; beyond it the quietest go Cold. A Warm pane holds about 1 KiB per line of history.",
        example: "256",
        live: true,
    },
    Key {
        path: "tiers.warm_memory",
        kind: "a size (512MiB, 2GiB) | none",
        default: "1GiB",
        doc: "At most this much memory held by all the host's mirrors together, an estimate from each mirror's screen, history, and journal; beyond it the quietest panes that nothing follows go Cold. Followed panes are never demoted, so they alone can hold more.",
        example: "512MiB",
        live: true,
    },
    Key {
        path: "window.decorations",
        kind: "full | none",
        default: "full",
        doc: "Whether desktop windows have the system's title bar and its close, zoom, and minimize buttons. With none the tab strip still moves the window (double-click zooms it), and the window still resizes from its edges.",
        example: "none",
        live: true,
    },
    Key {
        path: "memory.provider",
        kind: "files | sqlite | kosmonaut",
        default: "sqlite",
        doc: "What keeps durable memory (dream memory, the memory.* API): sqlite is one SQLite database for the user on this machine (<data dir>/memory.sqlite), where links are edges and recall also follows them; the first time it reaches a scope it copies in, once, what files kept there; files is one markdown file per memory and a MEMORY.md index, under <config dir>/memory/ for the user and .dreamer/memory/ in a project; kosmonaut keeps memories on a Kosmonaut server (a build with the kosmonaut feature). dream memory import and export copy between files and the others.",
        example: "files",
        live: true,
    },
    Key {
        path: "web.component",
        kind: "ask | auto | never",
        default: "ask",
        doc: "Dreamer Web, the Chromium component web panes need (about 150 MB): ask offers it once at the first start and in each web pane that needs it, and downloads nothing without a yes; auto adds it without asking when something needs it; never offers nothing, and web panes say it is turned off. A copy already on the system is used in place before anything is downloaded.",
        example: "auto",
        live: true,
    },
    Key {
        path: "web.url",
        kind: "address",
        default: "the release",
        doc: "Where to download Dreamer Web from instead of the release: the archive's https, http, or file address, or a directory's that holds it (with its .sha256 beside it). DREAMER_WEB_URL, when set, comes first.",
        example: "https://downloads.example.com/dreamer-web/",
        live: true,
    },
    Key {
        path: "web.downloads.folder",
        kind: "absolute path, or one from ~/",
        default: "~/Downloads (Linux: the XDG download directory)",
        doc: "Where a download in a web pane is saved, once you choose Save in the pane. Without it: ~/Downloads on macOS; on Linux XDG_DOWNLOAD_DIR from user-dirs.dirs, else ~/Downloads. Made if it is not there.",
        example: "~/Downloads/web",
        live: true,
    },
    Key {
        path: "web.color",
        kind: "srgb | display",
        default: "srgb",
        doc: "The color space web panes are drawn in. srgb matches the rest of the window: wide-gamut colors (Display P3 images, video, CSS color(display-p3 ...)) are clamped to sRGB. display draws pages in the display's own color space, so they keep their saturation on a wide-gamut display. Chromium reads it when it starts (the first web pane), so a change applies after Dreamer restarts.",
        example: "display",
        live: false,
    },
    Key {
        path: "memory.sqlite.path",
        kind: "absolute path or ~/path",
        default: "<data dir>/memory.sqlite",
        doc: "The sqlite provider's database file: one per user, every project's memories in it, each tagged with its project. Made on the first memory call.",
        example: "~/.local/share/dreamer/memory.sqlite",
        live: true,
    },
    Key {
        path: "memory.kosmonaut.url",
        kind: "grpc://host:port | unix:/path",
        default: "none (required with provider: kosmonaut)",
        doc: "The Kosmonaut server: grpc://host:port over the network, or unix:/path for a socket on this machine. When the server cannot be reached, memory calls fail with memory unavailable and agent sessions go on without memory. Kosmonaut runs only as a server; for a database on this machine use provider: sqlite.",
        example: "grpc://memory.example:7687",
        live: true,
    },
    Key {
        path: "memory.kosmonaut.tls",
        kind: "boolean",
        default: "true (false for unix:)",
        doc: "TLS to the server, checked against the Mozilla root certificates. false is allowed only to this machine (localhost, 127.0.0.1, [::1]).",
        example: "true",
        live: true,
    },
    Key {
        path: "memory.kosmonaut.token_env",
        kind: "environment variable name",
        default: "none (no token)",
        doc: "The environment variable in dreamerd's environment that holds the server's bearer token, read at each call. The token itself is never in the settings. Without it, no token is sent (a server with authentication off).",
        example: "KOSMONAUT_TOKEN",
        live: true,
    },
    Key {
        path: "memory.embedder",
        kind: "{kind: static, path} | {kind: local, command, dimension, timeout} | {kind: openai, model, key_env, url, dimension, timeout}",
        default: "none (no vectors)",
        doc: "Optional, for sqlite and kosmonaut: what turns memories into vectors, so recall also finds memories near the query in meaning. static reads word vectors from a file (word2vec text format); local runs a program that reads a JSON array of texts and prints a JSON array of vectors; openai sends the texts to OpenAI's embeddings API with the key in the environment variable key_env (url, default https://api.openai.com/v1, must be https unless it is this machine; timeout per request, default 30s). Without it, recall is by words and links. It was memory.kosmonaut.embedder, which is still read for now, with a note.",
        example: "{kind: openai, model: text-embedding-3-small, key_env: OPENAI_API_KEY}",
        live: true,
    },
];

/// What the live configuration files look like now: `dreamer.yaml` and
/// every file in `themes/`, each with its length and modification time.
/// Programs that apply the configuration live compare stamps to see a
/// change (a few `stat` calls; cheap enough to take every second).
pub fn stamp(dir: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let one = |p: PathBuf| {
        let m = std::fs::metadata(&p).ok();
        let len = m.as_ref().map_or(0, std::fs::Metadata::len);
        let modified = m.and_then(|m| m.modified().ok());
        (p, len, modified)
    };
    let mut out = vec![one(dir.join("dreamer.yaml"))];
    if let Ok(entries) = std::fs::read_dir(dir.join("themes")) {
        let mut themes: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        themes.sort();
        out.extend(themes.into_iter().map(one));
    }
    out
}

/// One file's verdict.
#[derive(Debug)]
pub struct Checked {
    pub file: PathBuf,
    /// `None` when the file is fine (or absent, which means defaults).
    pub problem: Option<String>,
    /// The file exists.
    pub exists: bool,
    /// What should change though the file works: a setting that moved.
    pub notes: Vec<String>,
}

/// Check every file Dreamer reads from configuration directory `dir`:
/// `dreamer.yaml` (including the theme it names), every theme file in
/// `themes/`, and `hosts.yaml`. Each is checked with the same parser the
/// programs use.
pub fn check(dir: &Path) -> Vec<Checked> {
    let mut out = Vec::new();
    let settings = dir.join("dreamer.yaml");
    out.push(match std::fs::read_to_string(&settings) {
        Ok(text) => {
            let parsed = crate::settings::parse_settings(&text, Some(&settings));
            Checked {
                notes: parsed
                    .as_ref()
                    .map(|s| s.memory.notes.clone())
                    .unwrap_or_default(),
                problem: parsed.err().map(|e| e.to_string()),
                file: settings,
                exists: true,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Checked {
            file: settings,
            problem: None,
            exists: false,
            notes: Vec::new(),
        },
        Err(e) => Checked {
            problem: Some(e.to_string()),
            file: settings,
            exists: true,
            notes: Vec::new(),
        },
    });
    let themes = dir.join("themes");
    let (listed, problems) = crate::themes::all(Some(&themes));
    for l in listed.into_iter().filter_map(|l| l.file) {
        out.push(Checked {
            file: l,
            problem: None,
            exists: true,
            notes: Vec::new(),
        });
    }
    for p in problems {
        // "<file>: <why>"
        let (file, why) = p.split_once(": ").unwrap_or((&p, ""));
        out.push(Checked {
            file: PathBuf::from(file),
            problem: Some(why.to_owned()),
            exists: true,
            notes: Vec::new(),
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
            notes: Vec::new(),
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
            // Keys that apply only beside another.
            let beside = match k.path {
                "memory.kosmonaut.tls" | "memory.kosmonaut.token_env" => {
                    "url: grpc://memory.example:7687, "
                }
                _ => "",
            };
            let mut line = format!("{beside}{last}: {}", k.example);
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
        // A moved key still works, with a note.
        std::fs::write(
            d.join("dreamer.yaml"),
            "memory:\n  kosmonaut:\n    embedder: {kind: static, path: /v/words.vec}\n",
        )
        .expect("write");
        let r = check(&d);
        assert!(r.iter().all(|c| c.problem.is_none()), "{r:?}");
        assert!(
            r[0].notes.iter().any(|n| n.contains("memory.embedder")),
            "{r:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
