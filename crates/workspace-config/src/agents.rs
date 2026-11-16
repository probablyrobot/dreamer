//! `agents:` in `dreamer.yaml`: which agent harness a new session uses, and
//! how each harness is started (see `docs/agents.md`).
//!
//! ```yaml
//! agents:
//!   default: pi                 # pi | claude | codex | a name below
//!   pi:
//!     version: latest           # or 1.5.2
//!     model: openai/gpt-5.5
//!   gemini:
//!     protocol: acp
//!     command: [gemini, --experimental-acp]
//! ```
//!
//! Every key is checked: an unknown one, or one that does not apply to that
//! harness, is refused with its key path.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_norway::Value;

/// The harnesses Dreamer knows without configuration.
pub const BUILT_IN: [&str; 3] = ["pi", "claude", "codex"];

/// The harness used when `agents.default` is not set.
pub const DEFAULT_HARNESS: &str = "pi";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentSettings {
    /// `agents.default`, checked to name a built-in or configured harness.
    pub default: Option<String>,
    /// Every configured block, by name (built-in ones included when given).
    pub harnesses: BTreeMap<String, Harness>,
}

impl AgentSettings {
    /// The harness a new session uses.
    pub fn default_harness(&self) -> &str {
        self.default.as_deref().unwrap_or(DEFAULT_HARNESS)
    }

    /// A harness's block; the built-in ones have an empty block when not
    /// configured.
    pub fn harness(&self, name: &str) -> Option<Harness> {
        self.harnesses
            .get(name)
            .cloned()
            .or_else(|| BUILT_IN.contains(&name).then(Harness::default))
    }

    /// The built-in harnesses, then every configured custom one, each with
    /// its block.
    pub fn all(&self) -> Vec<(String, Harness)> {
        let mut out: Vec<(String, Harness)> = BUILT_IN
            .iter()
            .map(|n| ((*n).to_owned(), self.harness(n).unwrap_or_default()))
            .collect();
        out.extend(
            self.harnesses
                .iter()
                .filter(|(n, _)| !BUILT_IN.contains(&n.as_str()))
                .map(|(n, h)| (n.clone(), h.clone())),
        );
        out
    }

    /// pi's version choice: `latest` unless pinned.
    pub fn pi_version(&self) -> PiVersion {
        self.harnesses
            .get("pi")
            .and_then(|h| h.version.clone())
            .unwrap_or(PiVersion::Latest)
    }
}

/// One harness block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Harness {
    /// The program and its arguments. For pi, without one, Dreamer's
    /// managed install; for claude and codex, the CLI on `PATH`.
    pub command: Option<Vec<String>>,
    /// The model to ask for; without one, the harness's own default.
    pub model: Option<String>,
    /// pi only: the release to keep installed.
    pub version: Option<PiVersion>,
    /// pi only: the agent directory (`PI_CODING_AGENT_DIR`) instead of
    /// Dreamer's own pi profile.
    pub agent_dir: Option<PathBuf>,
    /// Custom harnesses only: how Dreamer talks to it.
    pub protocol: Option<Protocol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiVersion {
    /// The newest release, checked for regularly.
    Latest,
    /// Exactly this release, such as `1.5.2`.
    Exact(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// The Agent Client Protocol on the program's standard streams.
    Acp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessFile {
    command: Option<Vec<String>>,
    model: Option<String>,
    version: Option<Value>,
    agent_dir: Option<String>,
    protocol: Option<Protocol>,
}

/// A release number: `X.Y.Z`, optionally with a `-pre.release` suffix.
pub fn valid_version(v: &str) -> bool {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && (v.len() == core.len()
            || (!pre.is_empty() && pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')))
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A problem at a key path.
pub type Problem = (String, String);

/// Parse the value of `agents:`.
pub fn parse(value: &Value) -> Result<AgentSettings, Problem> {
    let p = |at: &str, m: String| (format!("agents{at}"), m);
    let map = match value {
        Value::Null => return Ok(AgentSettings::default()),
        Value::Mapping(m) => m,
        _ => {
            return Err(p(
                "",
                "must be a map of `default` and harness blocks".into(),
            ));
        }
    };
    let mut out = AgentSettings::default();
    let mut default = None;
    for (k, v) in map {
        let Some(name) = k.as_str() else {
            return Err(p("", format!("{k:?} is not a harness name")));
        };
        if name == "default" {
            let Some(d) = v.as_str() else {
                return Err(p(".default", "must be a harness name".into()));
            };
            default = Some(d.to_owned());
            continue;
        }
        if !valid_name(name) {
            return Err(p(
                &format!(".{name}"),
                "a harness name is letters, digits, `-` and `_`, at most 32".into(),
            ));
        }
        let at = |k: &str| format!(".{name}.{k}");
        let file: HarnessFile = if v.is_null() {
            HarnessFile::default()
        } else {
            serde_norway::from_value(v.clone())
                .map_err(|e| p(&format!(".{name}"), e.to_string()))?
        };
        let built_in = BUILT_IN.contains(&name);
        let mut h = Harness::default();
        if let Some(c) = file.command {
            if c.first().is_none_or(|x| x.trim().is_empty()) {
                return Err(p(
                    &at("command"),
                    "must be a program and its arguments, the program first".into(),
                ));
            }
            h.command = Some(c);
        }
        if let Some(m) = file.model {
            if m.trim().is_empty() {
                return Err(p(&at("model"), "must not be empty".into()));
            }
            h.model = Some(m);
        }
        if let Some(v) = file.version {
            if name != "pi" {
                return Err(p(
                    &at("version"),
                    "only pi is installed by Dreamer; other harnesses use the installed program"
                        .into(),
                ));
            }
            let text = match &v {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            h.version = Some(if text == "latest" {
                PiVersion::Latest
            } else if valid_version(&text) {
                PiVersion::Exact(text)
            } else {
                return Err(p(
                    &at("version"),
                    format!("{text:?} is not `latest` or a release such as 1.5.2"),
                ));
            });
        }
        if let Some(d) = file.agent_dir {
            if name != "pi" {
                return Err(p(&at("agent_dir"), "applies to pi only".into()));
            }
            h.agent_dir = Some(expand_home(&d).ok_or_else(|| {
                p(
                    &at("agent_dir"),
                    format!("{d:?} must be an absolute path or start with ~/"),
                )
            })?);
        }
        if let Some(proto) = file.protocol {
            if built_in {
                return Err(p(
                    &at("protocol"),
                    format!(
                        "{name} is driven by its own protocol; give an ACP adapter its own name"
                    ),
                ));
            }
            h.protocol = Some(proto);
        }
        if !built_in {
            if h.protocol.is_none() {
                return Err(p(
                    &format!(".{name}"),
                    format!(
                        "`{name}` is not pi, claude, or codex; a custom harness needs `protocol: acp` and a `command`"
                    ),
                ));
            }
            if h.command.is_none() {
                return Err(p(
                    &at("command"),
                    "a custom harness needs the command that starts it".into(),
                ));
            }
        }
        out.harnesses.insert(name.to_owned(), h);
    }
    if let Some(d) = default {
        if !BUILT_IN.contains(&d.as_str()) && !out.harnesses.contains_key(&d) {
            return Err(p(
                ".default",
                format!("`{d}` is not pi, claude, codex, or a harness configured under agents"),
            ));
        }
        out.default = Some(d);
    }
    Ok(out)
}

fn expand_home(d: &str) -> Option<PathBuf> {
    if let Some(rest) = d.strip_prefix("~/") {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        return Some(PathBuf::from(home).join(rest));
    }
    let p = PathBuf::from(d);
    p.is_absolute().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(yaml: &str) -> Result<AgentSettings, Problem> {
        let v: Value = serde_norway::from_str(yaml).expect("yaml");
        parse(&v)
    }

    #[test]
    fn without_configuration_pi_is_the_default_and_every_built_in_is_listed() {
        let a = agents("").expect("empty");
        assert_eq!(a.default_harness(), "pi");
        assert_eq!(a.pi_version(), PiVersion::Latest);
        let names: Vec<String> = a.all().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["pi", "claude", "codex"]);
        assert!(a.harness("gemini").is_none());
    }

    #[test]
    fn blocks_configure_each_harness_and_a_custom_one_speaks_acp() {
        let a = agents(
            "default: gemini\npi:\n  version: 1.5.2\n  model: openai/gpt-5.5\n  agent_dir: /opt/pi\nclaude: {}\ncodex:\ngemini:\n  protocol: acp\n  command: [gemini, --experimental-acp]\n",
        )
        .expect("valid");
        assert_eq!(a.default_harness(), "gemini");
        assert_eq!(a.pi_version(), PiVersion::Exact("1.5.2".into()));
        let pi = a.harness("pi").expect("pi");
        assert_eq!(pi.model.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(pi.agent_dir, Some(PathBuf::from("/opt/pi")));
        let g = a.harness("gemini").expect("gemini");
        assert_eq!(g.protocol, Some(Protocol::Acp));
        assert_eq!(
            g.command.as_deref(),
            Some(&["gemini".to_owned(), "--experimental-acp".to_owned()][..])
        );
        assert_eq!(a.all().len(), 4);
        assert_eq!(
            agents("pi: {version: latest}")
                .expect("latest")
                .pi_version(),
            PiVersion::Latest
        );
    }

    #[test]
    fn bad_blocks_are_refused_with_the_key_path() {
        for (yaml, at) in [
            ("pi: {modle: x}", "agents.pi"),
            ("pi: {version: 1.5}", "agents.pi.version"),
            ("pi: {version: newest}", "agents.pi.version"),
            ("pi: {agent_dir: relative/dir}", "agents.pi.agent_dir"),
            ("pi: {command: []}", "agents.pi.command"),
            ("pi: {model: ''}", "agents.pi.model"),
            ("claude: {version: 1.0.0}", "agents.claude.version"),
            ("codex: {agent_dir: /x}", "agents.codex.agent_dir"),
            ("claude: {protocol: acp}", "agents.claude.protocol"),
            ("gemini: {command: [gemini]}", "agents.gemini"),
            ("gemini: {protocol: acp}", "agents.gemini.command"),
            ("gemini: {protocol: grpc, command: [g]}", "agents.gemini"),
            ("default: gemini", "agents.default"),
            ("default: [pi]", "agents.default"),
            ("'bad name': {}", "agents.bad name"),
        ] {
            let (path, why) = agents(yaml).expect_err(yaml);
            assert_eq!(path, at, "{yaml}: {why}");
        }
        let (_, why) = agents("pi: {modle: x}").expect_err("typo");
        assert!(why.contains("modle"), "{why}");
    }

    #[test]
    fn release_numbers_are_strict() {
        for ok in ["1.5.2", "0.0.1", "10.20.30-beta.1"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in [
            "1.5", "v1.5.2", "1.5.2-", "1.5.2/..", "../1.5.2", "1..2", "",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
    }
}
