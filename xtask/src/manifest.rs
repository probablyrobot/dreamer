//! Reader for `deps/baseline.toml`.
//!
//! The file deliberately uses a tiny TOML subset (`[section]` headers and
//! `key = "string"` lines) so that build scripts can read it without a TOML
//! dependency. This reader rejects anything outside that subset.

use std::collections::BTreeMap;

use crate::util::repo_root;

#[derive(Debug, Default)]
pub struct Manifest {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Manifest {
    pub fn load() -> Result<Self, String> {
        let path = repo_root().join("deps").join("baseline.toml");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut manifest = Manifest::default();
        let mut section: Option<String> = None;
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lineno = i + 1;
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let name = name.trim().to_owned();
                manifest.sections.entry(name.clone()).or_default();
                section = Some(name);
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("line {lineno}: expected `key = \"value\"`"))?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .filter(|v| !v.contains('"') && !v.contains('\\'))
                .ok_or_else(|| format!("line {lineno}: values must be plain quoted strings"))?;
            let name = section
                .as_ref()
                .ok_or_else(|| format!("line {lineno}: key outside a section"))?;
            let previous = manifest
                .sections
                .get_mut(name)
                .map(|s| s.insert(key.trim().to_owned(), value.to_owned()));
            if let Some(Some(_)) = previous {
                return Err(format!("line {lineno}: duplicate key `{}`", key.trim()));
            }
        }
        Ok(manifest)
    }

    pub fn get(&self, section: &str, key: &str) -> Result<&str, String> {
        self.sections
            .get(section)
            .and_then(|s| s.get(key))
            .map(String::as_str)
            .ok_or_else(|| format!("deps/baseline.toml: missing [{section}] {key}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    #[test]
    fn reads_sections_and_rejects_non_strings() {
        let m = Manifest::parse("# c\n[a]\nx = \"1\"\n[b]\ny-z = \"two\"\n").expect("parse");
        assert_eq!(m.get("a", "x"), Ok("1"));
        assert_eq!(m.get("b", "y-z"), Ok("two"));
        assert!(m.get("a", "y-z").is_err());
        assert!(Manifest::parse("[a]\nx = 1\n").is_err());
        assert!(Manifest::parse("x = \"1\"\n").is_err());
        assert!(Manifest::parse("[a]\nx = \"1\"\nx = \"2\"\n").is_err());
    }

    #[test]
    fn the_checked_in_baseline_is_complete() {
        let m = Manifest::load().expect("load baseline");
        for key in ["version", "commit", "source_sha256", "deps_sha256"] {
            assert!(!m.get("ghostty", key).expect(key).is_empty());
        }
        assert!(m.get("zig", "version").is_ok());
        for triple in [
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ] {
            assert!(m.get("targets", triple).is_ok(), "{triple}");
        }
    }
}
