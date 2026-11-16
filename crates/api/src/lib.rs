//! Dreamer's typed API, generated from `schema/api.yaml` (spec 12.1).
//!
//! The same records travel as Protobuf on the native wire and as JSON over
//! JSON-RPC 2.0; `Service` is implemented once and reached through either
//! endpoint. Everything in [`generated`] comes from the schema; edit the
//! schema and run `cargo xtask api`, never the generated file.

mod generated;
pub mod json;

pub use generated::*;

/// The Luau module scripts receive; call it with the host's `call` function.
pub const LUAU_MODULE: &str = include_str!("../generated/dreamer.luau");

/// Not a method: the grant entry that lets an app (a filter pane) install
/// and manage filters that only read, within its own panes. See
/// `docs/grants.md`.
pub const MANAGE_FILTERS: &str = "filters.manage";

/// The methods an agent's grant holds at `level` (an [`ActLevel`] number):
/// the terminal tools that observe at every level, and those that act from
/// `own` up. See `docs/grants.md`.
pub fn agent_methods(level: i32) -> Vec<String> {
    let mut m = vec![
        method::TMUX_TOPOLOGY,
        method::TERMINAL_LIST,
        method::TERMINAL_READ,
        method::TERMINAL_QUERY,
        method::TERMINAL_WATCH,
        method::JOBS_GET,
        method::JOBS_LIST,
        method::ACTIONS_GET,
        method::ACTIONS_LIST,
        method::AGENTS_RUNS,
        // Collectors only read panes, so observing may start and query
        // them. Stopping one, which may be the user's, is an action.
        method::COLLECT_CREATE,
        method::COLLECT_LIST,
        method::COLLECT_QUERY,
    ];
    if level >= ActLevel::Own as i32 {
        m.extend([
            method::TERMINAL_INPUT,
            method::TERMINAL_RUN,
            method::ACTIONS_SUBMIT,
            method::JOBS_START,
            method::JOBS_CANCEL,
            method::COLLECT_STOP,
        ]);
    }
    m.into_iter().map(str::to_owned).collect()
}

/// Standard JSON-RPC 2.0 codes, plus Dreamer's own in the server range.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
    /// The target (endpoint, pane, window, action) does not exist.
    pub const NOT_FOUND: i32 = -32001;
    /// A precondition such as a pane generation no longer holds.
    pub const PRECONDITION: i32 = -32002;
    /// An idempotency key was reused with different content.
    pub const CONFLICT: i32 = -32003;
    /// tmux or the daemon is not reachable right now.
    pub const UNAVAILABLE: i32 = -32004;
    /// The connection's grant does not allow this method or target.
    pub const FORBIDDEN: i32 = -32005;
    /// A limit was reached: a rate, or a message chain's causal depth.
    pub const LIMITED: i32 = -32006;
}

/// A request or response that could not be decoded for a method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    UnknownMethod(String),
    Decode(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::UnknownMethod(m) => write!(f, "unknown method `{m}`"),
            CallError::Decode(e) => write!(f, "invalid payload: {e}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<prost::DecodeError> for CallError {
    fn from(e: prost::DecodeError) -> Self {
        CallError::Decode(e.to_string())
    }
}

impl From<serde_json::Error> for CallError {
    fn from(e: serde_json::Error) -> Self {
        CallError::Decode(e.to_string())
    }
}

impl From<CallError> for RpcError {
    fn from(e: CallError) -> Self {
        let code = match e {
            CallError::UnknownMethod(_) => code::METHOD_NOT_FOUND,
            CallError::Decode(_) => code::INVALID_PARAMS,
        };
        RpcError {
            code,
            message: e.to_string(),
            data: None,
        }
    }
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}
