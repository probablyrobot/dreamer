//! Agent sessions (`agent.sessions.*`): a harness process of the daemon per
//! session, its updates normalised into one event model, and a transcript.
//!
//! - **Transcript.** Every event is appended, as one JSON line, to
//!   `<state>/sessions/<id>.jsonl` before anyone sees it, numbered from 1.
//!   A session's record (harness, directory, grant, and the harness's own
//!   session for resume) is `<id>.json` beside it. Both outlive the daemon:
//!   after a restart the sessions are listed as detached and their events
//!   replay from the file.
//! - **Harness.** A child process of the daemon, in its own process group,
//!   with no API key variable in its environment, driven over its own
//!   protocol: pi's RPC mode, Claude Code's stream-json, Codex's app
//!   server, or the Agent Client Protocol (`agents.<name>.protocol`). No
//!   prompt reaches a harness that is not logged in by its own login. Closing a session ends
//!   its process group; stopping the daemon ends every harness and leaves
//!   the sessions detached. Sending to a detached session (or resuming it)
//!   starts the harness again on its own session.
//! - **Grant.** Each session holds a grant within its creator's, without
//!   any `agent.sessions`, `apps`, or owner authority. The harness gets the
//!   session's id and a ticket file; a process it starts trades them with
//!   `apps.register` for a connection holding exactly that grant (the
//!   daemon checks the process descends from the harness). Only callers
//!   other than the session answer its approval requests.

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{AgentSessionState as St, RpcError, code};
use providers::session::{Control, Input, Protocol, StopReason, ToolStatus, Update};
use tokio::sync::broadcast;

use crate::log;

/// Events a live follower may fall behind before it reads the transcript
/// again.
const LIVE: usize = 1024;
/// How long closing waits for the harness to end.
const CLOSE_WAIT: Duration = Duration::from_secs(15);
/// How long a registration waits for its session to finish starting.
const REGISTER_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_LIMIT: u32 = 1000;
const MAX_LIMIT: u32 = 10_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn not_found(id: &str) -> RpcError {
    RpcError::new(code::NOT_FOUND, format!("no agent session {id}"))
}

/// Who calls: the connection's instance and grant.
#[derive(Debug, Clone)]
pub struct Caller {
    pub instance: String,
    pub grant: api::Grant,
}

impl Caller {
    fn owner(&self) -> bool {
        self.grant.owner
    }
}

struct Info {
    s: api::AgentSession,
    harness_file: Option<String>,
    creator: String,
    /// Approval requests waiting for an answer.
    pending: BTreeSet<String>,
}

struct Log {
    path: PathBuf,
    seq: u64,
}

struct Running {
    control: Control,
    pump: tokio::task::JoinHandle<()>,
    ticket_path: PathBuf,
}

pub struct Session {
    id: String,
    meta_path: PathBuf,
    info: Mutex<Info>,
    log: Mutex<Log>,
    live: broadcast::Sender<api::AgentSessionEvent>,
    /// The running harness. Held across a start, so starts and closes of
    /// one session never interleave.
    run: tokio::sync::Mutex<Option<Running>>,
    ticket: Mutex<Option<String>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Session>>>,
    /// The daemon is stopping: harnesses that end now leave their sessions
    /// detached for that reason.
    stopping: AtomicBool,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions").finish_non_exhaustive()
    }
}

fn dir() -> PathBuf {
    platform::state_dir().join("sessions")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Write `path` in one step.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The newest sequence number in a transcript.
fn last_seq(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| {
            t.lines()
                .rev()
                .find_map(|l| serde_json::from_str::<api::AgentSessionEvent>(l).ok())
        })
        .map_or(0, |e| e.seq)
}

/// The default grant: the terminal tools on every pane, observing.
fn default_grant() -> api::Grant {
    let level = api::ActLevel::Observe as i32;
    api::Grant {
        methods: api::agent_methods(level),
        all_panes: true,
        level,
        ..api::Grant::default()
    }
}

/// A session's grant never holds the owner's authority, sessions, apps, or
/// approvals: a harness never starts sessions or answers for the user.
fn confine(mut g: api::Grant) -> api::Grant {
    g.owner = false;
    g.methods.retain(|m| {
        !(m.starts_with("agent.") || m.starts_with("apps.") || m.starts_with("approvals."))
    });
    g
}

impl Session {
    fn snapshot(&self) -> api::AgentSession {
        let info = self.info.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = info.s.clone();
        s.last_seq = self.log.lock().map_or(0, |l| l.seq);
        s
    }

    fn state(&self) -> St {
        let info = self.info.lock().unwrap_or_else(|e| e.into_inner());
        St::try_from(info.s.state).unwrap_or(St::Unspecified)
    }

    fn set_state(&self, st: St, detail: Option<String>) {
        let mut info = self.info.lock().unwrap_or_else(|e| e.into_inner());
        info.s.state = st as i32;
        if detail.is_some() || st != St::Detached {
            info.s.detail = detail;
        }
    }

    fn persist(&self) {
        let info = self.info.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = info.s.clone();
        // What is true only while the daemon runs is not kept.
        s.pid = None;
        if s.state != St::Closed as i32 {
            s.state = St::Detached as i32;
        }
        let v = serde_json::json!({
            "session": s,
            "harness_file": info.harness_file,
            "creator": info.creator,
        });
        drop(info);
        if let Err(e) = write_atomic(&self.meta_path, &format!("{v}\n")) {
            log::warn(format!("agent session {}: {e}", self.id));
        }
    }

    /// Append one event to the transcript, then tell followers. In order:
    /// the transcript lock is held for both.
    fn append(&self, kind: api::agent_session_event::Kind) -> api::AgentSessionEvent {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.seq += 1;
        let e = api::AgentSessionEvent {
            seq: log.seq,
            unix_ms: now_ms(),
            kind: Some(kind),
        };
        let line = match serde_json::to_string(&e) {
            Ok(l) => l + "\n",
            Err(_) => return e,
        };
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(err) = written {
            log::warn(format!(
                "agent session {}: transcript {}: {err}",
                self.id,
                log.path.display()
            ));
        }
        let _ = self.live.send(e.clone());
        e
    }

    fn status(&self, st: St, detail: Option<String>) {
        let harness_session = self
            .info
            .lock()
            .ok()
            .and_then(|i| i.s.harness_session.clone());
        self.append(api::agent_session_event::Kind::Status(
            api::AgentSessionStatus {
                state: st as i32,
                detail,
                harness_session,
            },
        ));
    }

    /// Events after `after`, at most `limit`, from the transcript; and
    /// whether more follow. Read under the transcript lock, so a line is
    /// never read half written.
    pub fn read(&self, after: u64, limit: usize) -> (Vec<api::AgentSessionEvent>, bool) {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let text = std::fs::read_to_string(&log.path).unwrap_or_default();
        drop(log);
        let mut out = Vec::new();
        let mut more = false;
        for l in text.lines() {
            let Ok(e) = serde_json::from_str::<api::AgentSessionEvent>(l) else {
                continue;
            };
            if e.seq <= after {
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(e);
        }
        (out, more)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<api::AgentSessionEvent> {
        self.live.subscribe()
    }

    fn visible_to(&self, caller: &Caller) -> bool {
        caller.owner() || self.info.lock().is_ok_and(|i| i.creator == caller.instance)
    }
}

fn to_event(u: Update) -> Option<api::agent_session_event::Kind> {
    use api::agent_session_event::Kind as K;
    Some(match u {
        Update::Text(t) => K::TextDelta(api::AgentText { text: t }),
        Update::Thinking(t) => K::ThinkingDelta(api::AgentText { text: t }),
        Update::Tool(t) => K::ToolCall(api::AgentToolCall {
            id: t.id,
            name: t.name,
            status: match t.status {
                ToolStatus::Started => api::AgentToolStatus::Started,
                ToolStatus::Updated => api::AgentToolStatus::Updated,
                ToolStatus::Finished => api::AgentToolStatus::Finished,
                ToolStatus::Failed => api::AgentToolStatus::Failed,
            } as i32,
            input: t.input.map(|v| v.to_string()),
            output: t.output,
        }),
        Update::Approval(a) => K::ApprovalRequest(api::AgentApprovalRequest {
            id: a.id,
            title: a.title,
            message: a.message,
            options: a.options,
            expires_unix_ms: a.timeout_ms.map(|t| now_ms() + t),
        }),
        Update::Plan(entries) => K::Plan(api::AgentPlan {
            entries: entries
                .into_iter()
                .map(|e| api::AgentPlanEntry {
                    content: e.content,
                    status: e.status,
                    priority: e.priority,
                })
                .collect(),
        }),
        Update::Usage(u) => K::Usage(api::AgentUsage {
            input_tokens: u.input,
            output_tokens: u.output,
            cache_read_tokens: u.cache_read,
            cache_write_tokens: u.cache_write,
            cost_micro_usd: u
                .cost_usd
                .filter(|c| c.is_finite() && *c >= 0.0)
                .map(|c| (c * 1e6).round() as u64),
        }),
        Update::TurnEnd(r) => K::TurnEnd(api::AgentTurnEnd {
            stop_reason: match r {
                StopReason::EndTurn => api::AgentStopReason::EndTurn,
                StopReason::Cancelled => api::AgentStopReason::Cancelled,
                StopReason::Error => api::AgentStopReason::Error,
            } as i32,
        }),
        Update::Error(m) => K::Error(api::AgentError { message: m }),
        Update::HarnessSession { .. } | Update::Exited(_) => return None,
    })
}

/// A configured program: a path as given, a bare name from `PATH`.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        Some(PathBuf::from(program)).filter(|p| p.is_file())
    } else {
        providers::locate_in_path(program)
    }
}

/// pi's own login check for the provider `spec` is pinned to, off the
/// daemon's async threads.
async fn pi_login_gate(spec: &providers::session::Spec) -> Result<(), String> {
    let (program, args) = (spec.program.clone(), spec.args.clone());
    let dir = spec
        .agent_dir
        .clone()
        .unwrap_or_else(providers::pi::profile_dir);
    let provider = spec
        .provider
        .clone()
        .ok_or("no provider is pinned for pi")?;
    tokio::task::spawn_blocking(move || {
        providers::pi::require_login(&program, &args, &dir, &provider)
    })
    .await
    .map_err(|e| format!("the login check failed: {e}"))?
}

/// Whether the harness may take a prompt: logged in by its own login, and
/// not with an API key. ACP agents report their own login when a session
/// starts.
async fn login_gate(protocol: Protocol, spec: &providers::session::Spec) -> Result<(), String> {
    let kind = match protocol {
        Protocol::Pi => return pi_login_gate(spec).await,
        Protocol::Acp => return Ok(()),
        Protocol::Claude => providers::Kind::ClaudeCli,
        Protocol::Codex => providers::Kind::CodexCli,
    };
    let probe = providers::probe_command(kind, Some(spec.program.clone()), spec.args.clone()).await;
    let cli = match kind {
        providers::Kind::ClaudeCli => "Claude Code",
        providers::Kind::CodexCli => "Codex",
    };
    match probe.state {
        providers::State::Ready => Ok(()),
        providers::State::LoggedOut => Err(match kind {
            providers::Kind::ClaudeCli => {
                "Claude Code is not logged in; run `claude` and log in with `/login`".to_owned()
            }
            providers::Kind::CodexCli => "Codex is not logged in; run `codex login`".to_owned(),
        }),
        providers::State::MeteredLogin(why) => Err(format!(
            "{cli} is logged in with an API key ({why}), which would be metered; log it in to your subscription"
        )),
        providers::State::NotInstalled => Err(format!("{cli} is not installed")),
        providers::State::Incompatible(why) => Err(format!("{cli}: {why}")),
        providers::State::Unknown(why) => Err(format!("{cli}'s login could not be checked: {why}")),
    }
}

/// `dream` beside the daemon (or `DREAMER_DREAM`): Dreamer's tools for the
/// harnesses that take an MCP server, as `dream mcp`. Inside a session it
/// takes the session's grant (`DREAMER_SESSION_ID` and its ticket).
fn dream_tools() -> Option<providers::Tools> {
    let dream = std::env::var_os("DREAMER_DREAM")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| Some(e.parent()?.join("dream")))
        })
        .filter(|p| p.is_file())?;
    Some(providers::Tools {
        program: dream,
        args: vec!["mcp".into()],
    })
}

/// How to start the session's harness, from the settings in force: its
/// protocol and its start.
fn spec(
    info: &Info,
    env: Vec<(String, String)>,
) -> Result<(Protocol, providers::session::Spec), String> {
    let settings = workspace_config::load_settings().map_err(|e| e.to_string())?;
    let name = info.s.harness.as_str();
    let h = settings
        .agents
        .harness(name)
        .ok_or_else(|| format!("no harness `{name}` in the agents settings"))?;
    let protocol = settings
        .agents
        .protocol(name)
        .and_then(|p| Protocol::parse(p.name()))
        .ok_or_else(|| format!("agents.{name}: no protocol"))?;
    let model = info.s.model.clone().or(h.model.clone());
    let cwd = PathBuf::from(&info.s.cwd);
    let command = |default: &str| match &h.command {
        Some(argv) if !argv.is_empty() => Ok((
            resolve(&argv[0])
                .ok_or_else(|| format!("agents.{name}.command: `{}` is not there", argv[0]))?,
            argv[1..].to_vec(),
        )),
        _ => resolve(default).map(|p| (p, Vec::new())).ok_or_else(|| {
            format!("`{default}` is not on PATH; install it, or set agents.{name}.command")
        }),
    };
    if protocol != Protocol::Pi {
        let (program, args) = command(protocol.name())?;
        let tools = dream_tools();
        if tools.is_none() {
            log::warn(format!(
                "agent session {}: `dream` was not found beside dreamerd (or in DREAMER_DREAM); the harness gets no Dreamer tools",
                info.s.id
            ));
        }
        return Ok((
            protocol,
            providers::session::Spec {
                program,
                args,
                cwd,
                env,
                model,
                resume: info.s.harness_session.clone(),
                tools,
                mode: h.mode.clone(),
                ..providers::session::Spec::default()
            },
        ));
    }
    let (program, args) = match &h.command {
        Some(argv) if !argv.is_empty() => command("pi")?,
        _ => {
            let c = providers::pi::Install::default().command();
            if !c.is_file() {
                return Err("pi is not installed; install it with `dream agents update`".to_owned());
            }
            (c, Vec::new())
        }
    };
    let agent_dir = match h.agent_dir {
        Some(d) => d,
        None => {
            // Dreamer's own pi profile is Dreamer's to create.
            let d = providers::pi::profile_dir();
            std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
            d
        }
    };
    let provider = providers::pi::pinned_provider(model.as_deref(), &agent_dir)?;
    Ok((
        protocol,
        providers::session::Spec {
            program,
            args,
            cwd,
            env,
            agent_dir: Some(agent_dir),
            model,
            provider: Some(provider),
            // Decided by the login check when the harness starts.
            offline: true,
            // pi keeps a session on disk once it has a message; resuming one
            // that never had any would fail, and loses nothing when skipped.
            resume: info.harness_file.clone().filter(|f| Path::new(f).is_file()),
            ..providers::session::Spec::default()
        },
    ))
}

impl Sessions {
    /// The sessions kept in the state directory, all detached or closed.
    pub fn load() -> Sessions {
        let sessions = Sessions::default();
        let Ok(entries) = std::fs::read_dir(dir()) else {
            return sessions;
        };
        let mut map = sessions.map.lock().unwrap_or_else(|e| e.into_inner());
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(v) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            else {
                continue;
            };
            let Ok(mut s) = serde_json::from_value::<api::AgentSession>(v["session"].clone())
            else {
                continue;
            };
            if !valid_id(&s.id) {
                continue;
            }
            if s.state != St::Closed as i32 {
                s.state = St::Detached as i32;
            }
            let id = s.id.clone();
            let log_path = dir().join(format!("{id}.jsonl"));
            let seq = last_seq(&log_path);
            map.insert(
                id.clone(),
                Arc::new(Session {
                    id: id.clone(),
                    meta_path: path.clone(),
                    info: Mutex::new(Info {
                        s,
                        harness_file: v["harness_file"].as_str().map(str::to_owned),
                        creator: v["creator"].as_str().unwrap_or_default().to_owned(),
                        pending: BTreeSet::new(),
                    }),
                    log: Mutex::new(Log {
                        path: log_path,
                        seq,
                    }),
                    live: broadcast::channel(LIVE).0,
                    run: tokio::sync::Mutex::new(None),
                    ticket: Mutex::new(None),
                }),
            );
        }
        if !map.is_empty() {
            log::info(format!("agent sessions: {} kept", map.len()));
        }
        drop(map);
        sessions
    }

    /// A session the caller may see.
    pub fn get(&self, caller: &Caller, id: &str) -> Result<Arc<Session>, RpcError> {
        let s = self
            .map
            .lock()
            .ok()
            .and_then(|m| m.get(id).cloned())
            .ok_or_else(|| not_found(id))?;
        if s.visible_to(caller) {
            Ok(s)
        } else {
            Err(not_found(id))
        }
    }

    pub fn list(&self, caller: &Caller) -> api::AgentSessionList {
        let all: Vec<Arc<Session>> = self
            .map
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        let mut sessions: Vec<api::AgentSession> = all
            .iter()
            .filter(|s| s.visible_to(caller))
            .map(|s| s.snapshot())
            .collect();
        sessions.sort_by(|a, b| (a.created_unix_ms, &a.id).cmp(&(b.created_unix_ms, &b.id)));
        api::AgentSessionList { sessions }
    }

    pub async fn create(
        self: &Arc<Self>,
        caller: &Caller,
        p: api::AgentSessionCreate,
    ) -> Result<api::AgentSession, RpcError> {
        let settings = workspace_config::load_settings()
            .map_err(|e| RpcError::new(code::PRECONDITION, e.to_string()))?;
        let harness = p
            .harness
            .clone()
            .unwrap_or_else(|| settings.agents.default_harness().to_owned());
        if settings.agents.harness(&harness).is_none() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!("no harness `{harness}` in the agents settings"),
            ));
        }
        let wanted = match p.grant {
            Some(g) if g != api::Grant::default() => g,
            _ => default_grant(),
        };
        let grant = confine(wanted);
        crate::grants::within(&caller.grant, &grant)
            .map_err(|why| RpcError::new(code::FORBIDDEN, format!("the session's grant: {why}")))?;
        let cwd = match p.cwd {
            Some(c) if !c.is_empty() => PathBuf::from(c),
            _ => std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
        };
        if !cwd.is_dir() {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!("{} is not a directory", cwd.display()),
            ));
        }
        let id = format!("s{}", crate::apps::random_hex(8)?);
        let d = dir();
        platform::private_dir(&d).map_err(|e| RpcError::new(code::INTERNAL, e))?;
        let session = Arc::new(Session {
            id: id.clone(),
            meta_path: d.join(format!("{id}.json")),
            info: Mutex::new(Info {
                s: api::AgentSession {
                    id: id.clone(),
                    harness,
                    cwd: cwd.display().to_string(),
                    state: St::Starting as i32,
                    grant: Some(grant),
                    created_unix_ms: now_ms(),
                    model: p.model,
                    name: p.name,
                    ..api::AgentSession::default()
                },
                harness_file: None,
                creator: caller.instance.clone(),
                pending: BTreeSet::new(),
            }),
            log: Mutex::new(Log {
                path: d.join(format!("{id}.jsonl")),
                seq: 0,
            }),
            live: broadcast::channel(LIVE).0,
            run: tokio::sync::Mutex::new(None),
            ticket: Mutex::new(None),
        });
        session.persist();
        if let Ok(mut m) = self.map.lock() {
            m.insert(id.clone(), session.clone());
        }
        log::info(format!(
            "agent session {id}: created by {} on {}",
            caller.instance,
            session.snapshot().harness
        ));
        self.start(&session).await?;
        Ok(session.snapshot())
    }

    /// Start the session's harness if it is not running.
    async fn start(self: &Arc<Self>, s: &Arc<Session>) -> Result<(), RpcError> {
        let mut run = s.run.lock().await;
        if run.as_ref().is_some_and(|r| !r.pump.is_finished()) {
            return Ok(());
        }
        // The harness's ticket: a process it starts registers with it.
        let ticket = crate::apps::random_hex(32)?;
        let tdir = platform::runtime_dir().join("sessions");
        let ticket_path = tdir.join(&s.id);
        {
            use std::os::unix::fs::OpenOptionsExt;
            platform::private_dir(&tdir).map_err(|e| RpcError::new(code::INTERNAL, e))?;
            let _ = std::fs::remove_file(&ticket_path);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&ticket_path)
                .and_then(|mut f| f.write_all(ticket.as_bytes()))
                .map_err(|e| {
                    RpcError::new(code::INTERNAL, format!("{}: {e}", ticket_path.display()))
                })?;
        }
        let env = vec![
            (
                "DREAMER_RUNTIME_DIR".to_owned(),
                platform::runtime_dir().display().to_string(),
            ),
            ("DREAMER_SESSION_ID".to_owned(), s.id.clone()),
            (
                "DREAMER_SESSION_TICKET".to_owned(),
                ticket_path.display().to_string(),
            ),
        ];
        let resumed = {
            let info = s.info.lock().unwrap_or_else(|e| e.into_inner());
            info.harness_file.is_some() || info.s.harness_session.is_some()
        };
        s.set_state(St::Starting, None);
        let spec = {
            let info = s.info.lock().unwrap_or_else(|e| e.into_inner());
            spec(&info, env)
        };
        let started = match spec {
            Ok((Protocol::Pi, mut spec)) => {
                // Without a login, pi runs with no automatic network use.
                spec.offline = pi_login_gate(&spec).await.is_err();
                providers::pi_rpc::start(&spec).await
            }
            // The others take no turn until a prompt, which is checked.
            Ok((protocol, spec)) => providers::session::start(protocol, &spec).await,
            Err(e) => Err(e),
        };
        let mut h = match started {
            Ok(h) => h,
            Err(why) => {
                let _ = std::fs::remove_file(&ticket_path);
                log::warn(format!("agent session {}: {why}", s.id));
                s.set_state(St::Detached, Some(why.clone()));
                s.persist();
                s.append(api::agent_session_event::Kind::Error(api::AgentError {
                    message: why.clone(),
                }));
                s.status(St::Detached, Some(why.clone()));
                return Err(RpcError::new(code::UNAVAILABLE, why));
            }
        };
        if let Ok(mut t) = s.ticket.lock() {
            *t = Some(ticket);
        }
        {
            let mut info = s.info.lock().unwrap_or_else(|e| e.into_inner());
            info.s.pid = Some(h.pid);
            info.s.harness_version = h.version.clone();
            info.s.state = St::Idle as i32;
            info.s.detail = None;
            if let Some((id, file)) = h.session.clone() {
                info.s.harness_session = Some(id);
                info.harness_file = file;
            }
        }
        s.persist();
        log::info(format!(
            "agent session {}: harness pid {} ({})",
            s.id,
            h.pid,
            h.version.as_deref().unwrap_or("version unknown")
        ));
        s.status(
            St::Idle,
            Some(if resumed { "resumed" } else { "started" }.to_owned()),
        );
        let updates = std::mem::replace(&mut h.updates, tokio::sync::mpsc::unbounded_channel().1);
        let pump = tokio::spawn(pump(self.clone(), s.clone(), updates, h.pid));
        *run = Some(Running {
            control: h.control,
            pump,
            ticket_path,
        });
        Ok(())
    }

    pub async fn send(
        self: &Arc<Self>,
        caller: &Caller,
        p: api::AgentSessionSend,
        memory: &crate::memories::Memory,
    ) -> Result<api::AgentSession, RpcError> {
        let s = self.get(caller, &p.session)?;
        if s.state() == St::Closed {
            return Err(RpcError::new(
                code::PRECONDITION,
                format!("agent session {} is closed; resume it first", s.id),
            ));
        }
        if p.text.trim().is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "the prompt is empty"));
        }
        // No model turn without a real login: refused before the harness is
        // started or sent anything.
        let (protocol, spec) = {
            let info = s.info.lock().unwrap_or_else(|e| e.into_inner());
            spec(&info, Vec::new())
        }
        .map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
        if let Err(why) = login_gate(protocol, &spec).await {
            log::info(format!("agent session {}: prompt refused: {why}", s.id));
            return Err(RpcError::new(code::PRECONDITION, why));
        }
        self.start(&s).await?;
        let prompt = first_prompt(&s, memory, p.text.clone()).await;
        let run = s.run.lock().await;
        let r = run
            .as_ref()
            .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "the harness is not running"))?;
        s.append(api::agent_session_event::Kind::UserMessage(
            api::AgentText {
                text: p.text.clone(),
            },
        ));
        s.set_state(St::Working, None);
        r.control
            .send(Input::Prompt(prompt))
            .map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
        drop(run);
        Ok(s.snapshot())
    }

    pub async fn cancel(
        &self,
        caller: &Caller,
        p: api::AgentSessionRef,
    ) -> Result<api::AgentSession, RpcError> {
        let s = self.get(caller, &p.session)?;
        let run = s.run.lock().await;
        let r = run.as_ref().ok_or_else(|| {
            RpcError::new(
                code::PRECONDITION,
                format!("agent session {} has no running harness", s.id),
            )
        })?;
        r.control
            .send(Input::Cancel)
            .map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
        drop(run);
        Ok(s.snapshot())
    }

    pub async fn answer(
        &self,
        caller: &Caller,
        p: api::AgentSessionAnswer,
        allow: bool,
    ) -> Result<api::AgentSession, RpcError> {
        let s = self.get(caller, &p.session)?;
        // The session itself never answers its own requests.
        if caller.instance == format!("session-{}", s.id) {
            return Err(RpcError::new(
                code::FORBIDDEN,
                "an agent session never answers its own approval requests",
            ));
        }
        let run = s.run.lock().await;
        let waiting = s
            .info
            .lock()
            .is_ok_and(|mut i| i.pending.remove(&p.approval));
        let Some(r) = run.as_ref().filter(|_| waiting) else {
            return Err(RpcError::new(
                code::NOT_FOUND,
                format!(
                    "agent session {} has no approval request {} waiting",
                    s.id, p.approval
                ),
            ));
        };
        r.control
            .send(Input::Answer {
                id: p.approval.clone(),
                allow,
                option: p.option.clone(),
            })
            .map_err(|e| RpcError::new(code::UNAVAILABLE, e))?;
        drop(run);
        s.append(api::agent_session_event::Kind::ApprovalAnswer(
            api::AgentApprovalAnswer {
                id: p.approval,
                allowed: allow,
                option: p.option,
                by: caller.instance.clone(),
            },
        ));
        let none_left = s.info.lock().is_ok_and(|i| i.pending.is_empty());
        if none_left && s.state() == St::Waiting {
            s.set_state(St::Working, None);
        }
        Ok(s.snapshot())
    }

    pub async fn resume(
        self: &Arc<Self>,
        caller: &Caller,
        p: api::AgentSessionRef,
    ) -> Result<api::AgentSession, RpcError> {
        let s = self.get(caller, &p.session)?;
        self.start(&s).await?;
        Ok(s.snapshot())
    }

    pub async fn close(
        &self,
        caller: &Caller,
        p: api::AgentSessionRef,
    ) -> Result<api::AgentSession, RpcError> {
        let s = self.get(caller, &p.session)?;
        let mut run = s.run.lock().await;
        if s.state() == St::Closed && run.is_none() {
            return Ok(s.snapshot());
        }
        s.set_state(St::Closed, None);
        s.persist();
        if let Some(r) = run.take() {
            end(r).await;
        }
        drop(run);
        if let Ok(mut t) = s.ticket.lock() {
            *t = None;
        }
        let pending: Vec<String> = s
            .info
            .lock()
            .map(|mut i| std::mem::take(&mut i.pending).into_iter().collect())
            .unwrap_or_default();
        for id in pending {
            s.append(api::agent_session_event::Kind::ApprovalAnswer(
                api::AgentApprovalAnswer {
                    id,
                    allowed: false,
                    option: None,
                    by: "dreamer".into(),
                },
            ));
        }
        s.status(St::Closed, None);
        log::info(format!(
            "agent session {}: closed by {}",
            s.id, caller.instance
        ));
        Ok(s.snapshot())
    }

    /// The daemon is stopping: end every harness. The sessions stay, detached.
    pub async fn stop_all(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let all: Vec<Arc<Session>> = self
            .map
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        for s in all {
            let r = s.run.lock().await.take();
            if let Some(r) = r {
                end(r).await;
            }
        }
    }

    /// `apps.register` with `session:<id>` and the session's ticket, from a
    /// process the harness started: the connection's grant becomes the
    /// session's. Returns the registration.
    pub async fn register(
        &self,
        launch_id: &str,
        ticket: &str,
        peer: Option<u32>,
    ) -> Result<api::Registration, RpcError> {
        let refuse = |why: &str| RpcError::new(code::FORBIDDEN, why.to_owned());
        let id = launch_id.strip_prefix("session:").unwrap_or_default();
        let s = self
            .map
            .lock()
            .ok()
            .and_then(|m| m.get(id).cloned())
            .ok_or_else(|| refuse("no such agent session"))?;
        // An agent may start its MCP servers while its session is still
        // being set up (an ACP agent during `session/new`), before the
        // harness is recorded: wait for that, briefly.
        let mut waited = Duration::ZERO;
        let (expected, pid) = loop {
            let expected = s.ticket.lock().ok().and_then(|t| t.clone());
            let (pid, starting) = s
                .info
                .lock()
                .ok()
                .map(|i| (i.s.pid, i.s.state == St::Starting as i32))
                .unwrap_or_default();
            if (expected.is_some() && pid.is_some()) || !starting || waited >= REGISTER_WAIT {
                break (expected, pid);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += Duration::from_millis(100);
        };
        let (Some(expected), Some(pid)) = (expected, pid) else {
            return Err(refuse("the agent session's harness is not running"));
        };
        if !crate::apps::same(&expected, ticket.trim()) {
            log::warn(format!(
                "agent session {id}: registration with a wrong ticket"
            ));
            return Err(refuse("wrong session ticket"));
        }
        let peer = peer.ok_or_else(|| refuse("the connecting process cannot be identified"))?;
        let inside = tokio::task::spawn_blocking(move || crate::apps::descends_from(peer, pid))
            .await
            .unwrap_or(false);
        if !inside {
            log::warn(format!(
                "agent session {id}: process {peer} is not the harness's"
            ));
            return Err(refuse(
                "the registering process was not started by the session's harness",
            ));
        }
        let grant = s
            .info
            .lock()
            .ok()
            .and_then(|i| i.s.grant.clone())
            .unwrap_or_default();
        Ok(api::Registration {
            instance: format!("session-{id}"),
            app_id: "agent-session".into(),
            pane: 0,
            grant: Some(grant),
        })
    }
}

/// The prompt the harness gets for `text`: the session's first prompt
/// starts with the memory index of its scopes (docs/memory.md); later ones
/// go as they are. The transcript keeps `text` as the user wrote it.
async fn first_prompt(s: &Session, memory: &crate::memories::Memory, text: String) -> String {
    let (first, _) = s.read(0, usize::MAX);
    if first
        .iter()
        .any(|e| matches!(e.kind, Some(api::agent_session_event::Kind::UserMessage(_))))
    {
        return text;
    }
    let cwd = s.info.lock().map(|i| i.s.cwd.clone()).unwrap_or_default();
    match memory.index(&cwd, &s.id).await {
        Some(index) => format!("{index}\n\n{text}"),
        None => text,
    }
}

/// Stop a running harness and wait for its updates to end.
async fn end(r: Running) {
    let Running {
        mut control,
        pump,
        ticket_path,
    } = r;
    control.close();
    let _ = std::fs::remove_file(&ticket_path);
    if tokio::time::timeout(CLOSE_WAIT, pump).await.is_err() {
        log::warn("an agent session's harness did not end in time");
    }
}

/// Carry a harness's updates into the session until it ends.
async fn pump(
    sessions: Arc<Sessions>,
    s: Arc<Session>,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<Update>,
    pid: u32,
) {
    let mut why = "the harness ended".to_owned();
    while let Some(u) = updates.recv().await {
        match &u {
            Update::HarnessSession { id, file } => {
                {
                    let mut info = s.info.lock().unwrap_or_else(|e| e.into_inner());
                    info.s.harness_session = Some(id.clone());
                    info.harness_file = file.clone();
                }
                s.persist();
                continue;
            }
            Update::Exited(w) => {
                why = w.clone();
                break;
            }
            Update::Approval(a) => {
                if let Ok(mut i) = s.info.lock() {
                    i.pending.insert(a.id.clone());
                }
                s.set_state(St::Waiting, None);
            }
            Update::TurnEnd(_) => {
                if let Ok(mut i) = s.info.lock() {
                    i.pending.clear();
                }
                s.set_state(St::Idle, None);
            }
            _ => {}
        }
        if let Some(kind) = to_event(u) {
            s.append(kind);
        }
    }
    // Ended by itself or by a close.
    {
        let mut info = s.info.lock().unwrap_or_else(|e| e.into_inner());
        if info.s.pid == Some(pid) {
            info.s.pid = None;
        }
        info.pending.clear();
    }
    if s.state() == St::Closed {
        return;
    }
    let detail = if sessions.stopping.load(Ordering::SeqCst) {
        "the daemon stopped".to_owned()
    } else {
        why
    };
    log::info(format!("agent session {}: detached: {detail}", s.id));
    s.set_state(St::Detached, Some(detail.clone()));
    s.status(St::Detached, Some(detail));
    // A harness that ended by itself leaves nothing running.
    if let Ok(mut run) = s.run.try_lock()
        && let Some(r) = run.take()
    {
        let _ = std::fs::remove_file(&r.ticket_path);
    }
    if let Ok(mut t) = s.ticket.lock() {
        *t = None;
    }
}

/// Clamp a query's limit.
pub fn limit(q: &api::AgentSessionEventsQuery) -> usize {
    q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_grant_never_holds_sessions_apps_approvals_or_the_owner() {
        let g = confine(api::Grant {
            owner: true,
            methods: vec![
                "terminal.read".into(),
                "agent.sessions.approve".into(),
                "agent.*".into(),
                "apps.launch".into(),
                "approvals.answer".into(),
            ],
            ..api::Grant::default()
        });
        assert!(!g.owner);
        assert_eq!(g.methods, vec!["terminal.read".to_owned()]);
        let d = default_grant();
        assert!(d.all_panes && d.level == api::ActLevel::Observe as i32);
        assert!(d.methods.iter().all(|m| !m.starts_with("agent.")));
        assert!(!d.methods.contains(&"terminal.input".to_owned()));
    }
}
