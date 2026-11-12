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
async fn session(d: &DaemonArgs) -> Result<Client, String> {
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
            println!("${} {}", s.id, s.name);
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
                        "    %{}{} {}x{} {}{} gen {}{}",
                        p.id,
                        if p.active { "*" } else { "" },
                        p.width,
                        p.height,
                        p.current_command,
                        p.pid.map(|pid| format!(" pid {pid}")).unwrap_or_default(),
                        p.generation,
                        if p.dead { " (dead)" } else { "" }
                    );
                }
            }
        }
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
                generation: Some(2)
            })
        );
        assert_eq!(
            parse(&["keys", "%0", "C-c", "--generation", "5", "Enter"]),
            Kind::SendKeys(api::SendKeys {
                pane: 0,
                keys: vec!["C-c".into(), "Enter".into()],
                generation: Some(5)
            })
        );
        let err =
            |s: &[&str]| parse_act(&s.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>()).is_err();
        assert!(err(&["split", "x3"]));
        assert!(err(&["resize", "%1", "--width"]));
        assert!(err(&["frobnicate", "%1"]));
    }
}
