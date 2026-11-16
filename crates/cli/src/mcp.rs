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
/// The idle time of a lease this server takes.
const LEASE_MS: u32 = 120_000;

#[derive(Clone)]
struct Dreamer {
    client: Arc<client::Client>,
    /// Interaction leases this session holds, by pane.
    leases: Arc<tokio::sync::Mutex<Leases>>,
}

#[derive(Default)]
struct Leases {
    held: std::collections::HashMap<u32, String>,
    /// Panes a person typed into while this session held their lease: it
    /// does not type there again.
    taken_over: std::collections::HashSet<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadPane {
    /// The pane id (the number in %N).
    pane: u32,
    /// How many history lines above the screen (default 50, at most 500).
    lines: Option<u32>,
    /// A connected host's name (see list_hosts); without one, this host.
    host: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct OnHost {
    /// A connected host's name (see list_hosts); without one, this host.
    host: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct JobOnHost {
    /// The job id start_job returned.
    id: String,
    /// The host the job was started on; without one, this host.
    host: Option<String>,
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
    /// A connected host's name (see list_hosts) to run it on; without one,
    /// this host.
    host: Option<String>,
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
struct MovePanes {
    /// The panes to move, in order.
    panes: Vec<u32>,
    /// The window to gather them in: an existing window with this name in
    /// the first pane's session, else a new one.
    window: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FindLines {
    /// Plain text to find (case-insensitive), unless `regex` is set.
    text: String,
    /// Treat `text` as a regular expression.
    #[serde(default)]
    regex: bool,
    /// The panes to search; empty searches every pane the grant allows.
    #[serde(default)]
    panes: Vec<u32>,
    /// How many of each pane's newest lines to search (default 200).
    lines: Option<u32>,
    /// Connected hosts to search too (all their panes); "*" means every
    /// live host. Each match names its host.
    #[serde(default)]
    hosts: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WatchLines {
    /// The filter's name: letters, digits, `-`, `_`, or `.`; unique.
    name: String,
    /// Plain text to match (case-insensitive), unless `regex` is set.
    text: String,
    /// Treat `text` as a regular expression.
    #[serde(default)]
    regex: bool,
    /// The panes to watch; empty watches every pane the grant allows.
    #[serde(default)]
    panes: Vec<u32>,
    /// Mark matched lines in the panes: "highlight", "dim", "fold", or
    /// "badge" (with `badge`). Without one, lines are only reported.
    mark: Option<String>,
    /// The badge's text, for mark "badge".
    badge: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ByName {
    /// The filter's name.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Nothing {}

#[derive(Deserialize, schemars::JsonSchema)]
struct TerminalRead {
    /// The pane id (the number in %N).
    pane: u32,
    /// Return the last this many lines (history and screen, at most 2000)
    /// instead of the screen.
    lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TerminalSearch {
    /// A regular expression, or plain text with `literal`.
    pattern: String,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    ignore_case: bool,
    /// The panes to search; empty searches every pane.
    #[serde(default)]
    panes: Vec<u32>,
    /// How many of each pane's newest lines to search (default 200, at
    /// most 2000).
    lines: Option<u32>,
    /// Matches returned at most (default 100, at most 1000).
    max_matches: Option<u32>,
    /// Bytes of matching lines returned at most (default 65536).
    max_bytes: Option<u32>,
    /// Time budget in ms (default 2000, at most 10000).
    timeout_ms: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TerminalWatch {
    pane: u32,
    /// Wait for a line matching this regular expression (or plain text with
    /// `literal`) in output written from now on.
    pattern: Option<String>,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    ignore_case: bool,
    /// Or until the pane writes nothing for this many ms.
    quiet_ms: Option<u32>,
    /// Or until the pane's program ends.
    #[serde(default)]
    exit: bool,
    /// Give up after this many ms (default 30000, at most 600000).
    timeout_ms: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TerminalRun {
    /// The exact program and arguments; no shell. Give this or `command`.
    #[serde(default)]
    argv: Vec<String>,
    /// A shell command line, run with /bin/sh -c. Give this or `argv`.
    command: Option<String>,
    cwd: Option<String>,
    /// Split this pane for it instead of opening a new window.
    split: Option<u32>,
    /// For split: "vertical" (default) or "horizontal".
    direction: Option<String>,
    /// The new window's name.
    name: Option<String>,
    /// How long to wait in ms (default 60000, at most 600000); it keeps
    /// running after that.
    timeout_ms: Option<u32>,
    /// Output lines returned at most, the newest (default 200).
    lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TerminalSend {
    pane: u32,
    /// Text typed exactly as given.
    text: Option<String>,
    /// Text pasted (bracketed if the program asked for that).
    paste: Option<String>,
    /// Press Enter afterwards.
    #[serde(default)]
    enter: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LayoutSplit {
    /// The pane to split.
    pane: u32,
    /// "vertical" (one above the other, default) or "horizontal".
    direction: Option<String>,
    /// The exact program and arguments; empty runs the default shell.
    #[serde(default)]
    argv: Vec<String>,
    cwd: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PaneOrWindow {
    /// A pane id; give this or `window`.
    pane: Option<u32>,
    /// A window id (the number in @N); give this or `pane`.
    window: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LayoutRename {
    /// A window id; give this or `session`.
    window: Option<u32>,
    /// A session id (the number in $N); give this or `window`.
    session: Option<u32>,
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LayoutMove {
    pane: u32,
    /// The window to move it into; give this or `new_window`.
    window: Option<u32>,
    /// The name of a new window of its own; give this or `window`.
    new_window: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct OnePane {
    pane: u32,
}

fn direction(d: Option<&str>) -> Result<api::SplitDirection, String> {
    match d {
        None | Some("vertical") => Ok(api::SplitDirection::Vertical),
        Some("horizontal") => Ok(api::SplitDirection::Horizontal),
        Some(other) => Err(format!("direction `{other}`: use horizontal or vertical")),
    }
}

fn text(v: Value) -> CallToolResult {
    let mut r = CallToolResult::success(vec![ContentBlock::text(v.to_string())]);
    r.structured_content = Some(v);
    r
}

fn failed(why: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(why)])
}

impl Dreamer {
    async fn topology(&self) -> Result<api::Topology, String> {
        match self
            .client
            .call(api::Request::TmuxTopology(api::TopologyParams::default()))
            .await
        {
            Ok(api::Response::TmuxTopology(t)) => Ok(t),
            Ok(_) => Err("tmux.topology: unexpected answer".into()),
            Err(e) => Err(format!("tmux.topology: {e}")),
        }
    }

    async fn submit(&self, kind: api::action::Kind) -> Result<api::ActionRecord, String> {
        match self
            .client
            .call(api::Request::ActionsSubmit(api::SubmitAction {
                action: Some(api::Action { kind: Some(kind) }),
                ..api::SubmitAction::default()
            }))
            .await
        {
            Ok(api::Response::ActionsSubmit(r)) => Ok(r),
            Ok(_) => Err("actions.submit: unexpected answer".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// One typed call, here or (with `host`) forwarded to that connected
    /// host's daemon, under the same grant checks: a forwarded call is a
    /// `hosts.call`, which this connection's grant must allow.
    async fn call_on(
        &self,
        host: Option<String>,
        request: api::Request,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(name) = host.filter(|h| !h.is_empty()) else {
            return self.call(request).await;
        };
        let method = request.method();
        let reply = self
            .client
            .call(api::Request::HostsCall(api::HostCall {
                name: name.clone(),
                method: method.to_owned(),
                payload: request.encode(),
            }))
            .await;
        Ok(match reply {
            Ok(api::Response::HostsCall(r)) => match api::Response::decode(method, &r.payload) {
                Ok(v) => {
                    let mut j = v.to_json();
                    if let Some(o) = j.as_object_mut() {
                        o.insert("host".into(), serde_json::Value::String(name));
                    }
                    text(j)
                }
                Err(e) => failed(format!("{name}: {method}: undecodable reply: {e}")),
            },
            Ok(_) => failed("hosts.call: unexpected answer".into()),
            Err(e) => failed(format!("{name}: {method}: {e}")),
        })
    }

    /// One layout action; a tool error unless it completed.
    async fn act(&self, kind: api::action::Kind) -> Result<CallToolResult, ErrorData> {
        Ok(match self.submit(kind).await {
            Ok(r) => {
                let done = r.state == api::ActionState::Completed as i32;
                let mut out = text(serde_json::to_value(&r).unwrap_or_default());
                if !done {
                    out.is_error = Some(true);
                }
                out
            }
            Err(e) => failed(e),
        })
    }

    /// Send input events to a pane as automation: under this session's
    /// lease on the pane, taken on first use. A person typing there revokes
    /// it, and this session then leaves the pane alone.
    async fn type_into(
        &self,
        pane: u32,
        events: Vec<api::terminal_input::Event>,
    ) -> CallToolResult {
        let mut leases = self.leases.lock().await;
        if leases.taken_over.contains(&pane) {
            return failed(format!(
                "a person typed into pane %{pane} while this agent held it; ask the user before typing there again"
            ));
        }
        let mut sent = 0u64;
        let mut events = events.into_iter().peekable();
        let mut renewed = false;
        while let Some(event) = events.peek().cloned() {
            let lease = match leases.held.get(&pane) {
                Some(id) => id.clone(),
                None => match self
                    .client
                    .call(api::Request::LeasesAcquire(api::LeaseAcquire {
                        pane,
                        // Long enough to outlast waiting for an approval.
                        ttl_ms: Some(LEASE_MS),
                    }))
                    .await
                {
                    Ok(api::Response::LeasesAcquire(l)) => {
                        leases.held.insert(pane, l.id.clone());
                        l.id
                    }
                    Ok(_) => return failed("leases.acquire: unexpected answer".into()),
                    Err(e) => return failed(format!("leases.acquire: {e}")),
                },
            };
            let r = self
                .client
                .call(api::Request::TerminalInput(api::TerminalInput {
                    pane,
                    lease: Some(lease),
                    event: Some(event),
                    ..api::TerminalInput::default()
                }))
                .await;
            match r {
                Ok(api::Response::TerminalInput(r)) => {
                    sent += r.bytes;
                    events.next();
                }
                Ok(_) => return failed("terminal.input: unexpected answer".into()),
                Err(e) => {
                    let why = e.to_string();
                    leases.held.remove(&pane);
                    if why.contains("a person typed") {
                        leases.taken_over.insert(pane);
                        return failed(format!(
                            "{why}. A person took over pane %{pane}; ask the user before typing there again"
                        ));
                    }
                    // An expired lease is renewed once; anything else is
                    // the agent's to read.
                    if !renewed && why.contains("expired") {
                        renewed = true;
                        continue;
                    }
                    return failed(why);
                }
            }
        }
        text(serde_json::json!({ "pane": pane, "bytes": sent }))
    }

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
    #[tool(
        description = "List the tmux sessions, windows, and panes on this host, or on a connected host."
    )]
    async fn list_panes(
        &self,
        Parameters(p): Parameters<OnHost>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_on(
            p.host,
            api::Request::TmuxTopology(api::TopologyParams::default()),
        )
        .await
    }

    #[tool(
        description = "List the remote hosts Dreamer knows: each one's name, whether it is connected (live), and why not. Tools that take `host` act on a live one."
    )]
    async fn list_hosts(&self, _: Parameters<Nothing>) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::HostsList(api::Empty::default()))
            .await
    }

    #[tool(
        description = "A job's record: its state, exit status, and window. Pass the host it was started on."
    )]
    async fn check_job(
        &self,
        Parameters(p): Parameters<JobOnHost>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_on(p.host, api::Request::JobsGet(api::JobRef { id: p.id }))
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
        self.call_on(
            p.host,
            api::Request::TerminalRead(api::ReadParams {
                pane: p.pane,
                history: Some(p.lines.unwrap_or(50).min(MAX_LINES)),
                endpoint: None,
            }),
        )
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
        description = "Start an exact program as a job in its own tmux window, on this host or a connected one; it keeps running if the daemon stops. Returns the job record; follow it with check_job on the same host."
    )]
    async fn start_job(
        &self,
        Parameters(p): Parameters<StartJob>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_on(
            p.host,
            api::Request::JobsStart(api::JobStart {
                argv: p.argv,
                cwd: p.cwd,
                name: p.name,
                ..api::JobStart::default()
            }),
        )
        .await
    }

    #[tool(
        description = "Type text into a pane, exactly as given (\"\\n\" presses Enter). This is explicit input to whatever program is in the pane."
    )]
    async fn send_input(
        &self,
        Parameters(p): Parameters<SendInput>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .type_into(
                p.pane,
                vec![api::terminal_input::Event::Text(api::TextInput {
                    text: p.text,
                })],
            )
            .await)
    }

    #[tool(
        description = "Move panes, with their programs, into one window with the given name: an existing window of that name in the first pane's session, else a new one. Returns each pane's action record; it stops at the first move that does not complete."
    )]
    async fn move_panes(
        &self,
        Parameters(p): Parameters<MovePanes>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::action::Kind;
        if p.panes.is_empty() || p.window.trim().is_empty() {
            return Ok(failed("move_panes needs panes and a window name".into()));
        }
        let top = match self.topology().await {
            Ok(t) => t,
            Err(e) => return Ok(failed(e)),
        };
        let Some(first) = top.panes.iter().find(|x| x.id == p.panes[0]) else {
            return Ok(failed(format!("pane %{} does not exist", p.panes[0])));
        };
        let session = top
            .windows
            .iter()
            .find(|w| w.id == first.window)
            .and_then(|w| w.sessions.first().copied());
        let mut target = top
            .windows
            .iter()
            .find(|w| w.name == p.window && session.is_some_and(|s| w.sessions.contains(&s)))
            .map(|w| w.id);
        let mut records = Vec::new();
        for &pane in &p.panes {
            let here = top.panes.iter().find(|x| x.id == pane).map(|x| x.window);
            if target.is_some() && here == target {
                continue;
            }
            let kind = Kind::MovePane(api::MovePane {
                pane,
                window: target,
                new_window: target.is_none().then(|| p.window.clone()),
            });
            let r = match self.submit(kind).await {
                Ok(r) => r,
                Err(e) => {
                    records.push(serde_json::json!({ "pane": pane, "error": e }));
                    break;
                }
            };
            let ok = r.state == api::ActionState::Completed as i32;
            records.push(serde_json::to_value(&r).unwrap_or_default());
            if !ok {
                break;
            }
            if target.is_none() {
                target = match self.topology().await {
                    Ok(t) => t.panes.iter().find(|x| x.id == pane).map(|x| x.window),
                    Err(e) => return Ok(failed(e)),
                };
            }
        }
        // Stopping early always leaves a failed or unfinished record last.
        let failed_move = records
            .iter()
            .any(|r| r.get("error").is_some() || r["state"] != "completed");
        let mut r = text(serde_json::json!({ "window": target, "moves": records }));
        if failed_move {
            r.is_error = Some(true);
        }
        Ok(r)
    }

    #[tool(
        description = "Find lines in panes' recent output: which panes show this text, and the matching lines. Bounded; a cut-short answer says so."
    )]
    async fn find_lines(
        &self,
        Parameters(p): Parameters<FindLines>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalQuery(api::TerminalQuery {
            pattern: p.text,
            literal: !p.regex,
            ignore_case: true,
            panes: p.panes,
            lines: p.lines,
            hosts: p.hosts,
            max_matches: Some(100),
            ..api::TerminalQuery::default()
        }))
        .await
    }

    #[tool(
        description = "Install a filter: watch panes for lines that match, from now on, and report each one (and mark it in the pane, if asked). It only reads; it never changes what a program wrote. Returns its status; list_filters shows its counts."
    )]
    async fn watch_lines(
        &self,
        Parameters(p): Parameters<WatchLines>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::DecorationStyle as D;
        let decorate = match p.mark.as_deref() {
            None => D::Unspecified,
            Some("highlight") => D::Highlight,
            Some("dim") => D::Dim,
            Some("fold") => D::Fold,
            Some("badge") => D::Badge,
            Some(other) => {
                return Ok(failed(format!(
                    "mark `{other}` is not highlight, dim, fold, or badge"
                )));
            }
        };
        let every = p.panes.is_empty();
        self.call(api::Request::FiltersInstall(api::FilterSpec {
            name: p.name,
            panes: p.panes.clone(),
            every_pane: every,
            pattern: p.text,
            literal: !p.regex,
            ignore_case: true,
            event: true,
            decorate: decorate as i32,
            badge: p.badge,
            grant: Some(api::ScriptGrant {
                methods: vec![api::method::TAPS_SUBSCRIBE.into()],
                panes: p.panes,
                all_panes: every,
            }),
            ..api::FilterSpec::default()
        }))
        .await
    }

    #[tool(
        description = "The filters you may manage, with their state and counts (matched, dropped)."
    )]
    async fn list_filters(
        &self,
        Parameters(_): Parameters<Nothing>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::FiltersList(api::Empty {})).await
    }

    #[tool(description = "Remove a filter you installed.")]
    async fn stop_filter(
        &self,
        Parameters(p): Parameters<ByName>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::FiltersRemove(api::FilterRef { name: p.name }))
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

    #[tool(
        description = "List every terminal pane: its session, window, command, directory, title, size, tier, how long it has been quiet, and whether you created it (you may act there without asking)."
    )]
    async fn terminals_list(&self, _: Parameters<Nothing>) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalList(api::TopologyParams::default()))
            .await
    }

    #[tool(
        description = "Read a pane as text: its screen, or with `lines` the last that many lines of history and screen."
    )]
    async fn terminal_read(
        &self,
        Parameters(p): Parameters<TerminalRead>,
    ) -> Result<CallToolResult, ErrorData> {
        let history = p.lines.map(|n| n.min(2000));
        let r = self
            .client
            .call(api::Request::TerminalRead(api::ReadParams {
                pane: p.pane,
                history,
                endpoint: None,
            }))
            .await;
        Ok(match r {
            Ok(api::Response::TerminalRead(t)) => {
                let mut lines = t.lines;
                if let Some(n) = history {
                    while lines.last().is_some_and(|l| l.trim().is_empty()) {
                        lines.pop();
                    }
                    let keep = (n as usize).max(1);
                    if lines.len() > keep {
                        lines.drain(..lines.len() - keep);
                    }
                } else {
                    lines.drain(..(t.history_lines as usize).min(lines.len()));
                }
                text(serde_json::json!({
                    "pane": t.pane,
                    "cols": t.cols,
                    "rows": t.rows,
                    "cursor": [t.cursor_x, t.cursor_y],
                    "lines": lines,
                }))
            }
            Ok(_) => failed("terminal.read: unexpected answer".into()),
            Err(e) => failed(e.to_string()),
        })
    }

    #[tool(
        description = "Search panes' recent output with a regular expression: one pane or every pane. Bounded in lines, matches, bytes, and time; a cut-short answer says so."
    )]
    async fn terminal_search(
        &self,
        Parameters(p): Parameters<TerminalSearch>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalQuery(api::TerminalQuery {
            pattern: p.pattern,
            literal: p.literal,
            ignore_case: p.ignore_case,
            panes: p.panes,
            lines: p.lines,
            max_matches: p.max_matches,
            max_bytes: p.max_bytes,
            timeout_ms: p.timeout_ms,
            ..api::TerminalQuery::default()
        }))
        .await
    }

    #[tool(
        description = "Wait until a pattern appears in a pane's new output, the pane goes quiet for quiet_ms, or (exit) its program ends; whichever comes first, or the timeout. Returns what happened and the matching line."
    )]
    async fn terminal_watch(
        &self,
        Parameters(p): Parameters<TerminalWatch>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::TerminalWatch(api::TerminalWatch {
            pane: p.pane,
            pattern: p.pattern,
            literal: p.literal,
            ignore_case: p.ignore_case,
            quiet_ms: p.quiet_ms,
            exit: p.exit,
            timeout_ms: p.timeout_ms,
        }))
        .await
    }

    #[tool(
        description = "Run a command in a new window (or a pane split from `split`) that you then own, wait for it, and return its exit status and output. If it outlasts the timeout it keeps running: follow it with terminal_watch (exit) and terminal_read."
    )]
    async fn terminal_run(
        &self,
        Parameters(p): Parameters<TerminalRun>,
    ) -> Result<CallToolResult, ErrorData> {
        let argv = match (p.argv.is_empty(), p.command) {
            (false, None) => p.argv,
            (true, Some(c)) => vec!["/bin/sh".into(), "-c".into(), c],
            _ => return Ok(failed("give either argv or command".into())),
        };
        let direction = match direction(p.direction.as_deref()) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        self.call(api::Request::TerminalRun(api::TerminalRun {
            argv,
            cwd: p.cwd,
            name: p.name,
            split: p.split,
            direction: direction as i32,
            timeout_ms: p.timeout_ms,
            lines: p.lines,
            ..api::TerminalRun::default()
        }))
        .await
    }

    #[tool(
        description = "Type or paste into a pane, under an interaction lease: a person typing there takes it back, and you then stop. In a pane you did not create this may wait for the user's approval."
    )]
    async fn terminal_send(
        &self,
        Parameters(p): Parameters<TerminalSend>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::terminal_input::Event;
        let mut events = Vec::new();
        if let Some(t) = p.text {
            events.push(Event::Text(api::TextInput { text: t }));
        }
        if let Some(t) = p.paste {
            events.push(Event::Paste(api::PasteInput {
                data: t.into_bytes(),
                more: false,
                id: 1,
            }));
        }
        if p.enter {
            events.push(Event::Key(api::KeyInput {
                key: "Enter".into(),
                ..api::KeyInput::default()
            }));
        }
        if events.is_empty() {
            return Ok(failed("give text, paste, or enter".into()));
        }
        Ok(self.type_into(p.pane, events).await)
    }

    #[tool(
        description = "Split a pane, optionally running an exact program in the new pane, which you then own. Returns the action record with the new pane."
    )]
    async fn layout_split(
        &self,
        Parameters(p): Parameters<LayoutSplit>,
    ) -> Result<CallToolResult, ErrorData> {
        let direction = match direction(p.direction.as_deref()) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        self.act(api::action::Kind::SplitPane(api::SplitPane {
            pane: p.pane,
            direction: direction as i32,
            argv: p.argv,
            cwd: p.cwd,
            ..api::SplitPane::default()
        }))
        .await
    }

    #[tool(description = "Select (focus) a pane, or a window.")]
    async fn layout_select(
        &self,
        Parameters(p): Parameters<PaneOrWindow>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::action::Kind;
        let kind = match (p.pane, p.window) {
            (Some(pane), None) => Kind::SelectPane(api::SelectPane { pane }),
            (None, Some(window)) => Kind::SelectWindow(api::SelectWindow { window }),
            _ => return Ok(failed("give a pane or a window".into())),
        };
        self.act(kind).await
    }

    #[tool(description = "Rename a window or a session.")]
    async fn layout_rename(
        &self,
        Parameters(p): Parameters<LayoutRename>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::action::Kind;
        let kind = match (p.window, p.session) {
            (Some(window), None) => Kind::RenameWindow(api::RenameWindow {
                window,
                name: p.name,
            }),
            (None, Some(session)) => Kind::RenameSession(api::RenameSession {
                session,
                name: p.name,
            }),
            _ => return Ok(failed("give a window or a session".into())),
        };
        self.act(kind).await
    }

    #[tool(
        description = "Move a pane, with its program, into another window or into a new window of its own."
    )]
    async fn layout_move(
        &self,
        Parameters(p): Parameters<LayoutMove>,
    ) -> Result<CallToolResult, ErrorData> {
        self.act(api::action::Kind::MovePane(api::MovePane {
            pane: p.pane,
            window: p.window,
            new_window: p.new_window,
        }))
        .await
    }

    #[tool(description = "Toggle zoom of a pane: it fills its window, or stops filling it.")]
    async fn layout_zoom(
        &self,
        Parameters(p): Parameters<OnePane>,
    ) -> Result<CallToolResult, ErrorData> {
        self.act(api::action::Kind::ZoomPane(api::ZoomPane { pane: p.pane }))
            .await
    }

    #[tool(
        description = "Close a pane or a window, ending its programs. Outside what you created, this always asks the user."
    )]
    async fn layout_close(
        &self,
        Parameters(p): Parameters<PaneOrWindow>,
    ) -> Result<CallToolResult, ErrorData> {
        use api::action::Kind;
        let kind = match (p.pane, p.window) {
            (Some(pane), None) => Kind::KillPane(api::KillPane { pane }),
            (None, Some(window)) => Kind::KillWindow(api::KillWindow { window }),
            _ => return Ok(failed("give a pane or a window".into())),
        };
        self.act(kind).await
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

pub use api::agent_methods;

/// Inside an agent session's harness: the registration that gives this
/// connection the session's grant (`DREAMER_SESSION_ID` and the ticket
/// file `DREAMER_SESSION_TICKET`, set by the daemon).
fn session_registration() -> Option<api::Request> {
    let id = std::env::var("DREAMER_SESSION_ID").ok()?;
    let path = std::env::var("DREAMER_SESSION_TICKET").ok()?;
    let ticket = std::fs::read_to_string(path).ok()?;
    Some(api::Request::AppsRegister(api::RegisterApp {
        launch_id: format!("session:{id}"),
        ticket: ticket.trim().to_owned(),
    }))
}

/// `dream mcp [--level L] [--method M]... [--pane %N]... [--all-panes]
/// [tmux options]`.
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
            "--level" => {
                grant.level = match it.next().map(String::as_str) {
                    Some("observe") => api::ActLevel::Observe,
                    Some("own") => api::ActLevel::Own,
                    Some("elsewhere") => api::ActLevel::Elsewhere,
                    _ => return Err("--level needs observe, own, or elsewhere".into()),
                } as i32;
            }
            other => return Err(format!("unknown option {other}")),
        }
        narrow = true;
    }
    if grant.level != api::ActLevel::Unspecified as i32 {
        // An agent's grant: every pane, at its level.
        if grant.panes.is_empty() {
            grant.all_panes = true;
        }
        if grant.methods.is_empty() {
            grant.methods = agent_methods(grant.level);
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let c = session(d).await?;
        // Started by an agent session's harness: take the session's grant
        // first, so nothing here can exceed it.
        if let Some(register) = session_registration() {
            c.call(register)
                .await
                .map_err(|e| format!("the agent session's grant: {e}"))?;
        }
        if narrow {
            c.call(api::Request::GrantsNarrow(grant))
                .await
                .map_err(|e| format!("grants.narrow: {e}"))?;
        }
        let server = Dreamer {
            client: Arc::new(c),
            leases: Arc::default(),
        }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("mcp: {e}"))?;
        server.waiting().await.map_err(|e| format!("mcp: {e}"))?;
        Ok::<(), String>(())
    })?;
    Ok(())
}
