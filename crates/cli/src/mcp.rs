//! `dream mcp`: an MCP server on standard input and output (spec 12.6),
//! so an MCP client (an agent) can use Dreamer's tools.
//!
//! Every tool is one typed API call over an ordinary connection to the
//! daemon, so the daemon's checks (grants, targets, bounds) apply exactly
//! as for any client. With `--method`, `--pane`, or `--all-panes` the
//! connection first narrows its own grant (`grants.narrow`) to just that,
//! before any tool runs; it can never widen afterwards. A refused or
//! failed call is a tool error the agent can read, never a crash.
//!
//! The app tools (`app_*`) build packages with `dream app` in a directory
//! inside the apps directory (appdir.rs), which needs no daemon; an image
//! preview and launching an app are calls on this connection like the
//! rest, and installing is allowed when its grant may launch apps.

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
    /// Where the app tools work (see appdir.rs), or why they cannot.
    apps_root: Result<std::path::PathBuf, String>,
    /// Whether this server may install apps, or why not: installing
    /// needs the grant that launches them.
    install: Result<(), String>,
    /// The pane the agent runs in (`TMUX_PANE`): apps open beside it.
    own_pane: Option<u32>,
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

/// Which panes a collector reads: exactly one of these.
#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectFrom {
    /// Every pane, including panes created later.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    all: bool,
    /// The panes of the session with this name.
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// The panes of window @N.
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<u32>,
    /// These panes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    panes: Vec<u32>,
    /// Panes whose current command (such as cargo or pytest) matches this
    /// regular expression, while it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

/// What makes a row: exactly one of line, command_end, idle_ms, bell,
/// exit, or every_s.
#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectWhen {
    /// A committed output line matches this regular expression (Rust
    /// syntax); named groups (?P<name>...) become fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ignore_case: bool,
    /// A command finished (needs the shell's OSC 133 marks); the row has
    /// its exit status and the field duration_ms.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    command_end: bool,
    /// The pane wrote nothing for this many ms after writing something.
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    bell: bool,
    /// The pane's process ended.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    exit: bool,
    /// Every this many seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    every_s: Option<u32>,
}

#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectField {
    name: String,
    /// A template: $1, ${1}, or ${name} is a group of `line`.
    value: String,
}

/// What a row keeps besides the matched line.
#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectCapture {
    /// Fields from templates over `line`'s groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fields: Vec<CollectField>,
    /// The matched line is a JSON object: its members become fields.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    json: bool,
    /// Keep the pane's screen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    screen: bool,
    /// Keep the pane's last N lines (at most 1000).
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<u32>,
    /// Lines before a matched line (at most 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<u32>,
    /// Lines after a matched line (at most 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<u32>,
}

#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectKeep {
    /// Rows kept (default 5000, at most 100000); the oldest go first.
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<u32>,
    /// Rows older than this many seconds go (default 7 days).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_age_s: Option<u64>,
}

#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorCreate {
    /// Letters, digits, `-`, `_`, or `.`; unique. Defaults to the
    /// preset's name.
    #[serde(default)]
    name: String,
    /// A shipped definition to start from: errors, test-failures,
    /// commands, urls, or paths. Each part given here replaces the
    /// preset's.
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<CollectFrom>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when: Option<CollectWhen>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture: Option<CollectCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep: Option<CollectKeep>,
    /// Rows per second per pane (default 10); more are dropped and
    /// counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    per_second: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorQuery {
    /// The collector's name.
    name: String,
    /// Only rows from these panes.
    #[serde(default)]
    panes: Vec<u32>,
    /// Only rows with each of these fields at exactly this value, such as
    /// {"severity": "error"}.
    #[serde(default)]
    fields: std::collections::BTreeMap<String, String>,
    /// Only rows from the last this many seconds.
    since_s: Option<u64>,
    /// Only rows after this row id (the first `limit` of them): pick up
    /// where you left off.
    after: Option<u64>,
    /// Rows returned at most (default 100, at most 10000); without
    /// `after`, the newest.
    limit: Option<u32>,
    /// Count rows by a field, or by pane, session, window, cwd, command,
    /// or trigger, instead of listing them.
    group_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CollectorStop {
    /// The collector's name.
    name: String,
    /// Also delete its definition and rows.
    #[serde(default)]
    delete: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryRecall {
    /// Words to look for; empty returns the newest memories.
    #[serde(default)]
    query: String,
    /// Only this scope: user, host, project, or session. Default: every
    /// scope this session reaches.
    scope: Option<String>,
    /// How many at most (default 10, at most 100).
    limit: Option<u32>,
    /// The project's directory; default the directory this server runs in.
    cwd: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryRemember {
    /// The fact, in a sentence or a short paragraph.
    text: String,
    /// What it is about: user (the user's role, preferences, knowledge),
    /// feedback (guidance on how to work), project (decisions, people,
    /// dates in the work), or reference (where to find something).
    kind: String,
    /// Who it applies to: user (everywhere), host (this machine), project
    /// (this project), or session (this session only).
    scope: String,
    /// A short title; default the text's first line.
    title: Option<String>,
    /// One line saying what it is about, for the index.
    description: Option<String>,
    /// Paths, URLs, or other memories' ids.
    #[serde(default)]
    links: Vec<String>,
    /// The project's directory; default the directory this server runs in.
    cwd: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryForget {
    /// The memory's id, such as project/small-commits.
    id: String,
    /// The project's directory; default the directory this server runs in.
    cwd: Option<String>,
}

/// The memory context of this server: `cwd` (else its own directory) and
/// the agent session it runs for (`DREAMER_SESSION_ID`).
fn memory_context(cwd: Option<String>) -> api::MemoryContext {
    let cwd = cwd
        .filter(|c| !c.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.display().to_string())
        })
        .map(|d| std::path::absolute(&d).map_or(d, |a| a.display().to_string()));
    api::MemoryContext {
        cwd,
        session: std::env::var("DREAMER_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty()),
    }
}

fn memory_scope(name: &str) -> Result<i32, String> {
    api::MemoryScope::from_name(name)
        .filter(|s| *s != api::MemoryScope::Unspecified)
        .map(|s| s as i32)
        .ok_or_else(|| format!("scope `{name}`: use user, host, project, or session"))
}

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

#[derive(Deserialize, schemars::JsonSchema)]
struct AppNew {
    /// The app's name: lowercase letters, digits, and -, starting with a
    /// letter. Its id is local.NAME.
    name: String,
    /// "table" (default), "form", or "chat".
    template: Option<String>,
    /// The package directory, inside the apps directory; default NAME.
    dir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppDir {
    /// The package directory, inside the apps directory.
    dir: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppPreview {
    /// The package directory, inside the apps directory.
    dir: String,
    /// "text" (default): the app drawn from its fixtures as text. "image":
    /// the desktop's drawing of it as a PNG; needs a running Dreamer window.
    mode: Option<String>,
    /// For text: WIDTHxHEIGHT, such as 80x24.
    size: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppTest {
    /// The package directory, inside the apps directory.
    dir: String,
    /// Record the screens drawn now as the expected ones, after a
    /// deliberate change.
    #[serde(default)]
    update: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppInstall {
    /// The package directory, inside the apps directory.
    dir: String,
    /// The installed name; default the app's id.
    name: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppLaunch {
    /// The installed name (what app_install said).
    name: String,
    /// Open it beside this pane; default the pane this agent runs in.
    beside: Option<u32>,
    /// Open it in a new window instead.
    #[serde(default)]
    new_window: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BrowserOpen {
    /// The address, as typed into a browser's address bar: a URL,
    /// example.com, localhost:3000, or an absolute file path.
    url: String,
    /// Load it in this web pane (one you opened) instead of a new one.
    pane: Option<u32>,
    /// Open the new pane beside this pane; default the pane this agent
    /// runs in.
    beside: Option<u32>,
    /// Open it in a new window instead.
    #[serde(default)]
    new_window: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebRead {
    /// A web pane, or the pane of a web app.
    pane: u32,
    /// Also the page's accessibility tree: each node's id (for web_click
    /// and web_type), depth, role, name, and value.
    #[serde(default)]
    tree: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebPane {
    /// A web pane, or the pane of a web app.
    pane: u32,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebClick {
    /// A web pane, or the pane of a web app.
    pane: u32,
    /// The element: a node id from web_read's tree.
    node: Option<u64>,
    /// Or the element: a CSS selector (its first match).
    selector: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebType {
    /// A web pane, or the pane of a web app.
    pane: u32,
    /// The text, inserted at the caret.
    text: String,
    /// The element to focus first: a node id from web_read's tree.
    node: Option<u64>,
    /// Or the element to focus first: a CSS selector (its first match).
    selector: Option<String>,
    /// Then press Enter, which submits a form from one of its fields.
    #[serde(default)]
    submit: bool,
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

    /// A package directory inside the apps directory, as a path.
    fn app_dir(&self, dir: &str) -> Result<String, String> {
        let root = self.apps_root.as_ref().map_err(Clone::clone)?;
        crate::appdir::resolve(root, dir).map(|p| p.display().to_string())
    }

    /// `dream app ARGS`: what it printed and whether it succeeded, or why
    /// it could not run.
    async fn dream_app(&self, args: &[&str]) -> Result<crate::appdir::Ran, String> {
        let mut all = vec!["app".to_owned()];
        all.extend(args.iter().map(|a| (*a).to_owned()));
        crate::appdir::dream(all).await
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

    #[tool(
        description = "Create a collector: a standing rule in the daemon that turns terminal output into rows, kept across restarts. Start from a preset (errors, test-failures, commands, urls, paths) or give from (which panes), when (a line regex, command_end, idle_ms, bell, exit, every_s), capture, and keep; parts you give replace the preset's. It only reads panes. Read its rows with collector_query."
    )]
    async fn collector_create(
        &self,
        Parameters(p): Parameters<CollectorCreate>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.name.is_empty() && p.preset.is_none() {
            return Ok(failed("give a name or a preset".into()));
        }
        let spec = match serde_json::to_value(&p).and_then(serde_json::from_value) {
            Ok(s) => s,
            Err(e) => return Ok(failed(format!("collector_create: {e}"))),
        };
        self.call(api::Request::CollectCreate(spec)).await
    }

    #[tool(
        description = "Rows a collector recorded, oldest first: filter by panes, exact field values, time (since_s), or row id (after); or with group_by count rows per value of a field (or pane, session, window, cwd, command, trigger), most first."
    )]
    async fn collector_query(
        &self,
        Parameters(p): Parameters<CollectorQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let since_unix_ms = p.since_s.map(|s| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            now.saturating_sub(s.saturating_mul(1000))
        });
        self.call(api::Request::CollectQuery(api::CollectQuery {
            name: p.name,
            panes: p.panes,
            fields: p
                .fields
                .into_iter()
                .map(|(name, value)| api::CollectField { name, value })
                .collect(),
            since_unix_ms,
            after: p.after,
            limit: p.limit,
            group_by: p.group_by,
            ..api::CollectQuery::default()
        }))
        .await
    }

    #[tool(
        description = "List collectors: each one's definition, whether it runs, rows kept, rows collected and dropped, the panes it follows, and its last error."
    )]
    async fn collector_list(&self, _: Parameters<Nothing>) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::CollectList(api::Empty::default()))
            .await
    }

    #[tool(
        description = "Stop a collector; its rows stay queryable. With delete, also remove its definition and rows."
    )]
    async fn collector_stop(
        &self,
        Parameters(p): Parameters<CollectorStop>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::CollectStop(api::CollectRef {
            name: p.name,
            delete: p.delete,
        }))
        .await
    }

    #[tool(
        description = "Recall durable memory: notes kept across sessions and agents about the user, this host, this project, and this session. Finds memories by words (no query: the newest), best first, each with its id, kind, title, and text. Recall before asking the user something they may have told an agent before."
    )]
    async fn memory_recall(
        &self,
        Parameters(p): Parameters<MemoryRecall>,
    ) -> Result<CallToolResult, ErrorData> {
        let scope = match p.scope.as_deref().map(memory_scope).transpose() {
            Ok(s) => s.unwrap_or_default(),
            Err(why) => return Ok(failed(why)),
        };
        self.call(api::Request::MemoryRecall(api::MemoryRecall {
            query: p.query,
            scope,
            limit: p.limit,
            context: Some(memory_context(p.cwd)),
        }))
        .await
    }

    #[tool(
        description = "Remember a fact worth having in a later session, with any agent: something the user told you about themselves or how to work, a decision in this project, or where to find something. One fact per memory; do not store secrets or what the code or git history already records. Returns the new memory's id and file."
    )]
    async fn memory_remember(
        &self,
        Parameters(p): Parameters<MemoryRemember>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = match api::MemoryKind::from_name(&p.kind)
            .filter(|k| *k != api::MemoryKind::Unspecified)
        {
            Some(k) => k as i32,
            None => {
                return Ok(failed(format!(
                    "kind `{}`: use user, feedback, project, or reference",
                    p.kind
                )));
            }
        };
        let scope = match memory_scope(&p.scope) {
            Ok(s) => s,
            Err(why) => return Ok(failed(why)),
        };
        self.call(api::Request::MemoryRemember(api::MemoryRemember {
            text: p.text,
            kind,
            scope,
            title: p.title,
            description: p.description,
            links: p.links,
            context: Some(memory_context(p.cwd)),
        }))
        .await
    }

    #[tool(
        description = "Forget a memory that is wrong or no longer true: deletes its file. Only when the user asks, or a memory you kept turned out wrong. Needs a grant from `own` up."
    )]
    async fn memory_forget(
        &self,
        Parameters(p): Parameters<MemoryForget>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::MemoryForget(api::MemoryRef {
            id: p.id,
            context: Some(memory_context(p.cwd)),
        }))
        .await
    }

    #[tool(
        description = "Create a working app package to start from: app.toml, app.ts (TypeScript), fixtures to preview and test it, and types/dreamer.d.ts. Templates: table (a list of rows), form (fields and a button), chat (a log and an input). Directories are inside the apps directory (the working directory unless DREAMER_APPS_ROOT says otherwise). Then edit app.ts and run app_check."
    )]
    async fn app_new(
        &self,
        Parameters(p): Parameters<AppNew>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = match self.app_dir(p.dir.as_deref().unwrap_or(&p.name)) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        let template = p.template.unwrap_or_else(|| "table".into());
        let ran = match self
            .dream_app(&["new", &p.name, "--template", &template, "--dir", &dir])
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(failed(e)),
        };
        if !ran.ok {
            return Ok(failed(ran.stderr));
        }
        Ok(text(serde_json::json!({
            "dir": dir,
            "id": format!("local.{}", p.name),
            "output": ran.stdout.trim(),
        })))
    }

    #[tool(
        description = "Check an app package: build its app.ts, then the manifest, the grant it needs, TypeScript type errors, and the view tree. Returns ok, and problems as file, line, column, and message (a syntax error is in error instead); fix them in order and check again. The check does not hold the methods app.ts calls to the manifest's capabilities: add each one you call."
    )]
    async fn app_check(
        &self,
        Parameters(p): Parameters<AppDir>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = match self.app_dir(&p.dir) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        let ran = match self.dream_app(&["check", &dir, "--json"]).await {
            Ok(r) => r,
            Err(e) => return Ok(failed(e)),
        };
        Ok(match serde_json::from_str::<Value>(ran.stdout.trim()) {
            Ok(v) => text(v),
            Err(_) => failed(ran.stderr),
        })
    }

    #[tool(
        description = "Preview an app from its fixtures, without the daemon's data. mode text (default) returns the screen as text, as the app host draws it in a terminal. mode image returns a PNG of the app as the Dreamer desktop draws it natively; it needs a running Dreamer window, opens the preview in a new window of its own, and closes it again."
    )]
    async fn app_preview(
        &self,
        Parameters(p): Parameters<AppPreview>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = match self.app_dir(&p.dir) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        match p.mode.as_deref() {
            None | Some("text") => {
                let mut args = vec!["preview", dir.as_str(), "--text"];
                if let Some(s) = p.size.as_deref() {
                    args.extend(["--size", s]);
                }
                Ok(match self.dream_app(&args).await {
                    Ok(r) if r.ok => CallToolResult::success(vec![ContentBlock::text(r.stdout)]),
                    Ok(r) => failed(r.stderr),
                    Err(e) => failed(e),
                })
            }
            Some("image") => {
                if p.size.is_some() {
                    return Ok(failed(
                        "size is for mode text; the desktop draws the pane at its size".into(),
                    ));
                }
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let file = std::env::temp_dir().join(format!(
                    "dreamer-mcp-preview-{}-{n}.png",
                    std::process::id()
                ));
                let shot = crate::preview::agent_png(&self.client, &dir, &file).await;
                let png = std::fs::read(&file);
                let _ = std::fs::remove_file(&file);
                Ok(match (shot, png) {
                    (Ok((got, problems)), Ok(bytes)) => {
                        let mut said =
                            format!("{}x{} as the desktop draws it", got.width, got.height);
                        for problem in &problems {
                            said.push_str(&format!("\nwarning: {problem}"));
                        }
                        CallToolResult::success(vec![
                            ContentBlock::image(crate::appdir::base64(&bytes), "image/png"),
                            ContentBlock::text(said),
                        ])
                    }
                    (Ok(_), Err(e)) => failed(format!("{}: {e}", file.display())),
                    (Err(e), _) => failed(e),
                })
            }
            Some(other) => Ok(failed(format!("mode `{other}`: use text or image"))),
        }
    }

    #[tool(
        description = "Test an app: replay fixtures/events.jsonl and compare the screen after each event with fixtures/expect/. Returns ok and what differs. With update, record the screens drawn now as the expected ones (after a deliberate change)."
    )]
    async fn app_test(
        &self,
        Parameters(p): Parameters<AppTest>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = match self.app_dir(&p.dir) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        let mut args = vec!["test", dir.as_str()];
        if p.update {
            args.push("--update");
        }
        Ok(match self.dream_app(&args).await {
            Ok(r) => text(serde_json::json!({
                "ok": r.ok,
                "output": if r.ok { r.stdout.trim() } else { r.stderr.as_str() },
            })),
            Err(e) => failed(e),
        })
    }

    #[tool(
        description = "Install an app package for the user: check it, draw it from its fixtures, and copy it into Dreamer's apps directory. A version it replaces is kept (dream app rollback NAME puts it back). Returns the installed name for app_launch."
    )]
    async fn app_install(
        &self,
        Parameters(p): Parameters<AppInstall>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(e) = &self.install {
            return Ok(failed(e.clone()));
        }
        let dir = match self.app_dir(&p.dir) {
            Ok(d) => d,
            Err(e) => return Ok(failed(e)),
        };
        let mut args = vec!["install", dir.as_str()];
        if let Some(n) = p.name.as_deref() {
            args.extend(["--name", n]);
        }
        let ran = match self.dream_app(&args).await {
            Ok(r) => r,
            Err(e) => return Ok(failed(e)),
        };
        if !ran.ok {
            return Ok(failed(ran.stderr));
        }
        // "installed ID VERSION as NAME (DIR)"
        let name = ran
            .stdout
            .split(" as ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or_default()
            .to_owned();
        Ok(text(serde_json::json!({
            "name": name,
            "output": ran.stdout.trim(),
        })))
    }

    #[tool(
        description = "Open a web page for the user in Dreamer's browser: a new pane beside the pane you run in (or `beside` another pane, or in a new window), drawn by the user's desktop; or, with `pane`, load the address in that web pane. You own a pane you opened. Splitting or loading into a pane you did not create may wait for the user's approval. The page gets no Dreamer API, and you cannot read it from here."
    )]
    async fn browser_open(
        &self,
        Parameters(p): Parameters<BrowserOpen>,
    ) -> Result<CallToolResult, ErrorData> {
        // The daemon makes the address a URL, as the address bar does.
        let url = p.url;
        if let Some(pane) = p.pane {
            let request = api::Request::WebNavigate(api::WebNavigate {
                pane,
                url: url.clone(),
                ..api::WebNavigate::default()
            });
            return Ok(match self.client.call(request).await {
                Ok(_) => text(serde_json::json!({ "pane": pane, "url": url })),
                Err(e) => failed(e.to_string()),
            });
        }
        let beside = if p.new_window {
            None
        } else {
            p.beside.or(self.own_pane)
        };
        let session = match beside {
            Some(_) => None,
            None => match crate::build::session_of(&self.client, None).await {
                Ok(s) => Some(s),
                Err(e) => return Ok(failed(e)),
            },
        };
        let request = api::Request::WebOpen(api::WebOpen {
            url: url.clone(),
            pane: beside,
            session,
            ..api::WebOpen::default()
        });
        Ok(match self.client.call(request).await {
            Ok(api::Response::WebOpen(o)) => text(serde_json::json!({
                "pane": o.pane,
                "url": url,
                "beside": beside,
            })),
            Ok(_) => failed("web.open: unexpected answer".into()),
            Err(e) => failed(e.to_string()),
        })
    }

    #[tool(
        description = "Read a page the user's Dreamer window shows, in a web pane or a web app's pane: its address, title, and text, and with `tree` its accessibility tree (each node's id, depth, role, name, and value). Needs the web.inspect capability in your grant."
    )]
    async fn web_read(
        &self,
        Parameters(p): Parameters<WebRead>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::WebRead(api::WebRead {
            pane: p.pane,
            tree: p.tree,
            wait_ms: None,
        }))
        .await
    }

    #[tool(
        description = "Click an element of a page in a web pane or a web app's pane, named by `node` (an id from web_read's tree) or `selector` (CSS, its first match): it is scrolled into view and clicked in its middle, as with a mouse. Needs web.inspect; in a pane you did not open it may wait for the user's approval."
    )]
    async fn web_click(
        &self,
        Parameters(p): Parameters<WebClick>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::WebClick(api::WebClick {
            pane: p.pane,
            node: p.node,
            selector: p.selector,
            wait_ms: None,
        }))
        .await
    }

    #[tool(
        description = "Type text into a page in a web pane or a web app's pane: into the element named by `node` or `selector` (focused first), else where the page's focus is; with `submit`, then press Enter, which submits a form from its field. The clipboard is not used. Needs web.inspect; in a pane you did not open it may wait for the user's approval."
    )]
    async fn web_type(
        &self,
        Parameters(p): Parameters<WebType>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call(api::Request::WebType(api::WebType {
            pane: p.pane,
            node: p.node,
            selector: p.selector,
            text: p.text,
            submit: p.submit,
            wait_ms: None,
        }))
        .await
    }

    #[tool(
        description = "An image of a page in a web pane or a web app's pane, as Chromium draws it in the user's Dreamer window: the page only, at the display's scale. Needs web.inspect."
    )]
    async fn web_screenshot(
        &self,
        Parameters(p): Parameters<WebPane>,
    ) -> Result<CallToolResult, ErrorData> {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file =
            std::env::temp_dir().join(format!("dreamer-mcp-web-{}-{n}.png", std::process::id()));
        let shot = self
            .client
            .call(api::Request::WebScreenshot(api::WebScreenshot {
                pane: p.pane,
                path: file.display().to_string(),
                wait_ms: None,
            }))
            .await;
        let png = std::fs::read(&file);
        let _ = std::fs::remove_file(&file);
        Ok(match (shot, png) {
            (Ok(api::Response::WebScreenshot(got)), Ok(bytes)) => CallToolResult::success(vec![
                ContentBlock::image(crate::appdir::base64(&bytes), "image/png"),
                ContentBlock::text(format!("{}x{} pixels", got.width, got.height)),
            ]),
            (Ok(_), Ok(_)) => failed("web.screenshot: unexpected answer".into()),
            (Ok(_), Err(e)) => failed(format!("{}: {e}", file.display())),
            (Err(e), _) => failed(e.to_string()),
        })
    }

    #[tool(
        description = "Open an installed app for the user in a pane beside the pane you run in (or `beside` another pane, or in a new window), with the grant it declares; that grant must be within yours. You own the new pane. Splitting a pane you did not create may wait for the user's approval."
    )]
    async fn app_launch(
        &self,
        Parameters(p): Parameters<AppLaunch>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = p.name.clone();
        let request = tokio::task::spawn_blocking(move || crate::build::launch_request(&name))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        let mut r = match request {
            Ok(r) => r,
            Err(e) => return Ok(failed(e)),
        };
        r.split = if p.new_window {
            None
        } else {
            p.beside.or(self.own_pane)
        };
        r.session = match crate::build::session_of(&self.client, r.split).await {
            Ok(s) => s,
            Err(e) => return Ok(failed(e)),
        };
        Ok(
            match self.client.call(api::Request::AppsLaunch(r.clone())).await {
                Ok(api::Response::AppsLaunch(l)) => text(serde_json::json!({
                    "app_id": r.app_id,
                    "name": p.name,
                    "pane": l.pane,
                    "beside": r.split,
                    "launch_id": l.launch_id,
                })),
                Ok(_) => failed("apps.launch: unexpected answer".into()),
                Err(e) => failed(e.to_string()),
            },
        )
    }
}

/// `observe`, `own`, or `elsewhere` as an act level.
pub(crate) fn parse_level(v: &str) -> Result<i32, String> {
    Ok(match v {
        "observe" => api::ActLevel::Observe,
        "own" => api::ActLevel::Own,
        "elsewhere" => api::ActLevel::Elsewhere,
        _ => return Err("the level is observe, own, or elsewhere".into()),
    } as i32)
}

/// An agent's grant at `level`: every pane, and the methods of its tools.
pub(crate) fn agent_grant(level: i32) -> api::Grant {
    api::Grant {
        methods: agent_methods(level),
        all_panes: true,
        level,
        ..api::Grant::default()
    }
}

/// Whether `methods` (a grant's, `service.*` included) allow `method`.
fn lists(methods: &[String], method: &str) -> bool {
    methods.iter().any(|m| {
        m == method
            || m.strip_suffix(".*").is_some_and(|s| {
                method
                    .strip_prefix(s)
                    .is_some_and(|rest| rest.starts_with('.'))
            })
    })
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

/// For a command an agent runs (such as `dream memory`): inside an agent
/// session's harness, take the session's grant; else, under a harness
/// Dreamer started with an agent level (`DREAMER_AGENT_LEVEL`, set by
/// `dream agents pi`), narrow to the agent's grant at that level, as the
/// pi extension does. Run by the user, nothing changes.
pub(crate) async fn as_agent(c: &client::Client) -> Result<(), String> {
    if let Some(register) = session_registration() {
        c.call(register)
            .await
            .map_err(|e| format!("the agent session's grant: {e}"))?;
        return Ok(());
    }
    let level = match std::env::var("DREAMER_AGENT_LEVEL").as_deref() {
        Ok("observe") => api::ActLevel::Observe,
        Ok("own") => api::ActLevel::Own,
        Ok("elsewhere") => api::ActLevel::Elsewhere,
        Ok("") | Err(_) => return Ok(()),
        Ok(other) => {
            return Err(format!(
                "DREAMER_AGENT_LEVEL `{other}`: use observe, own, or elsewhere"
            ));
        }
    } as i32;
    c.call(api::Request::GrantsNarrow(api::Grant {
        methods: agent_methods(level),
        all_panes: true,
        level,
        ..api::Grant::default()
    }))
    .await
    .map_err(|e| format!("grants.narrow: {e}"))?;
    Ok(())
}

/// `dream mcp [--level L] [--method M]... [--pane %N]... [--all-panes]
/// [--web-inspect] [tmux options]`.
pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let mut grant = api::Grant::default();
    let mut narrow = false;
    let mut web = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            // Adds reading and driving pages to a narrowed grant; the
            // owner's has it already.
            "--web-inspect" => {
                web = true;
                continue;
            }
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
                grant.level = it
                    .next()
                    .ok_or("--level needs observe, own, or elsewhere")
                    .and_then(|v| {
                        parse_level(v).map_err(|_| "--level needs observe, own, or elsewhere")
                    })?;
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
    if web && narrow {
        grant.methods.push(api::INSPECT_WEB.to_owned());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let c = session(d).await?;
        // Started by an agent session's harness: take the session's grant
        // first, so nothing here can exceed it.
        let registered = session_registration();
        // Installing writes no pane, so the daemon does not see it: this
        // server allows it exactly when its grant may launch apps.
        let install = if registered.is_some() {
            Err("an agent session's grant does not launch apps, so it does not install them".into())
        } else if narrow && !lists(&grant.methods, api::method::APPS_LAUNCH) {
            Err(format!(
                "installing an app needs `{}` in this server's grant (an agent at own or elsewhere)",
                api::method::APPS_LAUNCH
            ))
        } else {
            Ok(())
        };
        if let Some(register) = registered {
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
            apps_root: crate::appdir::root(),
            install,
            own_pane: std::env::var("TMUX_PANE")
                .ok()
                .and_then(|p| p.trim_start_matches('%').parse().ok()),
        }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("mcp: {e}"))?;
        server.waiting().await.map_err(|e| format!("mcp: {e}"))?;
        Ok::<(), String>(())
    })?;
    Ok(())
}
