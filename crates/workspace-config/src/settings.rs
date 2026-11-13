//! User settings: `<config dir>/dreamer.yaml` (see `docs/settings.md`).
//!
//! Every field is optional; a missing file means all defaults. Unknown keys
//! and out-of-range values are errors with the file, line, and column, so a
//! typo never silently falls back to a default.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ConfigError;

/// JetBrains Mono (IntelliJ's font) with Nerd Font icons, in the variant
/// whose icons stay one cell wide.
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const DEFAULT_LINE_HEIGHT: f32 = 1.2;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub font: FontSettings,
    pub links: LinkSettings,
}

/// Which hyperlink schemes a modifier-click may open.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSettings {
    /// Lowercase scheme names.
    pub schemes: Vec<String>,
}

pub const DEFAULT_LINK_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

impl LinkSettings {
    /// Whether `uri` has an allowed scheme. A URI with no scheme is refused.
    pub fn allows(&self, uri: &str) -> bool {
        let Some((scheme, _)) = uri.split_once(':') else {
            return false;
        };
        let scheme = scheme.to_ascii_lowercase();
        self.schemes.contains(&scheme)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontSettings {
    pub family: String,
    /// Points (logical pixels).
    pub size: f32,
    /// Multiple of the font size.
    pub line_height: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font: FontSettings {
                family: DEFAULT_FONT_FAMILY.into(),
                size: DEFAULT_FONT_SIZE,
                line_height: DEFAULT_LINE_HEIGHT,
            },
            links: LinkSettings {
                schemes: DEFAULT_LINK_SCHEMES.map(String::from).to_vec(),
            },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    font: FontFile,
    #[serde(default)]
    links: LinksFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinksFile {
    schemes: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FontFile {
    family: Option<String>,
    size: Option<f32>,
    line_height: Option<f32>,
}

/// `<config dir>/dreamer.yaml`.
pub fn settings_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("dreamer.yaml"))
}

fn error(origin: Option<&Path>, at: &str, message: String) -> ConfigError {
    ConfigError {
        file: origin.map(Path::to_path_buf),
        line: None,
        column: None,
        at: at.into(),
        message,
    }
}

pub fn parse_settings(text: &str, origin: Option<&Path>) -> Result<Settings, ConfigError> {
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
    let mut s = Settings::default();
    if let Some(family) = file.font.family {
        if family.trim().is_empty() {
            return Err(error(origin, "font.family", "must not be empty".into()));
        }
        s.font.family = family;
    }
    if let Some(size) = file.font.size {
        if !(6.0..=72.0).contains(&size) {
            return Err(error(
                origin,
                "font.size",
                format!("{size} is outside 6 to 72 points"),
            ));
        }
        s.font.size = size;
    }
    if let Some(lh) = file.font.line_height {
        if !(0.8..=3.0).contains(&lh) {
            return Err(error(
                origin,
                "font.line_height",
                format!("{lh} is outside 0.8 to 3.0"),
            ));
        }
        s.font.line_height = lh;
    }
    if let Some(schemes) = file.links.schemes {
        let valid = |x: &str| {
            x.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && x.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        };
        if let Some(bad) = schemes.iter().find(|x| !valid(x)) {
            return Err(error(
                origin,
                "links.schemes",
                format!("{bad:?} is not a URI scheme name"),
            ));
        }
        s.links.schemes = schemes.iter().map(|x| x.to_ascii_lowercase()).collect();
    }
    Ok(s)
}

/// The user's settings: defaults when the file does not exist, an error
/// when it exists but is invalid.
pub fn load_settings() -> Result<Settings, ConfigError> {
    let Some(path) = settings_path() else {
        return Ok(Settings::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_settings(&text, Some(&path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(error(Some(&path), "", e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_jetbrains_mono_nerd_font_at_13_points() {
        let s = parse_settings("", None).expect("empty");
        assert_eq!(s, Settings::default());
        assert_eq!(s.font.family, "JetBrainsMono Nerd Font Mono");
        assert_eq!(s.font.size, 13.0);
    }

    #[test]
    fn fields_override_defaults_one_by_one() {
        let s = parse_settings("font:\n  size: 15\n", None).expect("size");
        assert_eq!(s.font.size, 15.0);
        assert_eq!(s.font.family, DEFAULT_FONT_FAMILY);
        let s = parse_settings("font: { family: Menlo, line_height: 1.4 }", None).expect("family");
        assert_eq!((s.font.family.as_str(), s.font.line_height), ("Menlo", 1.4));
    }

    #[test]
    fn typos_and_bad_values_are_errors_with_a_location() {
        let e = parse_settings("font:\n  sise: 15\n", None).expect_err("typo");
        assert_eq!(e.line, Some(2), "{e}");
        assert!(e.message.contains("sise"), "{e}");
        let e = parse_settings("font: { size: 200 }", None).expect_err("range");
        assert_eq!(e.at, "font.size");
        assert!(parse_settings("font: { family: '  ' }", None).is_err());
        assert!(parse_settings("colors: {}", None).is_err());
        let e = parse_settings("links: { schemes: ['https:'] }", None).expect_err("scheme");
        assert_eq!(e.at, "links.schemes");
    }

    #[test]
    fn only_listed_link_schemes_open() {
        let d = Settings::default().links;
        assert!(d.allows("https://example.com"));
        assert!(d.allows("HTTP://example.com"));
        assert!(d.allows("mailto:someone@example.com"));
        assert!(!d.allows("file:///etc/passwd"));
        assert!(!d.allows("javascript:alert(1)"));
        assert!(!d.allows("no-scheme-here"));
        let s = parse_settings("links: { schemes: [https, SSH] }", None).expect("schemes");
        assert!(s.links.allows("ssh://host") && !s.links.allows("http://x"));
    }
}
