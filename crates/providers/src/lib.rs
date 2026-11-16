//! Agent providers (spec 13.5): one contract over the installed Claude Code
//! and Codex CLIs, run in non-interactive mode as supervised child
//! processes on the user's own login.
//!
//! - **Subscription first, never a metered fallback.** The child never
//!   sees `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, or
//!   `CODEX_API_KEY`, so a CLI can only use its own login. A run whose CLI
//!   still reports an API key (from its own settings) is stopped as
//!   [`Failure::Metered`] unless the request allows it.
//! - **Credentials stay where they are.** Login state comes from the CLI's
//!   own status command; nothing here reads a credential store.
//! - **Tools through MCP.** Dreamer's tools reach the CLI as one MCP
//!   server, `dream mcp` (spec 12.6), scoped by its arguments. The CLI's
//!   own built-in tools are turned off where the CLI allows it.
//! - **Same contract.** Every adapter yields the same [`Event`]s, can be
//!   cancelled (its whole process group), and has a [`Capabilities`]
//!   record. Callers never branch on the provider.

mod api;
mod claude;
mod codex;
mod mcp;
pub mod pi;
pub mod pi_profile;
pub mod pi_rpc;
mod run;
pub mod session;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use api::{Api, ApiConfig, KeySource, start_api};
pub use run::{Run, start, start_program};

/// Any provider: a CLI on the user's login, or an API adapter that the
/// user configured. Callers start either the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Cli(Kind),
    Api(ApiConfig),
}

impl Provider {
    /// Who pays for a run, as the user is told before every run.
    pub fn billed_to(&self) -> &'static str {
        match self {
            Provider::Cli(k) => k.billed_to(),
            Provider::Api(_) => "the configured API key (metered)",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Provider::Cli(k) => k.name(),
            Provider::Api(c) => c.api.name(),
        }
    }

    /// Start `req` on this provider. Must be called inside a Tokio runtime.
    pub fn start(&self, req: &Request) -> Result<Run, Failure> {
        match self {
            Provider::Cli(k) => start(*k, req),
            Provider::Api(c) => Ok(start_api(c, req)),
        }
    }

    pub fn capabilities(&self) -> Capabilities {
        match self {
            Provider::Cli(k) => k.capabilities(),
            Provider::Api(_) => Capabilities {
                streams_text: true,
                tools: true,
                cancel: true,
                usage: true,
                cost: false,
            },
        }
    }
}

/// Whether a configured API adapter can run: its key is there (it is read,
/// never shown) and `curl` is installed.
pub async fn probe_api(cfg: &ApiConfig) -> State {
    if locate_in_path("curl").is_none() {
        return State::Unknown("curl is not installed".into());
    }
    match api::key(&cfg.key).await {
        Ok(_) => State::Ready,
        Err(why) => State::Unknown(format!("no API key: {why}")),
    }
}

/// The first file called `name` in a `PATH` directory.
pub fn locate_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Which CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    ClaudeCli,
    CodexCli,
}

impl Kind {
    /// Who pays for a run on this CLI: the user's own login, never Dreamer.
    pub fn billed_to(self) -> &'static str {
        match self {
            Kind::ClaudeCli => "your own Claude Code login",
            Kind::CodexCli => "your own Codex login",
        }
    }
}

/// The default order: Claude Code, then Codex.
pub const ORDER: [Kind; 2] = [Kind::ClaudeCli, Kind::CodexCli];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::ClaudeCli => "claude",
            Kind::CodexCli => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        ORDER.into_iter().find(|k| k.name() == s)
    }

    /// The executable's name on `PATH`, and the variable that overrides it.
    fn program(self) -> (&'static str, &'static str) {
        match self {
            Kind::ClaudeCli => ("claude", "DREAMER_CLAUDE"),
            Kind::CodexCli => ("codex", "DREAMER_CODEX"),
        }
    }

    /// The versions this adapter was tested with, and the major version
    /// whose non-interactive flags and output it reads.
    pub fn tested(self) -> (&'static str, u32) {
        match self {
            Kind::ClaudeCli => ("2.1.325", 2),
            // Built from Codex's documented `exec --json` events; not run
            // against a real Codex yet.
            Kind::CodexCli => ("none", 0),
        }
    }

    pub fn capabilities(self) -> Capabilities {
        match self {
            Kind::ClaudeCli => Capabilities {
                streams_text: true,
                tools: true,
                cancel: true,
                usage: true,
                cost: true,
            },
            Kind::CodexCli => Capabilities {
                // Whole messages, not deltas.
                streams_text: false,
                tools: true,
                cancel: true,
                usage: true,
                cost: false,
            },
        }
    }

    /// Environment variables never passed to the CLI: keys that would make
    /// it bill an API account instead of the user's login.
    fn scrubbed(self) -> &'static [&'static str] {
        &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ]
    }
}

/// What an adapter can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Text arrives as it is written, not only as whole messages.
    pub streams_text: bool,
    pub tools: bool,
    pub cancel: bool,
    pub usage: bool,
    /// Reports what a run cost.
    pub cost: bool,
}

/// Whether a provider can run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ready,
    NotInstalled,
    LoggedOut,
    /// Logged in with an API key: runs would be metered.
    MeteredLogin(String),
    /// A version whose flags or output this adapter does not read.
    Incompatible(String),
    /// The status could not be read.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub kind: Kind,
    pub program: Option<PathBuf>,
    pub version: Option<String>,
    /// How the CLI says it is logged in (for example `claude.ai`).
    pub login: Option<String>,
    pub state: State,
}

/// Dreamer's tools for the run: `program args...` speaks MCP on its
/// standard streams (normally `dream mcp ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub prompt: String,
    /// Added to the CLI's own instructions.
    pub system: Option<String>,
    pub cwd: Option<PathBuf>,
    pub tools: Option<Tools>,
    pub max_turns: Option<u32>,
    pub model: Option<String>,
    /// Stopped (as cancelled by time) after this long.
    pub timeout: Option<Duration>,
    /// Let a run go on when the CLI reports an API key.
    pub allow_metered: bool,
    /// More environment for the CLI and the tools it starts (for example
    /// the daemon's runtime directory). The API key variables are removed
    /// even when given here.
    pub env: Vec<(String, String)>,
}

/// What a run produces, the same for every provider.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started {
        session: Option<String>,
        model: Option<String>,
    },
    /// Text written by the model (a delta when the provider streams).
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        ok: bool,
        text: String,
    },
    /// A usage limit's state, when it is not simply allowed.
    Limit(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    /// The run finished; its final answer.
    Done(String),
    /// The run ended without an answer.
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    NotInstalled,
    LoggedOut(String),
    /// The CLI used an API key and the request did not allow metered use.
    Metered(String),
    RateLimited(String),
    Cancelled,
    TimedOut,
    /// Anything else: the CLI's own words.
    Failed(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NotInstalled => write!(f, "not installed"),
            Failure::LoggedOut(why) => write!(f, "logged out: {why}"),
            Failure::Metered(source) => write!(
                f,
                "the CLI is using an API key ({source}), which would be metered; log the CLI in to your subscription, or allow metered use"
            ),
            Failure::RateLimited(why) => write!(f, "rate limited: {why}"),
            Failure::Cancelled => write!(f, "cancelled"),
            Failure::TimedOut => write!(f, "timed out"),
            Failure::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// The CLI's executable: the override variable, else `PATH`.
pub fn locate(kind: Kind) -> Option<PathBuf> {
    let (name, var) = kind.program();
    if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    locate_in_path(name)
}

/// The first number of a version like `2.1.325 (Claude Code)` or
/// `codex-cli 0.46.0`.
fn major(version: &str) -> Option<u32> {
    version
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .find(|w| w.contains('.'))
        .and_then(|w| w.split('.').next())
        .and_then(|n| n.parse().ok())
}

async fn output(program: &Path, args: &[&str], kind: Kind) -> Option<(bool, String)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for v in kind.scrubbed() {
        cmd.env_remove(v);
    }
    let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .ok()?
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

/// Whether `kind` can run now: installed, a version this adapter reads,
/// and logged in, by the CLI's own account.
pub async fn probe(kind: Kind) -> Probe {
    probe_program(kind, locate(kind)).await
}

/// [`probe`] with the executable given.
pub async fn probe_program(kind: Kind, program: Option<PathBuf>) -> Probe {
    let mut p = Probe {
        kind,
        program,
        version: None,
        login: None,
        state: State::NotInstalled,
    };
    let Some(program) = p.program.clone() else {
        return p;
    };
    let Some((_, version)) = output(&program, &["--version"], kind).await else {
        p.state = State::Unknown("`--version` did not answer".into());
        return p;
    };
    let version = version.trim().to_owned();
    p.version = Some(version.clone());
    let (tested, want) = kind.tested();
    if want != 0 && major(&version) != Some(want) {
        p.state = State::Incompatible(format!(
            "{version}; this adapter reads major version {want} (tested with {tested})"
        ));
        return p;
    }
    p.state = match kind {
        Kind::ClaudeCli => claude::login(&program, &mut p.login).await,
        Kind::CodexCli => codex::login(&program, &mut p.login).await,
    };
    p
}

/// Turns one line of a CLI's output into events. `stop` is set when the
/// run must be ended (a metered key, for example).
trait Parser: Send {
    fn line(&mut self, line: &str, stop: &mut bool) -> Vec<Event>;
}

/// The program's arguments for a request.
fn arguments(kind: Kind, req: &Request) -> Vec<String> {
    match kind {
        Kind::ClaudeCli => claude::arguments(req),
        Kind::CodexCli => codex::arguments(req),
    }
}

fn parser(kind: Kind, req: &Request) -> Box<dyn Parser> {
    match kind {
        Kind::ClaudeCli => Box::new(claude::Stream::new(req.allow_metered)),
        Kind::CodexCli => Box::new(codex::Stream::default()),
    }
}

/// A CLI's error text, classified.
fn classify(text: &str) -> Failure {
    let t = text.to_lowercase();
    if t.contains("/login")
        || t.contains("not logged in")
        || t.contains("log in")
        || t.contains("invalid api key")
        || t.contains("authentication")
    {
        Failure::LoggedOut(text.trim().to_owned())
    } else if t.contains("rate limit") || t.contains("usage limit") || t.contains("429") {
        Failure::RateLimited(text.trim().to_owned())
    } else {
        Failure::Failed(text.trim().to_owned())
    }
}

/// Keep long tool output short in events.
pub(crate) fn clip(s: &str) -> String {
    const MAX: usize = 2000;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Start `cmd`, retrying a few times if the program is busy being written
/// (`ETXTBSY`): on Linux another thread's fork can briefly hold a just
/// written executable open, so its first exec fails. Any other error is
/// returned at once.
pub(crate) fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 5 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_by_their_major_number() {
        assert_eq!(major("2.1.325 (Claude Code)"), Some(2));
        assert_eq!(major("codex-cli 0.46.0"), Some(0));
        assert_eq!(major("unknown"), None);
    }

    #[test]
    fn errors_are_classified_by_what_the_cli_said() {
        assert!(matches!(
            classify("Invalid API key · Please run /login"),
            Failure::LoggedOut(_)
        ));
        assert!(matches!(
            classify("You've hit your usage limit"),
            Failure::RateLimited(_)
        ));
        assert!(matches!(classify("something else"), Failure::Failed(_)));
    }
}
