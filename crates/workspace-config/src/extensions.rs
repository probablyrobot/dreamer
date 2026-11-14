//! Supervised extensions: `<config dir>/extensions.yaml` (see
//! `docs/extensions.md`).
//!
//! Each entry names a program the daemon runs and supervises, and the
//! narrow grant its connection holds. The grant is the user's decision,
//! written here; nothing the extension sends can widen it. Unknown keys
//! and invalid values are errors with a location.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ConfigError;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionGrant {
    /// Methods the extension may call: exact names or `service.*`.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Panes it may read, follow, type into, or act on.
    #[serde(default)]
    pub panes: Vec<u32>,
    /// Every pane, a window, a session, or a host.
    #[serde(default)]
    pub all_panes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extension {
    /// A label, unique among extensions; also its log file's name.
    pub name: String,
    /// The exact program and arguments: no shell.
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub grant: ExtensionGrant,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    extensions: Vec<Extension>,
}

/// `<config dir>/extensions.yaml`.
pub fn extensions_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("extensions.yaml"))
}

fn error(origin: Option<&Path>, at: String, message: String) -> ConfigError {
    ConfigError {
        file: origin.map(Path::to_path_buf),
        line: None,
        column: None,
        at,
        message,
    }
}

pub fn parse_extensions(text: &str, origin: Option<&Path>) -> Result<Vec<Extension>, ConfigError> {
    let file: File = if text.trim().is_empty() {
        File::default()
    } else {
        serde_norway::from_str(text).map_err(|e| {
            let loc = e.location();
            ConfigError {
                file: origin.map(Path::to_path_buf),
                line: loc.as_ref().map(serde_norway::Location::line),
                column: loc.as_ref().map(serde_norway::Location::column),
                at: String::new(),
                message: e.to_string(),
            }
        })?
    };
    let mut seen = std::collections::HashSet::new();
    for (i, x) in file.extensions.iter().enumerate() {
        let at = |field: &str| format!("extensions[{i}].{field}");
        let label = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
        if x.name.is_empty() || !x.name.chars().all(label) || x.name.starts_with('.') {
            return Err(error(
                origin,
                at("name"),
                "must be letters, digits, `-`, `_`, or `.`, not starting with `.`".into(),
            ));
        }
        if !seen.insert(x.name.as_str()) {
            return Err(error(
                origin,
                at("name"),
                format!("`{}` is used by another extension", x.name),
            ));
        }
        if x.argv.is_empty() || x.argv[0].is_empty() {
            return Err(error(origin, at("argv"), "needs the program to run".into()));
        }
        if x.argv.iter().any(|a| a.contains('\0')) {
            return Err(error(
                origin,
                at("argv"),
                "arguments cannot contain NUL".into(),
            ));
        }
        for m in &x.grant.methods {
            let service = m.strip_suffix(".*").unwrap_or(m);
            let ok = !service.is_empty()
                && service
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.')
                && !service.starts_with('.')
                && !service.ends_with('.');
            if !ok {
                return Err(error(
                    origin,
                    at("grant.methods"),
                    format!("`{m}` is not a method name or `service.*`"),
                ));
            }
        }
    }
    Ok(file.extensions)
}

/// The user's extensions: none when the file does not exist, an error when
/// it exists but is invalid.
pub fn load_extensions() -> Result<Vec<Extension>, ConfigError> {
    let Some(path) = extensions_path() else {
        return Ok(Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_extensions(&text, Some(&path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(error(Some(&path), String::new(), e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_parses_with_defaults_and_typos_are_errors() {
        let x = parse_extensions(
            "extensions:\n  - name: watch\n    argv: [/bin/watcher, --quiet]\n    grant:\n      methods: [events.subscribe, messages.*]\n",
            None,
        )
        .expect("parse");
        assert_eq!(x[0].argv, ["/bin/watcher", "--quiet"]);
        assert!(x[0].enabled);
        assert_eq!(x[0].grant.methods, ["events.subscribe", "messages.*"]);
        assert!(!x[0].grant.all_panes);
        for (bad, at) in [
            (
                "extensions:\n  - name: a\n    argv: [x]\n    gran: {}\n",
                "",
            ),
            (
                "extensions:\n  - name: ../a\n    argv: [x]\n",
                "extensions[0].name",
            ),
            (
                "extensions:\n  - name: a\n    argv: []\n",
                "extensions[0].argv",
            ),
            (
                "extensions:\n  - name: a\n    argv: [x]\n  - name: a\n    argv: [y]\n",
                "extensions[1].name",
            ),
            (
                "extensions:\n  - name: a\n    argv: [x]\n    grant: { methods: ['rm -rf'] }\n",
                "extensions[0].grant.methods",
            ),
        ] {
            let e = parse_extensions(bad, None).expect_err(bad);
            assert_eq!(e.at, at, "{bad}");
        }
    }
}
