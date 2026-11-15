//! `dream agents providers` and `dream agents ask`: the agent providers
//! (spec 13.5) on the command line. A run uses the installed CLI on the
//! user's own login, never an API key, and reaches Dreamer only through
//! `dream mcp` with the grant given here.

use std::io::Write;
use std::time::Duration;

use providers::{Event, Kind, Request, State, Tools};

use crate::daemon::DaemonArgs;

pub const USAGE: &str = "usage:
  dream agents providers [--json]
  dream agents ask [--provider claude|codex] [--method M]... [--pane %N]... [--all-panes]
                   [--no-tools] [--max-turns N] [--timeout SECONDS] [--allow-metered] -- PROMPT";

fn state_text(s: &State) -> String {
    match s {
        State::Ready => "ready".into(),
        State::NotInstalled => "not installed".into(),
        State::LoggedOut => "logged out (log in with the CLI itself)".into(),
        State::MeteredLogin(why) => format!("metered: {why}"),
        State::Incompatible(why) => format!("incompatible: {why}"),
        State::Unknown(why) => format!("unknown: {why}"),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

pub fn providers(args: &[String]) -> Result<(), String> {
    let json = match args {
        [] => false,
        [j] if j == "--json" => true,
        _ => return Err(USAGE.into()),
    };
    let probes = runtime()?.block_on(async {
        let mut out = Vec::new();
        for k in providers::ORDER {
            out.push(providers::probe(k).await);
        }
        out
    });
    if json {
        let list: Vec<serde_json::Value> = probes
            .iter()
            .map(|p| {
                let c = p.kind.capabilities();
                serde_json::json!({
                    "provider": p.kind.name(),
                    "program": p.program.as_ref().map(|x| x.display().to_string()),
                    "version": p.version,
                    "tested": p.kind.tested().0,
                    "login": p.login,
                    "ready": p.state == State::Ready,
                    "state": state_text(&p.state),
                    "capabilities": {
                        "streams_text": c.streams_text, "tools": c.tools, "cancel": c.cancel,
                        "usage": c.usage, "cost": c.cost,
                    },
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?
        );
    } else {
        for p in &probes {
            println!(
                "{:<7} {:<40} {}",
                p.kind.name(),
                state_text(&p.state),
                p.version.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

pub fn ask(d: &DaemonArgs, args: &[String]) -> Result<std::process::ExitCode, String> {
    let mut kind = None;
    let mut mcp: Vec<String> = vec!["mcp".into()];
    let mut tools = true;
    let mut req = Request::default();
    let mut it = args.iter();
    let mut prompt = Vec::new();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "--provider" => {
                let v = value("claude or codex")?;
                kind = Some(Kind::parse(&v).ok_or(format!("unknown provider {v}"))?);
            }
            "--method" => mcp.extend(["--method".into(), value("a method")?]),
            "--pane" => mcp.extend(["--pane".into(), value("%N")?]),
            "--all-panes" => mcp.push("--all-panes".into()),
            "--no-tools" => tools = false,
            "--max-turns" => {
                req.max_turns = Some(
                    value("a number")?
                        .parse()
                        .map_err(|_| "--max-turns needs a number")?,
                );
            }
            "--timeout" => {
                let s: u64 = value("seconds")?
                    .parse()
                    .map_err(|_| "--timeout needs seconds")?;
                req.timeout = Some(Duration::from_secs(s));
            }
            "--allow-metered" => req.allow_metered = true,
            "--" => {
                prompt.extend(it.by_ref().cloned());
                break;
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    req.prompt = prompt.join(" ");
    if req.prompt.trim().is_empty() {
        return Err(format!("no prompt\n{USAGE}"));
    }
    // Dreamer's tools: this `dream`'s MCP server, on the same daemon.
    if tools {
        mcp.extend(d.start.iter().cloned());
        req.tools = Some(Tools {
            program: std::env::current_exe().map_err(|e| e.to_string())?,
            args: mcp,
        });
    }
    let rt = runtime()?;
    rt.block_on(async move {
        if tools {
            // The MCP server needs the daemon; start it if it is not running.
            crate::remote::session(d).await?;
        }
        let kind = match kind {
            Some(k) => k,
            None => {
                let mut ready = None;
                for k in providers::ORDER {
                    if providers::probe(k).await.state == State::Ready {
                        ready = Some(k);
                        break;
                    }
                }
                ready.ok_or("no provider is ready; see `dream agents providers`")?
            }
        };
        let mut run = providers::start(kind, &req).map_err(|e| format!("{}: {e}", kind.name()))?;
        let mut interrupted = std::pin::pin!(tokio::signal::ctrl_c());
        let mut cancelled = false;
        let mut out = std::io::stdout();
        let mut answer_streamed = false;
        loop {
            let e = tokio::select! {
                e = run.events.recv() => e,
                _ = &mut interrupted, if !cancelled => {
                    cancelled = true;
                    run.cancel();
                    continue;
                }
            };
            let Some(e) = e else {
                return Err("the run ended without a result".into());
            };
            match e {
                Event::Started { model, .. } => {
                    eprintln!(
                        "[{}{}]",
                        kind.name(),
                        model.map(|m| format!(" {m}")).unwrap_or_default()
                    );
                }
                Event::Text(t) => {
                    answer_streamed = true;
                    let _ = write!(out, "{t}");
                    let _ = out.flush();
                }
                Event::ToolCall { name, input, .. } => {
                    eprintln!("\n→ {name} {input}");
                }
                Event::ToolResult { ok, text, .. } => {
                    let first = text.lines().next().unwrap_or_default();
                    eprintln!("← {} {first}", if ok { "ok" } else { "failed:" });
                }
                Event::Limit(l) => eprintln!("[limit: {l}]"),
                Event::Usage {
                    input_tokens,
                    output_tokens,
                    cost_usd,
                } => {
                    eprintln!(
                        "\n[{input_tokens} tokens in, {output_tokens} out{}]",
                        cost_usd
                            .map(|c| format!(", ${c:.4} equivalent"))
                            .unwrap_or_default()
                    );
                }
                Event::Done(answer) => {
                    if !answer_streamed {
                        let _ = writeln!(out, "{answer}");
                    } else {
                        let _ = writeln!(out);
                    }
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                Event::Failed(why) => {
                    eprintln!("dream: {}: {why}", kind.name());
                    return Ok(std::process::ExitCode::FAILURE);
                }
            }
        }
    })
}
