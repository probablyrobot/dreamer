//! `dream mcp`: an MCP server on standard input and output (spec 12.6),
//! so an MCP client (an agent) can use Dreamer's tools.
//!
//! Every tool is one typed API call over an ordinary connection to the
//! daemon, so the daemon's checks (grants, targets, bounds) apply exactly
//! as for any client. With `--method`, `--pane`, or `--all-panes` the
//! connection first narrows its own grant (`grants.narrow`) to just that,
//! before any tool runs; it can never widen afterwards. A refused or
//! failed call is a tool error the agent can read, never a crash.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServiceExt, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use crate::daemon::DaemonArgs;
use crate::remote::session;

/// Lines a read returns at most.
const MAX_LINES: u32 = 500;

#[derive(Clone)]
struct Dreamer {
    client: Arc<client::Client>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadPane {
    /// The pane id (the number in %N).
    pane: u32,
    /// How many history lines above the screen (default 50, at most 500).
    lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreatePane {
    /// The pane to split.
    pane: u32,
    /// "horizontal" (side by side) or "vertical" (one above the other).
    direction: Option<String>,
    /// The exact program and arguments to run; empty runs the default shell.
    #[serde(default)]
    argv: Vec<String>,
    /// The working directory.
    cwd: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StartJob {
    /// The exact program and arguments; no shell.
    argv: Vec<String>,
    cwd: Option<String>,
    /// The tmux window name.
    name: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SendInput {
    pane: u32,
    /// Text typed into the pane exactly as given; include "\n" to press Enter.
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ById {
    /// The action or job id.
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListRuns {
    /// Connected hosts to ask too; "*" means every live host.
    #[serde(default)]
    hosts: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Nothing {}

fn text(v: Value) -> CallToolResult {
    let mut r = CallToolResult::success(vec![ContentBlock::text(v.to_string())]);
    r.structured_content = Some(v);
    r
}

impl Dreamer {
    /// One typed call; its result as JSON, or a tool error saying why.
    async fn call(&self, request: api::Request) -> Result<CallToolResult, ErrorData> {
        Ok(match self.client.call(request).await {
            Ok(r) => text(r.to_json()),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        })
    }
}

#[tool_router(server_handler)]
impl Dreamer {
    #[tool(description = "List the tmux sessions, windows, and panes on this host.")]
    async fn list_panes(&self, _: Parameters<Nothing>) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TmuxTopology(api::TopologyParams::default()))
            .await
    }

    #[tool(
        description = "List agent runs (Claude Code, Codex) here and on connected hosts, with the evidence for each. Their state says only what is known."
    )]
    async fn list_runs(
        &self,
        Parameters(p): Parameters<ListRuns>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::AgentsRuns(api::AgentRunQuery {
            hosts: p.hosts,
            provider: None,
        }))
        .await
    }

    #[tool(description = "Read a pane's screen and up to 500 lines of history as text.")]
    async fn read_pane(
        &self,
        Parameters(p): Parameters<ReadPane>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalRead(api::ReadParams {
            pane: p.pane,
            history: Some(p.lines.unwrap_or(50).min(MAX_LINES)),
            endpoint: None,
        }))
        .await
    }

    #[tool(
        description = "Split a pane, optionally running an exact program in the new pane. Returns the action record with the new pane."
    )]
    async fn create_pane(
        &self,
        Parameters(p): Parameters<CreatePane>,
    ) -> Result<CallToolResult, ErrorData> {
        let direction = match p.direction.as_deref() {
            None | Some("vertical") => api::SplitDirection::Vertical,
            Some("horizontal") => api::SplitDirection::Horizontal,
            Some(other) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "direction `{other}`: use horizontal or vertical"
                ))]));
            }
        };
        self.call(api::Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action {
                kind: Some(api::action::Kind::SplitPane(api::SplitPane {
                    pane: p.pane,
                    direction: direction as i32,
                    argv: p.argv,
                    cwd: p.cwd,
                    ..api::SplitPane::default()
                })),
            }),
            ..api::SubmitAction::default()
        }))
        .await
    }

    #[tool(
        description = "Start an exact program as a job in its own tmux window; it keeps running if the daemon stops. Returns the job record."
    )]
    async fn start_job(
        &self,
        Parameters(p): Parameters<StartJob>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::JobsStart(api::JobStart {
            argv: p.argv,
            cwd: p.cwd,
            name: p.name,
            ..api::JobStart::default()
        }))
        .await
    }

    #[tool(
        description = "Type text into a pane, exactly as given (\"\\n\" presses Enter). This is explicit input to whatever program is in the pane."
    )]
    async fn send_input(
        &self,
        Parameters(p): Parameters<SendInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalInput(api::TerminalInput {
            pane: p.pane,
            event: Some(api::terminal_input::Event::Text(api::TextInput {
                text: p.text,
            })),
            ..api::TerminalInput::default()
        }))
        .await
    }

    #[tool(description = "The record of an action: its state and what was observed.")]
    async fn check_action(
        &self,
        Parameters(p): Parameters<ById>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::ActionsGet(api::ActionQuery { id: p.id }))
            .await
    }

    #[tool(description = "Cancel a managed job (SIGTERM to its program). Returns its record.")]
    async fn cancel_job(
        &self,
        Parameters(p): Parameters<ById>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::JobsCancel(api::JobRef { id: p.id }))
            .await
    }
}

/// `dream mcp [--method M]... [--pane %N]... [--all-panes] [tmux options]`.
pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let mut grant = api::Grant::default();
    let mut narrow = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--method" => grant
                .methods
                .push(it.next().ok_or("--method needs a name")?.clone()),
            "--pane" => grant.panes.push(
                it.next()
                    .ok_or("--pane needs %N")?
                    .trim_start_matches('%')
                    .parse()
                    .map_err(|_| "--pane needs %N".to_owned())?,
            ),
            "--all-panes" => grant.all_panes = true,
            other => return Err(format!("unknown option {other}")),
        }
        narrow = true;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let c = session(d).await?;
        if narrow {
            c.call(api::Request::GrantsNarrow(grant))
                .await
                .map_err(|e| format!("grants.narrow: {e}"))?;
        }
        let server = Dreamer {
            client: Arc::new(c),
        }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("mcp: {e}"))?;
        server.waiting().await.map_err(|e| format!("mcp: {e}"))?;
        Ok::<(), String>(())
    })?;
    Ok(())
}
