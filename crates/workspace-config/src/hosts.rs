//! Host profiles: `<config dir>/hosts.yaml` (see `docs/remote.md`).
//!
//! A profile says how the local daemon reaches another host's daemon over
//! SSH. It holds no credentials: keys, agents, and `known_hosts` stay in
//! the user's SSH facilities. Unknown keys and invalid values are errors
//! with a location, so a typo never silently changes how a host is reached.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ConfigError;

/// Whether a dropped connection is retried by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reconnect {
    /// Retry with growing delays, except after failures retrying cannot fix.
    #[default]
    Auto,
    /// Only on an explicit connect or reconnect.
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    /// The label, unique among profiles.
    pub name: String,
    /// SSH destination: a host alias, `user@host`, or an `ssh://` URI.
    pub destination: String,
    /// The remote `dream` executable; default `dream` on the remote `PATH`.
    #[serde(default)]
    pub dream: Option<String>,
    #[serde(default)]
    pub tmux: Option<String>,
    #[serde(default)]
    pub tmux_socket: Option<String>,
    #[serde(default)]
    pub tmux_socket_path: Option<String>,
    /// The workspace namespace on that host.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Labels for grouping hosts in the desktop.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub reconnect: Reconnect,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    hosts: Vec<HostProfile>,
}

/// `<config dir>/hosts.yaml`.
pub fn hosts_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("hosts.yaml"))
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

pub fn parse_hosts(text: &str, origin: Option<&Path>) -> Result<Vec<HostProfile>, ConfigError> {
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
    for (i, h) in file.hosts.iter().enumerate() {
        let at = |field: &str| format!("hosts[{i}].{field}");
        if h.name.trim().is_empty() || h.name.chars().any(char::is_control) {
            return Err(error(
                origin,
                at("name"),
                "must be a non-empty label".into(),
            ));
        }
        if !seen.insert(h.name.as_str()) {
            return Err(error(
                origin,
                at("name"),
                format!("`{}` is used by another host", h.name),
            ));
        }
        let d = &h.destination;
        if d.is_empty()
            || d.starts_with('-')
            || d.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(error(
                origin,
                at("destination"),
                format!("`{d}` is not a usable SSH destination"),
            ));
        }
        if h.tmux_socket.is_some() && h.tmux_socket_path.is_some() {
            return Err(error(
                origin,
                at("tmux_socket"),
                "give tmux_socket or tmux_socket_path, not both".into(),
            ));
        }
        let words = [&h.dream, &h.tmux, &h.tmux_socket, &h.tmux_socket_path];
        if words.iter().any(|w| {
            w.as_deref()
                .is_some_and(|w| w.is_empty() || w.chars().any(char::is_control))
        }) {
            return Err(error(
                origin,
                at("dream"),
                "paths and socket names must be non-empty, without control characters".into(),
            ));
        }
    }
    Ok(file.hosts)
}

/// The user's host profiles: none when the file does not exist, an error
/// when it exists but is invalid.
pub fn load_hosts() -> Result<Vec<HostProfile>, ConfigError> {
    let Some(path) = hosts_path() else {
        return Ok(Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_hosts(&text, Some(&path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(error(Some(&path), String::new(), e.to_string())),
    }
}

/// One YAML scalar, single-quoted (a `'` is doubled).
fn yaml_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The file text for `profiles`, after the comment lines `header`.
pub fn render_hosts(header: &str, profiles: &[HostProfile]) -> String {
    let mut out = String::from(header);
    if profiles.is_empty() {
        out.push_str("hosts: []\n");
        return out;
    }
    out.push_str("hosts:\n");
    for p in profiles {
        out.push_str(&format!("  - name: {}\n", yaml_str(&p.name)));
        out.push_str(&format!("    destination: {}\n", yaml_str(&p.destination)));
        for (key, value) in [
            ("dream", &p.dream),
            ("tmux", &p.tmux),
            ("tmux_socket", &p.tmux_socket),
            ("tmux_socket_path", &p.tmux_socket_path),
            ("workspace", &p.workspace),
        ] {
            if let Some(v) = value {
                out.push_str(&format!("    {key}: {}\n", yaml_str(v)));
            }
        }
        if !p.tags.is_empty() {
            let tags: Vec<String> = p.tags.iter().map(|t| yaml_str(t)).collect();
            out.push_str(&format!("    tags: [{}]\n", tags.join(", ")));
        }
        if p.reconnect == Reconnect::Manual {
            out.push_str("    reconnect: manual\n");
        }
    }
    out
}

/// The comment lines at the top of a file, kept when it is rewritten.
fn leading_comments(text: &str) -> String {
    text.lines()
        .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
        .filter(|l| l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// Replace the profiles in `path`. The new text is validated with the same
/// rules as reading before anything is written, and written atomically.
pub fn save_hosts(path: &Path, profiles: &[HostProfile]) -> Result<(), ConfigError> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let header = leading_comments(&old);
    let text = render_hosts(&header, profiles);
    parse_hosts(&text, Some(path))?;
    let dir = path
        .parent()
        .ok_or_else(|| error(Some(path), String::new(), "no directory".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| error(Some(path), String::new(), e.to_string()))?;
    let tmp = dir.join(format!(".hosts.yaml.{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| error(Some(path), String::new(), e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        error(Some(path), String::new(), e.to_string())
    })
}

/// Add `profile`, or replace the one named `replacing` (keeping its place).
pub fn put_host(
    path: &Path,
    profile: HostProfile,
    replacing: Option<&str>,
) -> Result<(), ConfigError> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut profiles = parse_hosts(&text, Some(path))?;
    let at = replacing
        .or(Some(profile.name.as_str()))
        .and_then(|n| profiles.iter().position(|p| p.name == n));
    match at {
        Some(i) => profiles[i] = profile,
        None => profiles.push(profile),
    }
    save_hosts(path, &profiles)
}

/// Remove the profile called `name`; `false` when there is none.
pub fn remove_host(path: &Path, name: &str) -> Result<bool, ConfigError> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut profiles = parse_hosts(&text, Some(path))?;
    let before = profiles.len();
    profiles.retain(|p| p.name != name);
    if profiles.len() == before {
        return Ok(false);
    }
    save_hosts(path, &profiles)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_with_every_field_parses() {
        let hosts = parse_hosts(
            "hosts:
  - name: devbox
    destination: me@devbox
    dream: ~/.local/bin/dream
    tmux_socket: work
    workspace: default
    tags: [work, linux]
    reconnect: manual
  - name: pi
    destination: pi
",
            None,
        )
        .expect("valid");
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].reconnect, Reconnect::Manual);
        assert_eq!(hosts[0].tags, ["work", "linux"]);
        assert_eq!(hosts[1].reconnect, Reconnect::Auto);
        assert_eq!(parse_hosts("", None).expect("empty"), []);
    }

    #[test]
    fn profiles_round_trip_through_the_file_and_bad_ones_are_never_written() {
        let dir = std::env::temp_dir().join(format!("drm-hosts-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hosts.yaml");
        std::fs::write(
            &path,
            "# my hosts\n# keep me\nhosts:\n  - {name: a, destination: a}\n",
        )
        .expect("seed");
        let tricky = HostProfile {
            name: "it's mine".into(),
            destination: "me@devbox".into(),
            dream: Some("~/.local/bin/dream".into()),
            tmux: None,
            tmux_socket: Some("work: #1".into()),
            tmux_socket_path: None,
            workspace: None,
            tags: vec!["lab".into(), "a, b".into()],
            reconnect: Reconnect::Manual,
        };
        put_host(&path, tricky.clone(), None).expect("add");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# my hosts\n# keep me\n"), "{text}");
        let back = parse_hosts(&text, None).expect("valid");
        assert_eq!(back.len(), 2);
        assert_eq!(back[1], tricky);
        // Edit in place, keeping order.
        let mut renamed = tricky.clone();
        renamed.name = "devbox".into();
        put_host(&path, renamed, Some("it's mine")).expect("edit");
        let names: Vec<String> = parse_hosts(&std::fs::read_to_string(&path).expect("read"), None)
            .expect("valid")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["a", "devbox"]);
        // An invalid profile leaves the file as it was.
        let before = std::fs::read_to_string(&path).expect("read");
        let mut bad = tricky;
        bad.destination = "-oProxyCommand=x".into();
        let e = put_host(&path, bad, None).expect_err("refused");
        assert!(e.at.ends_with(".destination"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
        assert!(remove_host(&path, "a").expect("remove"));
        assert!(!remove_host(&path, "a").expect("gone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typos_and_bad_values_are_errors_with_a_location() {
        let e = parse_hosts(
            "hosts:\n  - name: a\n    destinaton: a\n",
            Some(Path::new("h.yaml")),
        )
        .expect_err("typo");
        assert_eq!(e.line, Some(3), "{e}");
        assert!(e.to_string().starts_with("h.yaml:3:"), "{e}");
        let e = parse_hosts(
            "hosts:\n  - name: a\n    destination: a\n    reconnect: often\n",
            None,
        )
        .expect_err("bad policy");
        assert_eq!(e.line, Some(4), "{e}");
        for (text, at) in [
            (
                "hosts:\n  - {name: a, destination: -oProxyCommand=x}\n",
                "hosts[0].destination",
            ),
            (
                "hosts:\n  - {name: a, destination: a}\n  - {name: a, destination: b}\n",
                "hosts[1].name",
            ),
            (
                "hosts:\n  - {name: a, destination: a, tmux_socket: s, tmux_socket_path: /p}\n",
                "hosts[0].tmux_socket",
            ),
            ("hosts:\n  - {name: '', destination: a}\n", "hosts[0].name"),
        ] {
            assert_eq!(parse_hosts(text, None).expect_err(text).at, at);
        }
    }
}
