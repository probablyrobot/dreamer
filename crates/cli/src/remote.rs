//! `dream ls`, `dream act`, `dream actions`, and `dream doctor`: discovery
//! and control of tmux through `dreamerd`'s typed API.

use api::{Request, Response, action::Kind};
use client::{Client, ClientError, Launch, ensure_running};

use crate::daemon::{DaemonArgs, call, explain};

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// Connect, starting the daemon with these tmux options if needed. A
/// running daemon that drives another tmux server is reported, not used.
pub(crate) async fn session(d: &DaemonArgs) -> Result<Client, String> {
    let (mut c, _) = ensure_running(&Launch::new(d.start.clone()), "cli")
        .await
        .map_err(explain)?;
    if let Some(wanted) = wanted_socket(d) {
        let Response::DaemonStatus(s) = call(&mut c, Request::DaemonStatus(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        let drives = s
            .endpoints
            .first()
            .map(|e| e.socket.clone())
            .unwrap_or_default();
        if drives != wanted {
            return Err(format!(
                "the running dreamerd drives tmux {drives}, not {wanted}; stop it with \
                 `dream shutdown` to switch"
            ));
        }
    }
    Ok(c)
}

fn wanted_socket(d: &DaemonArgs) -> Option<String> {
    d.start.chunks(2).find_map(|kv| match kv {
        [k, v] if k == "--tmux-socket" => Some(format!("-L {v}")),
        [k, v] if k == "--tmux-socket-path" => Some(format!("-S {v}")),
        _ => None,
    })
}

fn id(text: &str, sigil: char, what: &str) -> Result<u32, String> {
    text.strip_prefix(sigil)
        .unwrap_or(text)
        .parse()
        .map_err(|_| format!("`{text}` is not a {what} id (expected {sigil}N)"))
}

// ---- ls ---------------------------------------------------------------------

pub fn ls(d: &DaemonArgs, json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let Response::TmuxTopology(t) = call(
            &mut c,
            Request::TmuxTopology(api::TopologyParams::default()),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        println!("endpoint {}", t.endpoint);
        for s in &t.sessions {
            println!(
                "${} {}{}",
                s.id,
                s.name,
                if s.holding { " (hidden drawers)" } else { "" }
            );
            for wid in &s.windows {
                let Some(w) = t.windows.iter().find(|w| w.id == *wid) else {
                    continue;
                };
                let active = if s.active_window == Some(w.id) {
                    "*"
                } else {
                    ""
                };
                let note = if w.layout_supported {
                    ""
                } else {
                    " (layout not drawable yet)"
                };
                println!(
                    "  @{} {}:{}{active} {}x{}{note}",
                    w.id, w.index, w.name, w.width, w.height
                );
                for pid in &w.panes {
                    let Some(p) = t.panes.iter().find(|p| p.id == *pid) else {
                        continue;
                    };
                    println!(
                        "    %{}{} {}x{} {}{} gen {}{}{}",
                        p.id,
                        if p.active { "*" } else { "" },
                        p.width,
                        p.height,
                        p.current_command,
                        p.pid.map(|pid| format!(" pid {pid}")).unwrap_or_default(),
                        p.generation,
                        if p.dead { " (dead)" } else { "" },
                        p.drawer_of
                            .map(|s| format!(" (drawer of ${s})"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        Ok(())
    })
}

// ---- read and mirrors -------------------------------------------------------

/// `dream read %PANE [--history N] [--json]`: the pane's text from the
/// daemon's canonical mirror.
pub fn read(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let history = args
        .iter()
        .position(|a| a == "--history")
        .map(|i| {
            args.get(i + 1)
                .and_then(|n| n.parse().ok())
                .ok_or("--history needs a number")
        })
        .transpose()?;
    let target = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && (*i == 0 || args[i - 1] != "--history"))
        .map(|(_, a)| a.clone())
        .ok_or("usage: dream read %PANE [--history N] [--json]")?;
    let pane = id(&target, '%', "pane")?;
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let Response::TerminalRead(t) = call(
            &mut c,
            Request::TerminalRead(api::ReadParams {
                pane,
                history,
                endpoint: None,
            }),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?
            );
        } else {
            for l in &t.lines {
                println!("{}", l.trim_end());
            }
        }
        Ok(())
    })
}

pub fn mirrors(d: &DaemonArgs, json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let Response::TerminalMirrors(m) = call(
            &mut c,
            Request::TerminalMirrors(api::TopologyParams::default()),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        println!(
            "{} VT workers; journals {} of {} bytes (per pane {}, max age {}s)",
            m.workers,
            m.host_journal_bytes,
            m.journal_host_limit,
            m.journal_pane_limit,
            m.journal_max_age_ms / 1000
        );
        for s in &m.mirrors {
            let state =
                api::MirrorState::try_from(s.state).map_or("unknown", api::MirrorState::name);
            let tier = api::Tier::try_from(s.tier).map_or("unknown", api::Tier::name);
            println!(
                "%{} gen {} epoch {} {state} {tier} {}x{} seq {} bytes {} journal {}..={} ({} bytes) worker {}{}",
                s.pane,
                s.generation,
                s.epoch,
                s.cols,
                s.rows,
                s.sequence,
                s.ingested_bytes,
                s.journal_first_sequence,
                s.sequence,
                s.journal_bytes,
                s.worker,
                s.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
        }
        Ok(())
    })
}

/// `dream tier %PANE hot|warm|cold`.
pub fn tier(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let [target, tier] = args else {
        return Err("usage: dream tier %PANE hot|warm|cold".into());
    };
    let pane = id(target, '%', "pane")?;
    let tier = api::Tier::from_name(tier).ok_or("tier must be hot, warm, or cold")?;
    rt()?.block_on(async {
        let mut c = session(d).await?;
        call(
            &mut c,
            Request::TerminalSetTier(api::SetTier {
                pane,
                tier: tier as i32,
                endpoint: None,
            }),
        )
        .await?;
        println!("%{pane} is {}", tier.name());
        Ok(())
    })
}

// ---- act --------------------------------------------------------------------

pub const ACT_USAGE: &str = "usage: dream act <action> [--key IDEMPOTENCY] [--json] ...
  split  %PANE [-h|-v] [--percent N | --cells N] [--cwd DIR] [-- PROGRAM ARGS...]
  resize %PANE [--width N] [--height N]
  select %PANE | @WINDOW
  zoom   %PANE
  rename @WINDOW | $SESSION NAME
  send   %PANE TEXT [--enter] [--generation G]
  keys   %PANE KEY... [--generation G]      (tmux key names: Enter, C-c, Up)
  kill   %PANE | @WINDOW
exit status: 0 completed, 1 failed, 3 outcome unknown";

/// Parse `dream act` arguments into an action. Pure, for tests.
pub fn parse_act(args: &[String]) -> Result<(Kind, Option<String>, bool), String> {
    let mut key = None;
    let mut json = false;
    let mut rest: Vec<String> = Vec::new();
    let mut argv: Option<Vec<String>> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                argv = Some(it.by_ref().cloned().collect());
            }
            "--key" => key = Some(it.next().ok_or("--key needs a value")?.clone()),
            "--json" => json = true,
            _ => rest.push(a.clone()),
        }
    }
    let (verb, rest) = rest.split_first().ok_or(ACT_USAGE)?;
    let opt = |name: &str| -> Result<Option<u32>, String> {
        rest.iter()
            .position(|a| a == name)
            .map(|i| {
                rest.get(i + 1)
                    .ok_or(format!("{name} needs a number"))?
                    .parse::<u32>()
                    .map_err(|_| format!("{name} needs a number"))
            })
            .transpose()
    };
    let flag = |name: &str| rest.iter().any(|a| a == name);
    let target = rest
        .first()
        .ok_or(format!("`{verb}` needs a target\n{ACT_USAGE}"))?;
    let generation = rest
        .iter()
        .position(|a| a == "--generation")
        .map(|i| {
            rest.get(i + 1)
                .and_then(|g| g.parse::<u64>().ok())
                .ok_or("--generation needs a number")
        })
        .transpose()?;
    let kind = match verb.as_str() {
        "split" => Kind::SplitPane(api::SplitPane {
            pane: id(target, '%', "pane")?,
            direction: if flag("-h") {
                api::SplitDirection::Horizontal as i32
            } else {
                api::SplitDirection::Vertical as i32
            },
            percent: opt("--percent")?,
            cells: opt("--cells")?,
            argv: argv.unwrap_or_default(),
            cwd: rest
                .iter()
                .position(|a| a == "--cwd")
                .map(|i| rest.get(i + 1).cloned().ok_or("--cwd needs a directory"))
                .transpose()?,
        }),
        "resize" => Kind::ResizePane(api::ResizePane {
            pane: id(target, '%', "pane")?,
            width: opt("--width")?,
            height: opt("--height")?,
        }),
        "select" if target.starts_with('@') => Kind::SelectWindow(api::SelectWindow {
            window: id(target, '@', "window")?,
        }),
        "select" => Kind::SelectPane(api::SelectPane {
            pane: id(target, '%', "pane")?,
        }),
        "zoom" => Kind::ZoomPane(api::ZoomPane {
            pane: id(target, '%', "pane")?,
        }),
        "rename" => {
            let name = rest.get(1).ok_or("rename needs a name")?.clone();
            if target.starts_with('$') {
                Kind::RenameSession(api::RenameSession {
                    session: id(target, '$', "session")?,
                    name,
                })
            } else {
                Kind::RenameWindow(api::RenameWindow {
                    window: id(target, '@', "window")?,
                    name,
                })
            }
        }
        "send" => {
            let mut data = rest.get(1).ok_or("send needs text")?.clone().into_bytes();
            if flag("--enter") {
                data.push(b'\r');
            }
            Kind::SendInput(api::SendInput {
                pane: id(target, '%', "pane")?,
                data,
                generation,
                lease: None,
            })
        }
        "keys" => {
            let mut keys = Vec::new();
            let mut skip = false;
            for a in &rest[1..] {
                if skip {
                    skip = false;
                } else if a == "--generation" {
                    skip = true;
                } else {
                    keys.push(a.clone());
                }
            }
            Kind::SendKeys(api::SendKeys {
                pane: id(target, '%', "pane")?,
                keys,
                generation,
                lease: None,
            })
        }
        "kill" if target.starts_with('@') => Kind::KillWindow(api::KillWindow {
            window: id(target, '@', "window")?,
        }),
        "kill" => Kind::KillPane(api::KillPane {
            pane: id(target, '%', "pane")?,
        }),
        other => return Err(format!("unknown action `{other}`\n{ACT_USAGE}")),
    };
    Ok((kind, key, json))
}

fn print_record(r: &api::ActionRecord) {
    let state = api::ActionState::try_from(r.state).map_or("unknown", api::ActionState::name);
    let detail = r
        .observed
        .as_deref()
        .or(r.reason.as_deref())
        .unwrap_or_default();
    println!("{} {state}: {detail}", r.id);
}

/// Returns the process exit status: 0 completed, 1 failed, 3 unknown.
pub fn act(d: &DaemonArgs, args: &[String]) -> Result<u8, String> {
    let (kind, key, json) = parse_act(args)?;
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let req = api::SubmitAction {
            action: Some(api::Action { kind: Some(kind) }),
            idempotency_key: key,
            ..api::SubmitAction::default()
        };
        let Response::ActionsSubmit(r) = call(&mut c, Request::ActionsSubmit(req)).await? else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?
            );
        } else {
            print_record(&r);
        }
        Ok(match api::ActionState::try_from(r.state) {
            Ok(api::ActionState::Completed) => 0,
            Ok(api::ActionState::OutcomeUnknown) => 3,
            _ => 1,
        })
    })
}

// ---- actions ----------------------------------------------------------------

pub fn actions(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let limit = args
        .iter()
        .position(|a| a == "--limit")
        .map(|i| {
            args.get(i + 1)
                .and_then(|n| n.parse().ok())
                .ok_or("--limit needs a number")
        })
        .transpose()?;
    let id = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && (*i == 0 || args[i - 1] != "--limit"))
        .map(|(_, a)| a.clone());
    rt()?.block_on(async {
        let mut c = crate::daemon::connect().await?;
        let records = match id {
            Some(id) => match call(&mut c, Request::ActionsGet(api::ActionQuery { id })).await? {
                Response::ActionsGet(r) => vec![r],
                _ => return Err("unexpected reply".into()),
            },
            None => match call(
                &mut c,
                Request::ActionsList(api::ActionListParams { limit }),
            )
            .await?
            {
                Response::ActionsList(l) => l.actions,
                _ => return Err("unexpected reply".into()),
            },
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?
            );
        } else {
            for r in &records {
                print_record(r);
            }
        }
        Ok(())
    })
}

// ---- doctor -----------------------------------------------------------------

/// Diagnostics that work whether or not the daemon runs. Never starts one.
pub fn doctor() -> Result<(), String> {
    println!(
        "dream {}  build family {}",
        env!("CARGO_PKG_VERSION"),
        terminal_core::family::build_family()
    );
    for (label, dir) in [
        ("runtime", platform::runtime_dir()),
        ("state", platform::state_dir()),
    ] {
        let note = match std::fs::metadata(&dir) {
            Ok(m) => {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let mode = m.permissions().mode() & 0o777;
                let ok = m.uid() == platform::uid() && mode & 0o077 == 0;
                format!(
                    "mode {mode:o}{}",
                    if ok {
                        ""
                    } else {
                        "  PROBLEM: must be 0700 and yours"
                    }
                )
            }
            Err(_) => "absent (created when the daemon starts)".into(),
        };
        println!("{label:8} {}  {note}", dir.display());
    }
    let socket = platform::socket_path();
    println!(
        "socket   {}  {}",
        socket.display(),
        if socket.exists() { "present" } else { "absent" }
    );
    match platform::lock_holder() {
        Some(pid) if platform::process_alive(pid) => println!("lock     held by pid {pid}"),
        Some(pid) => println!("lock     stale (pid {pid} is gone)"),
        None => println!("lock     free"),
    }
    let ledger = platform::ledger_path();
    match workspace_host::ledger::read_only(&ledger) {
        Ok(records) => {
            let mut counts = std::collections::BTreeMap::new();
            for r in &records {
                *counts
                    .entry(workspace_host::ledger::state_name(r.state))
                    .or_insert(0) += 1;
            }
            let summary: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
            println!(
                "ledger   {}  {} actions ({})",
                ledger.display(),
                records.len(),
                summary.join(", ")
            );
        }
        Err(_) => println!("ledger   {}  absent", ledger.display()),
    }
    rt()?.block_on(async {
        match Client::connect(&socket, "cli").await {
            Ok(mut c) => {
                println!(
                    "daemon   running, pid {}, instance {}",
                    c.welcome.pid, c.welcome.instance
                );
                if let Ok(Response::DaemonStatus(s)) =
                    call(&mut c, Request::DaemonStatus(api::Empty {})).await
                {
                    for e in &s.endpoints {
                        let state = match api::LinkState::try_from(e.state) {
                            Ok(api::LinkState::Unspecified) => "not connected",
                            Ok(s) => s.name(),
                            Err(_) => "unknown",
                        };
                        println!(
                            "tmux     {} ({}) {state}{}",
                            e.name,
                            e.socket,
                            e.tmux_version
                                .as_deref()
                                .map(|v| format!(", {v}"))
                                .unwrap_or_default()
                        );
                    }
                }
            }
            Err(ClientError::NotRunning(_)) => println!("daemon   not running"),
            Err(e) => println!("daemon   unreachable: {e}"),
        }
        Ok(())
    })
}

/// `dream extensions [--json]` and `dream extensions restart NAME`.
pub fn extensions(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        if let [verb, name] = args
            && verb == "restart"
        {
            let Response::ExtensionsRestart(s) = call(
                &mut c,
                Request::ExtensionsRestart(api::ExtensionRef { name: name.clone() }),
            )
            .await?
            else {
                return Err("unexpected reply".into());
            };
            println!("restarting {}", s.name);
            return Ok(());
        }
        let json = match args {
            [] => false,
            [j] if j == "--json" => true,
            _ => {
                return Err(
                    "usage: dream extensions [--json] | dream extensions restart NAME".into(),
                );
            }
        };
        let Response::ExtensionsList(l) =
            call(&mut c, Request::ExtensionsList(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        if let Some(e) = &l.config_error {
            println!("extensions.yaml: {e}");
        }
        if l.extensions.is_empty() {
            println!("no extensions (see docs/extensions.md)");
        }
        for x in &l.extensions {
            let state =
                api::ExtensionState::try_from(x.state).map_or("unknown", api::ExtensionState::name);
            let pid = x.pid.map(|p| format!(" pid {p}")).unwrap_or_default();
            let version = x
                .version
                .as_deref()
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            println!(
                "{} {state}{pid}{version}, restarts {}, failures {}",
                x.name, x.restarts, x.failures
            );
            if let Some(e) = &x.last_error {
                println!("  last error: {e}");
            }
            println!("  log: {}", x.log);
        }
        Ok(())
    })
}

/// `dream scripts ...`: list, check, load, reload, and stop scripts.
pub fn scripts(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage:
  dream scripts [--json]
  dream scripts check FILE
  dream scripts load NAME FILE [--method M]... [--pane %N]... [--all-panes]
                    [--event KIND]... [--call-ms N]
  dream scripts reload NAME FILE
  dream scripts stop NAME";
    let read = |path: &str| std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    let request = match args {
        [] => Request::ScriptsList(api::Empty {}),
        [j] if j == "--json" => Request::ScriptsList(api::Empty {}),
        [v, file] if v == "check" => Request::ScriptsValidate(api::ScriptSource {
            name: file.clone(),
            source: read(file)?,
        }),
        [v, name, file] if v == "reload" => Request::ScriptsReload(api::ScriptSource {
            name: name.clone(),
            source: read(file)?,
        }),
        [v, name] if v == "stop" => Request::ScriptsStop(api::ScriptRef { name: name.clone() }),
        [v, name, file, rest @ ..] if v == "load" => {
            let mut grant = api::ScriptGrant::default();
            let mut events = Vec::new();
            let mut limits = api::ScriptLimits::default();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let mut value = || {
                    it.next()
                        .cloned()
                        .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
                };
                match a.as_str() {
                    "--method" => grant.methods.push(value()?),
                    "--pane" => grant.panes.push(
                        value()?
                            .trim_start_matches('%')
                            .parse()
                            .map_err(|_| "--pane takes %N".to_owned())?,
                    ),
                    "--all-panes" => grant.all_panes = true,
                    "--event" => events.push(value()?),
                    "--call-ms" => {
                        limits.call_ms = Some(
                            value()?
                                .parse()
                                .map_err(|_| "--call-ms takes a number".to_owned())?,
                        );
                    }
                    other => return Err(format!("unknown option {other}\n{USAGE}")),
                }
            }
            Request::ScriptsLoad(api::ScriptLoad {
                name: name.clone(),
                source: read(file)?,
                grant: Some(grant),
                events,
                limits: Some(limits),
            })
        }
        _ => return Err(USAGE.into()),
    };
    let json = args.first().is_some_and(|a| a == "--json");
    rt()?.block_on(async {
        let mut c = session(d).await?;
        match call(&mut c, request).await? {
            Response::ScriptsList(l) if json => println!(
                "{}",
                serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
            ),
            Response::ScriptsList(l) => {
                if l.scripts.is_empty() {
                    println!("no scripts loaded");
                }
                for s in &l.scripts {
                    print_script(s);
                }
            }
            Response::ScriptsValidate(c) => match c.error {
                None => println!("ok"),
                Some(e) => return Err(e),
            },
            Response::ScriptsLoad(s) | Response::ScriptsReload(s) | Response::ScriptsStop(s) => {
                print_script(&s);
            }
            _ => return Err("unexpected reply".into()),
        }
        Ok(())
    })
}

fn print_script(s: &api::ScriptStatus) {
    let state = api::ScriptState::try_from(s.state).map_or("unknown", api::ScriptState::name);
    println!(
        "{} {state} revision {}, handled {}, errors {}, restarts {}, failures {}",
        s.name, s.revision, s.handled, s.errors, s.restarts, s.failures
    );
    if let Some(e) = &s.last_error {
        println!("  last error: {e}");
    }
}

/// `dream jobs ...`: list, start, cancel, and show artifacts of jobs.
pub fn jobs(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage:
  dream jobs [--json]
  dream jobs start [--cwd DIR] [--name NAME] -- PROGRAM [ARGS...]
  dream jobs cancel ID
  dream jobs artifacts ID";
    let request = match args {
        [] => Request::JobsList(api::Empty {}),
        [j] if j == "--json" => Request::JobsList(api::Empty {}),
        [v, id] if v == "cancel" => Request::JobsCancel(api::JobRef { id: id.clone() }),
        [v, id] if v == "artifacts" => Request::JobsArtifacts(api::JobRef { id: id.clone() }),
        [v, rest @ ..] if v == "start" => {
            let split = rest.iter().position(|a| a == "--").ok_or(USAGE)?;
            let (opts, argv) = (&rest[..split], &rest[split + 1..]);
            let mut start = api::JobStart {
                argv: argv.to_vec(),
                ..api::JobStart::default()
            };
            let mut it = opts.iter();
            while let Some(a) = it.next() {
                let value = it.next().cloned().ok_or(USAGE)?;
                match a.as_str() {
                    "--cwd" => start.cwd = Some(value),
                    "--name" => start.name = Some(value),
                    _ => return Err(USAGE.into()),
                }
            }
            Request::JobsStart(start)
        }
        _ => return Err(USAGE.into()),
    };
    let json = args.first().is_some_and(|a| a == "--json");
    rt()?.block_on(async {
        let mut c = session(d).await?;
        match call(&mut c, request).await? {
            Response::JobsList(l) if json => println!(
                "{}",
                serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
            ),
            Response::JobsList(l) => {
                if l.jobs.is_empty() {
                    println!("no jobs");
                }
                for j in &l.jobs {
                    print_job(j);
                }
            }
            Response::JobsStart(j) | Response::JobsCancel(j) => print_job(&j),
            Response::JobsArtifacts(a) => {
                println!("{}", a.directory);
                for f in &a.files {
                    println!("  {} ({} bytes)", f.path, f.bytes);
                }
            }
            _ => return Err("unexpected reply".into()),
        }
        Ok(())
    })
}

/// `dream approvals`: actions agents wait to have approved, and answers.
pub fn approvals(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage:
  dream approvals [--json]
  dream approvals allow ID [--pane]
  dream approvals deny ID";
    let answer = |id: &String, decision: api::ApprovalDecision| {
        Request::ApprovalsAnswer(api::ApprovalAnswer {
            id: id.clone(),
            decision: decision as i32,
        })
    };
    let request = match args {
        [] => Request::ApprovalsList(api::Empty {}),
        [j] if j == "--json" => Request::ApprovalsList(api::Empty {}),
        [v, id] if v == "allow" => answer(id, api::ApprovalDecision::Once),
        [v, id, p] if v == "allow" && p == "--pane" => answer(id, api::ApprovalDecision::Pane),
        [v, id] if v == "deny" => answer(id, api::ApprovalDecision::Deny),
        _ => return Err(USAGE.into()),
    };
    let json = args.first().is_some_and(|a| a == "--json");
    rt()?.block_on(async {
        let mut c = session(d).await?;
        match call(&mut c, request).await? {
            Response::ApprovalsList(l) if json => println!(
                "{}",
                serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
            ),
            Response::ApprovalsList(l) => {
                if l.approvals.is_empty() {
                    println!("nothing waits for approval");
                }
                for a in &l.approvals {
                    println!(
                        "{} {}: {}{}",
                        a.id,
                        a.requester,
                        a.action,
                        if a.closing { " (closes)" } else { "" }
                    );
                }
            }
            Response::ApprovalsAnswer(a) => println!("{} answered: {}", a.id, a.action),
            _ => return Err("unexpected reply".into()),
        }
        Ok(())
    })
}

fn print_job(j: &api::JobRecord) {
    let state = api::JobState::try_from(j.state).map_or("unknown", api::JobState::name);
    let how = match (j.exit_code, j.signal) {
        (Some(c), _) => format!(" (exit {c})"),
        (None, Some(s)) => format!(" (signal {s})"),
        _ => String::new(),
    };
    println!(
        "{} {state}{how} pane %{}: {}",
        j.id,
        j.pane,
        j.argv.join(" ")
    );
}

// ---- web --------------------------------------------------------------------

/// `dream web open URL`: a web page in a new tmux pane, which Dreamer's
/// desktop draws the page in. Without `--session` or `--pane`, the pane this
/// runs in (`TMUX_PANE`) is split.
pub fn web(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: dream web open URL [--session $N|NAME | --pane %N] [--json]";
    let [verb, url, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    if verb != "open" {
        return Err(USAGE.into());
    }
    let here = std::env::var("TMUX_PANE").ok();
    match page_args(url.clone(), rest, here.as_deref(), false)
        .map_err(|e| format!("{e}\n{USAGE}"))?
    {
        Page::Open(req, name, json) => open_page(d, req, name, json),
        Page::Navigate(..) => Err(USAGE.into()),
    }
}

/// What `dream web open` or `dream browser` asks for.
#[derive(Debug, PartialEq)]
enum Page {
    /// A page in a new pane: the request, a session named rather than
    /// numbered, and `--json`.
    Open(api::WebOpen, Option<String>, bool),
    /// Another address in an existing web pane, and `--json`.
    Navigate(api::WebNavigate, bool),
}

/// The options after the URL: `--session` (a new window there), `--pane`
/// (split it, or with `navigate`, load the page in that web pane), and
/// `--json`. Neither session nor pane: the pane this runs in (`here`, from
/// `TMUX_PANE`) is split.
fn page_args(
    url: String,
    rest: &[String],
    here: Option<&str>,
    navigate: bool,
) -> Result<Page, String> {
    let mut req = api::WebOpen {
        url,
        ..api::WebOpen::default()
    };
    let mut session_name = None;
    let mut json = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--session" => {
                let v = value()?;
                match id(v, '$', "session") {
                    Ok(n) => req.session = Some(n),
                    Err(_) => session_name = Some(v.clone()),
                }
            }
            "--pane" => req.pane = Some(id(value()?, '%', "pane")?),
            "--json" => json = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if navigate && let Some(pane) = req.pane {
        if req.session.is_some() || session_name.is_some() {
            return Err("give --pane (a web pane to load it in) or --session, not both".into());
        }
        return Ok(Page::Navigate(
            api::WebNavigate {
                pane,
                url: req.url,
                ..api::WebNavigate::default()
            },
            json,
        ));
    }
    if req.session.is_none() && session_name.is_none() && req.pane.is_none() {
        let here =
            here.ok_or("not inside tmux: name a session with --session or a pane with --pane")?;
        req.pane = Some(id(here, '%', "pane")?);
    }
    Ok(Page::Open(req, session_name, json))
}

/// Open a page in a new pane and print it (`%N`).
fn open_page(
    d: &DaemonArgs,
    mut req: api::WebOpen,
    session_name: Option<String>,
    json: bool,
) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        if let Some(name) = session_name {
            let Response::TmuxTopology(t) = call(
                &mut c,
                Request::TmuxTopology(api::TopologyParams::default()),
            )
            .await?
            else {
                return Err("unexpected reply".into());
            };
            req.session = Some(
                t.sessions
                    .iter()
                    .find(|s| s.name == name && !s.holding)
                    .ok_or_else(|| format!("no tmux session named `{name}`"))?
                    .id,
            );
        }
        let Response::WebOpen(opened) = call(&mut c, Request::WebOpen(req)).await? else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&opened).map_err(|e| e.to_string())?
            );
        } else {
            println!("%{}", opened.pane);
        }
        Ok(())
    })
}

/// `dream browser ADDRESS [--pane %N | --session $N|NAME] [--json]`: the
/// address, typed as in the address bar (`example.com`, `localhost:3000`),
/// in a new browser pane beside this one, or with `--pane` in that web
/// pane. `dream browser clear`: empty the browser's profile.
pub fn browser(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: dream browser ADDRESS [--pane %N | --session $N|NAME] [--json] | dream browser clear";
    match args {
        [verb] if verb == "clear" => clear_browser(),
        [address, rest @ ..] if !address.starts_with("--") => {
            // The daemon makes the address a URL, as the address bar does.
            let here = std::env::var("TMUX_PANE").ok();
            match page_args(address.clone(), rest, here.as_deref(), true)
                .map_err(|e| format!("{e}\n{USAGE}"))?
            {
                Page::Open(req, name, json) => open_page(d, req, name, json),
                Page::Navigate(req, json) => {
                    let pane = req.pane;
                    rt()?.block_on(async {
                        let mut c = session(d).await?;
                        call(&mut c, Request::WebNavigate(req)).await?;
                        if json {
                            println!("{}", serde_json::json!({ "pane": pane }));
                        } else {
                            println!("%{pane}");
                        }
                        Ok(())
                    })
                }
            }
        }
        _ => Err(USAGE.into()),
    }
}

/// `dream browser clear`, on this machine, where the desktop keeps the
/// profile.
fn clear_browser() -> Result<(), String> {
    use workspace_config::browser::{Cleared, clear, web_dir};
    let web = web_dir().ok_or("no state directory (HOME is not set)")?;
    match clear(&web, platform::process_alive)? {
        Cleared::Now => {
            println!("dream: the browser's profile is empty: cookies, logins, storage, and cache")
        }
        Cleared::Later(pid) => println!(
            "dream: Dreamer's desktop (process {pid}) has the browser's profile open; it is emptied when the desktop next starts its web panes"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod page_tests {
    use super::{Page, page_args};

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn browser_arguments_open_beside_this_pane_or_load_into_a_web_pane() {
        let url = || "https://example.com".to_owned();
        // Inside tmux, nothing named: split the pane it runs in.
        let Ok(Page::Open(req, None, false)) = page_args(url(), &[], Some("%7"), true) else {
            panic!("open beside this pane");
        };
        assert_eq!((req.pane, req.session), (Some(7), None));
        // `--pane` loads the page in that web pane, for the browser; for
        // `dream web open` it is the pane to split.
        assert_eq!(
            page_args(url(), &args(&["--pane", "%3", "--json"]), None, true),
            Ok(Page::Navigate(
                api::WebNavigate {
                    pane: 3,
                    url: url(),
                    ..api::WebNavigate::default()
                },
                true
            ))
        );
        let Ok(Page::Open(req, None, false)) =
            page_args(url(), &args(&["--pane", "%3"]), None, false)
        else {
            panic!("web open splits");
        };
        assert_eq!(req.pane, Some(3));
        // A session by name or number: a new window there.
        let Ok(Page::Open(req, Some(name), _)) =
            page_args(url(), &args(&["--session", "work"]), None, true)
        else {
            panic!("a named session");
        };
        assert_eq!((req.pane, name.as_str()), (None, "work"));
        let Ok(Page::Open(req, None, _)) =
            page_args(url(), &args(&["--session", "$2"]), None, true)
        else {
            panic!("a session id");
        };
        assert_eq!(req.session, Some(2));
        // Refused: outside tmux with nowhere named, both a pane and a
        // session, a bad pane, a missing value, an unknown option.
        for (a, here) in [
            (args(&[]), None),
            (args(&["--pane", "%3", "--session", "$1"]), None),
            (args(&["--pane", "three"]), Some("%1")),
            (args(&["--pane"]), Some("%1")),
            (args(&["--new-tab"]), Some("%1")),
        ] {
            assert!(page_args(url(), &a, here, true).is_err(), "{a:?}");
        }
    }
}

// ---- drawer -----------------------------------------------------------------

/// `dream drawer toggle|show|hide|status|new`: a tmux session's harness
/// drawer. Without `--session`, `--window`, or `--pane`, the pane this
/// runs in (`TMUX_PANE`) names the session and window.
pub fn drawer(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage:
  dream drawer toggle|show|hide|status [--session $N|NAME] [--window @N] [--pane %N] [--json]
  dream drawer new --yes [--session $N|NAME] [--window @N] [--pane %N] [--json]";
    let Some((verb, rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    let want = match verb.as_str() {
        "toggle" => Some(api::DrawerWant::Toggle),
        "show" => Some(api::DrawerWant::Show),
        "hide" => Some(api::DrawerWant::Hide),
        "new" => Some(api::DrawerWant::New),
        "status" => None,
        _ => return Err(USAGE.into()),
    };
    let mut req = api::DrawerRequest::default();
    let mut session_name = None;
    let (mut yes, mut json) = (false, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--session" => {
                let v = value()?;
                match id(v, '$', "session") {
                    Ok(n) => req.session = Some(n),
                    Err(_) => session_name = Some(v.clone()),
                }
            }
            "--window" => req.window = Some(id(value()?, '@', "window")?),
            "--pane" => req.pane = Some(id(value()?, '%', "pane")?),
            "--yes" => yes = true,
            "--json" => json = true,
            other => return Err(format!("unknown option `{other}`\n{USAGE}")),
        }
    }
    if req.session.is_none() && session_name.is_none() && req.window.is_none() && req.pane.is_none()
    {
        let here = std::env::var("TMUX_PANE").map_err(|_| {
            format!("not inside tmux: name the session with --session $N or NAME\n{USAGE}")
        })?;
        req.pane = Some(id(&here, '%', "pane")?);
    }
    rt()?.block_on(async {
        let mut c = session(d).await?;
        if let Some(name) = session_name {
            let Response::TmuxTopology(t) = call(
                &mut c,
                Request::TmuxTopology(api::TopologyParams::default()),
            )
            .await?
            else {
                return Err("unexpected reply".into());
            };
            req.session = Some(
                t.sessions
                    .iter()
                    .find(|s| s.name == name && !s.holding)
                    .ok_or_else(|| format!("no tmux session named `{name}`"))?
                    .id,
            );
        }
        if want == Some(api::DrawerWant::New) && !yes {
            let Response::DrawerStatus(s) =
                call(&mut c, Request::DrawerStatus(req.clone())).await?
            else {
                return Err("unexpected reply".into());
            };
            if let Some(p) = s.pane {
                return Err(format!(
                    "session ${}'s drawer is pane %{p}, running {}; `dream drawer new --yes` \
                     closes it and starts a fresh harness",
                    s.session,
                    if s.harness.is_empty() {
                        "its harness"
                    } else {
                        &s.harness
                    }
                ));
            }
        }
        let request = match want {
            Some(w) => Request::DrawerToggle(api::DrawerRequest {
                want: w as i32,
                ..req
            }),
            None => Request::DrawerStatus(req),
        };
        let s = match call(&mut c, request).await? {
            Response::DrawerToggle(s) | Response::DrawerStatus(s) => s,
            _ => return Err("unexpected reply".into()),
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        let Some(p) = s.pane else {
            println!("session ${} has no drawer yet", s.session);
            return Ok(());
        };
        let place = if s.shown {
            format!(
                "shown in @{}{}{}",
                s.window.unwrap_or_default(),
                if s.focused { ", focused" } else { "" },
                if s.zoomed { ", zoomed" } else { "" }
            )
        } else {
            "hidden".to_owned()
        };
        let harness = if s.harness.is_empty() {
            String::new()
        } else {
            format!(" ({})", s.harness)
        };
        let from = s
            .from_pane
            .map(|f| format!(", came from %{f}"))
            .unwrap_or_default();
        println!("session ${} drawer %{p}{harness}: {place}{from}", s.session);
        if s.created && !s.command.is_empty() {
            println!("started: {}", s.command.join(" "));
        }
        if let Some(why) = &s.problem {
            println!("the harness did not start: {why}");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[&str]) -> Kind {
        parse_act(&s.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
            .expect("parse")
            .0
    }

    #[test]
    fn act_arguments_become_typed_actions() {
        assert_eq!(
            parse(&[
                "split",
                "%3",
                "-h",
                "--percent",
                "30",
                "--",
                "htop",
                "-d",
                "5"
            ]),
            Kind::SplitPane(api::SplitPane {
                pane: 3,
                direction: api::SplitDirection::Horizontal as i32,
                percent: Some(30),
                argv: vec!["htop".into(), "-d".into(), "5".into()],
                ..api::SplitPane::default()
            })
        );
        assert_eq!(
            parse(&["select", "@2"]),
            Kind::SelectWindow(api::SelectWindow { window: 2 })
        );
        assert_eq!(
            parse(&["kill", "%4"]),
            Kind::KillPane(api::KillPane { pane: 4 })
        );
        assert_eq!(
            parse(&["rename", "$1", "work"]),
            Kind::RenameSession(api::RenameSession {
                session: 1,
                name: "work".into()
            })
        );
        assert_eq!(
            parse(&["send", "%0", "ls -la", "--enter", "--generation", "2"]),
            Kind::SendInput(api::SendInput {
                pane: 0,
                data: b"ls -la\r".to_vec(),
                generation: Some(2),
                lease: None,
            })
        );
        assert_eq!(
            parse(&["keys", "%0", "C-c", "--generation", "5", "Enter"]),
            Kind::SendKeys(api::SendKeys {
                pane: 0,
                keys: vec!["C-c".into(), "Enter".into()],
                generation: Some(5),
                lease: None,
            })
        );
        let err =
            |s: &[&str]| parse_act(&s.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>()).is_err();
        assert!(err(&["split", "x3"]));
        assert!(err(&["resize", "%1", "--width"]));
        assert!(err(&["frobnicate", "%1"]));
    }
}
