//! The environment: the configuration directory as one versioned unit
//! (see `docs/environment.md`, once written).
//!
//! `env.toml` names the environment and the oldest Dreamer that reads it.
//! [`IGNORED`] is what is never versioned: what is private, what is
//! specific to one machine, and what is rebuilt from something else.
//!
//! What is in the configuration directory, and who writes it:
//!
//! | Path | Read by | Written by | Versioned |
//! | --- | --- | --- | --- |
//! | `dreamer.yaml` | every program | the user; the desktop's theme picker | yes |
//! | `themes/` | every program | the user | yes |
//! | `workspaces/` | `dream workspace`, the desktop | the user | yes |
//! | `apps/<id>/` | the desktop, `dreamer-termapp` | `dream app install` | yes |
//! | `apps/.last-good/` | `dream app rollback` | `dream app install` | no: the previous install, kept for rollback |
//! | `apps/.staging-<name>/` | nothing | `dream app install`, for a moment | no |
//! | `apps/.unpack-<pid>/` | `dream app install` | `dream app install FILE.dreamerapp`, for a moment | no |
//! | `extensions.yaml` | the daemon | the user | yes |
//! | `hosts.yaml` | the daemon, the desktop | the user; the desktop's connect dialog | no: host names are private |
//! | `memory/` | the daemon (files memory) | the daemon, agents | no: personal |
//! | `local/` | (reserved) | the user | no: what differs per machine |
//! | `env.toml` | the environment commands | `dream env init`, the user | yes |

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ConfigError;

/// The environment's own file, in the configuration directory.
pub const ENV_FILE: &str = "env.toml";

/// The `env.toml` schema this Dreamer reads.
pub const SCHEMA_VERSION: u32 = 1;

/// Never versioned, as `.gitignore` lines (relative to the configuration
/// directory). `dream env init` writes these.
pub const IGNORED: &[&str] = &[
    // Specific to one machine (see `docs/extensions.md` once the overlay
    // exists).
    "/local/",
    // Private: host names and addresses.
    "/hosts.yaml",
    // Personal memory, written by the daemon and agents.
    "/memory/",
    // The version an app install replaced, kept for `dream app rollback`.
    "/apps/.last-good/",
    // An app being installed, for a moment.
    "/apps/.staging-*/",
    // A package being unpacked to install, for a moment.
    "/apps/.unpack-*/",
    // Editors and operating systems.
    ".DS_Store",
    "*~",
    "*.swp",
    ".*.swp",
    "#*#",
    ".#*",
];

/// `env.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    pub schema_version: u32,
    /// Letters, digits, `-` and `_`.
    pub name: String,
    /// The oldest Dreamer that reads this environment; `None` means any.
    pub dreamer: Option<semver::VersionReq>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    schema_version: u32,
    name: String,
    #[serde(default)]
    dreamer: Option<String>,
}

/// `<config dir>/env.toml`.
pub fn env_path(config_dir: &Path) -> PathBuf {
    config_dir.join(ENV_FILE)
}

/// Line and column (both from 1) of byte `at` in `text`.
fn line_column(text: &str, at: usize) -> (usize, usize) {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rfind('\n').map_or(before.chars().count(), |nl| {
        before[nl + 1..].chars().count()
    }) + 1;
    (line, column)
}

fn error(origin: Option<&Path>, at: &str, message: String) -> ConfigError {
    ConfigError {
        file: origin.map(Path::to_path_buf),
        line: None,
        column: None,
        at: at.to_owned(),
        message,
    }
}

/// Parse and validate `env.toml`.
pub fn parse_env(text: &str, origin: Option<&Path>) -> Result<EnvFile, ConfigError> {
    let raw: Raw = toml::from_str(text).map_err(|e| {
        let (line, column) = e
            .span()
            .map(|s| line_column(text, s.start))
            .map_or((None, None), |(l, c)| (Some(l), Some(c)));
        ConfigError {
            file: origin.map(Path::to_path_buf),
            line,
            column,
            at: String::new(),
            message: e.message().to_owned(),
        }
    })?;
    if raw.schema_version != SCHEMA_VERSION {
        return Err(error(
            origin,
            "schema_version",
            format!(
                "schema version {} is not one this Dreamer reads (it reads {SCHEMA_VERSION})",
                raw.schema_version
            ),
        ));
    }
    if raw.name.is_empty()
        || !raw
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(error(
            origin,
            "name",
            format!(
                "{:?} is not a name: use letters, digits, `-` and `_`",
                raw.name
            ),
        ));
    }
    let dreamer = raw
        .dreamer
        .map(|r| {
            semver::VersionReq::parse(&r).map_err(|e| {
                error(
                    origin,
                    "dreamer",
                    format!("{r:?} is not a version requirement such as \">=0.15\": {e}"),
                )
            })
        })
        .transpose()?;
    Ok(EnvFile {
        schema_version: raw.schema_version,
        name: raw.name,
        dreamer,
    })
}

/// Whether Dreamer `version` reads this environment; the error names both
/// versions.
pub fn compatible(env: &EnvFile, version: &str) -> Result<(), String> {
    let Some(req) = &env.dreamer else {
        return Ok(());
    };
    let v = semver::Version::parse(version)
        .map_err(|e| format!("this Dreamer's version {version:?} does not parse: {e}"))?;
    // A pre-release of this Dreamer counts as its release, so a
    // development build reads environments its release will read.
    let release = semver::Version::new(v.major, v.minor, v.patch);
    if req.matches(&v) || req.matches(&release) {
        Ok(())
    } else {
        Err(format!(
            "this environment needs Dreamer {req}; this is Dreamer {version}"
        ))
    }
}

/// The `env.toml` a new environment starts with.
pub fn starter(name: &str, version: &str) -> String {
    let req = semver::Version::parse(version)
        .map(|v| format!("\ndreamer = \">={}.{}\"", v.major, v.minor))
        .unwrap_or_default();
    format!(
        "# Dreamer's environment: this directory, versioned. See `dream env`.\nschema_version = {SCHEMA_VERSION}\nname = \"{name}\"{req}\n"
    )
}

/// The `.gitignore` a new environment starts with.
pub fn gitignore() -> String {
    let mut out =
        String::from("# Never versioned: private, specific to this machine, or rebuilt.\n");
    for line in IGNORED {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Check `env.toml` in `dir` for `dream config check`: `Ok(None)` when it
/// is absent (the directory is not an environment yet).
pub fn check(dir: &Path, version: &str) -> Result<Option<EnvFile>, String> {
    let path = env_path(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let env = parse_env(&text, Some(&path)).map_err(|e| e.to_string())?;
    compatible(&env, version)?;
    Ok(Some(env))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<EnvFile, ConfigError> {
        parse_env(text, Some(Path::new("env.toml")))
    }

    #[test]
    fn the_starter_file_parses_and_reads_on_its_own_version() {
        let env = parse(&starter("igor", "0.15.0")).expect("parse");
        assert_eq!(env.name, "igor");
        assert_eq!(
            env.dreamer.as_ref().map(ToString::to_string).as_deref(),
            Some(">=0.15")
        );
        compatible(&env, "0.15.0").expect("same version");
        compatible(&env, "0.15.0-dev.3").expect("its development builds");
        compatible(&env, "1.0.0").expect("later");
    }

    #[test]
    fn an_older_dreamer_is_refused_naming_both_versions() {
        let env = parse("schema_version = 1\nname = \"x\"\ndreamer = \">=0.16\"\n").expect("parse");
        let e = compatible(&env, "0.15.2").expect_err("older");
        assert!(e.contains(">=0.16") && e.contains("0.15.2"), "{e}");
    }

    #[test]
    fn mistakes_are_errors_with_a_location() {
        let unknown = parse("schema_version = 1\nname = \"x\"\ncolour = 1\n").expect_err("key");
        assert_eq!(
            (unknown.line, unknown.column),
            (Some(3), Some(1)),
            "{unknown}"
        );
        assert!(unknown.message.contains("colour"), "{unknown}");

        let name = parse("schema_version = 1\nname = \"a b\"\n").expect_err("name");
        assert_eq!(name.at, "name");

        let schema = parse("schema_version = 2\nname = \"x\"\n").expect_err("schema");
        assert_eq!(schema.at, "schema_version");

        let req = parse("schema_version = 1\nname = \"x\"\ndreamer = \"soon\"\n").expect_err("req");
        assert_eq!(req.at, "dreamer");

        let missing = parse("name = \"x\"\n").expect_err("missing");
        assert!(missing.message.contains("schema_version"), "{missing}");
    }

    #[test]
    fn the_ignore_list_keeps_private_and_machine_files_out() {
        let g = gitignore();
        for must in ["/hosts.yaml", "/memory/", "/local/", "/apps/.last-good/"] {
            assert!(g.lines().any(|l| l == must), "{must} missing from\n{g}");
        }
        for never in [
            "dreamer.yaml",
            "themes",
            "workspaces",
            "extensions.yaml",
            "env.toml",
        ] {
            assert!(!g.contains(never), "{never} must be versioned");
        }
    }
}
