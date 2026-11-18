//! Agent sessions: a long-lived harness process, driven over its own
//! protocol, whose output becomes one normalised stream of [`Update`]s
//! whatever the harness. The shapes follow the Agent Client Protocol:
//! text and thinking as deltas, a tool call as started, updated, and
//! finished records with one id, approval requests, a plan, usage, and the
//! end of each turn.
//!
//! The rules of the one-shot providers hold here too: the harness runs on
//! its own login, never sees an API key variable, and Dreamer never reads
//! its credentials.

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

/// What a harness reports, normalised.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Text(String),
    Thinking(String),
    Tool(ToolCall),
    Approval(ApprovalRequest),
    Plan(Vec<PlanEntry>),
    Usage(Usage),
    /// The turn a prompt started is over; nothing more follows for it.
    TurnEnd(StopReason),
    Error(String),
    /// The harness's own session, once known (kept for resume).
    HarnessSession {
        id: String,
        file: Option<String>,
    },
    /// The harness process ended; nothing follows.
    Exited(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    pub input: Option<serde_json::Value>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Started,
    Updated,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    pub title: String,
    pub message: Option<String>,
    /// Choices offered; empty for a yes or no question.
    pub options: Vec<String>,
    /// The harness answers by itself after this long.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
    pub priority: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    Cancelled,
    Error,
}

/// What a client asks of a running harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Prompt(String),
    /// Stop the running turn.
    Cancel,
    /// Answer an approval request.
    Answer {
        id: String,
        allow: bool,
        option: Option<String>,
    },
}

/// How to start a harness.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub program: PathBuf,
    /// Arguments before the ones the adapter adds.
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// More environment (the daemon's socket, the session's ticket). The
    /// API key variables are removed even when given here.
    pub env: Vec<(String, String)>,
    /// pi's agent directory (`PI_CODING_AGENT_DIR`).
    pub agent_dir: Option<PathBuf>,
    pub model: Option<String>,
    /// The provider the harness is pinned to. Before each prompt the
    /// adapter makes sure the harness's model is one of this provider's,
    /// and refuses the prompt when it cannot.
    pub provider: Option<String>,
    /// Run with no automatic network use (pi: `PI_OFFLINE=1`): set when
    /// there is no login.
    pub offline: bool,
    /// The harness's own session to continue: a file path or an id.
    pub resume: Option<String>,
    /// Dreamer's tools as an MCP server (`dream mcp`), for the harnesses
    /// that take one (Claude Code, Codex, ACP agents). It runs with `env`.
    pub tools: Option<crate::Tools>,
}

/// How Dreamer talks to a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `pi --mode rpc`.
    Pi,
    /// `claude -p` with stream-json input and output.
    Claude,
    /// `codex app-server`.
    Codex,
    /// The Agent Client Protocol on standard streams.
    Acp,
}

impl Protocol {
    pub fn parse(s: &str) -> Option<Protocol> {
        Some(match s {
            "pi" => Protocol::Pi,
            "claude" => Protocol::Claude,
            "codex" => Protocol::Codex,
            "acp" => Protocol::Acp,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Protocol::Pi => "pi",
            Protocol::Claude => "claude",
            Protocol::Codex => "codex",
            Protocol::Acp => "acp",
        }
    }
}

/// Start a harness over `protocol`. Must be called inside a Tokio runtime.
pub async fn start(protocol: Protocol, spec: &Spec) -> Result<Harness, String> {
    match protocol {
        Protocol::Pi => crate::pi_rpc::start(spec).await,
        Protocol::Claude => crate::claude_session::start(spec).await,
        Protocol::Codex => crate::codex_session::start(spec).await,
        Protocol::Acp => crate::acp::start(spec).await,
    }
}

/// A running harness: what it says arrives on `updates`, and `control`
/// talks to it. The process ends when `control` closes it or is dropped.
#[derive(Debug)]
pub struct Harness {
    pub pid: u32,
    pub version: Option<String>,
    /// The harness's own session id and file, from the start check.
    pub session: Option<(String, Option<String>)>,
    pub control: Control,
    pub updates: mpsc::UnboundedReceiver<Update>,
}

impl Harness {
    pub fn send(&self, input: Input) -> Result<(), String> {
        self.control.send(input)
    }

    pub fn close(&mut self) {
        self.control.close();
    }
}

/// Sends input to a running harness, and ends it.
#[derive(Debug)]
pub struct Control {
    pub(crate) input: mpsc::UnboundedSender<Input>,
    pub(crate) close: Option<oneshot::Sender<()>>,
}

impl Control {
    pub fn send(&self, input: Input) -> Result<(), String> {
        self.input
            .send(input)
            .map_err(|_| "the harness has ended".to_owned())
    }

    /// Ask the harness to end: its input closes, then its process group
    /// gets SIGTERM and finally SIGKILL. `Update::Exited` follows. Dropping
    /// the control does the same.
    pub fn close(&mut self) {
        if let Some(c) = self.close.take() {
            let _ = c.send(());
        }
    }
}

/// Environment variables never passed to a harness: keys that would make
/// it bill an API account instead of the user's own login.
pub const SCRUBBED: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
];
