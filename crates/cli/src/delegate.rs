//! `dream agents delegate`: hand one task to Claude Code or Codex, run
//! headless on the CLI's own login, and report its answer.
//!
//! This is what pi's `claude_code` and `codex` tools run, through
//! `terminal.run`, in a pane the agent owns: the pane shows the run as it
//! goes, and the result is written as JSON for the tool to read. The CLI
//! flags, the API key scrub, and the stop on a metered key are the agent
//! providers' (see `providers`); a CLI that is missing or logged out is
//! reported, never replaced by an API key.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use providers::{Event, Kind, Request, State};
use serde_json::{Value, json};

pub const USAGE: &str = "usage:
  dream agents delegate --to claude|codex [--cwd DIR] [--timeout SECONDS] [--json] [--result FILE] -- PROMPT";

/// How long a delegated run may take unless `--timeout` says otherwise.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The result file's name in a job's artifacts directory.
pub const RESULT_FILE: &str = "delegate.json";

struct Args {
    kind: Kind,
    cwd: Option<PathBuf>,
    timeout: Duration,
    json: bool,
    result: Option<PathBuf>,
    prompt: String,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut kind = None;
    let mut cwd = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut json = false;
    let mut result = None;
    let mut prompt = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "--to" => {
                let v = value("claude or codex")?;
                kind = Some(Kind::parse(&v).ok_or(format!("--to {v}: use claude or codex"))?);
            }
            "--cwd" => cwd = Some(PathBuf::from(value("a directory")?)),
            "--timeout" => {
                let s: u64 = value("seconds")?
                    .parse()
                    .map_err(|_| "--timeout needs whole seconds")?;
                if s == 0 {
                    return Err("--timeout must be at least 1 second".into());
                }
                timeout = Duration::from_secs(s);
            }
            "--json" => json = true,
            "--result" => result = Some(PathBuf::from(value("a file")?)),
            "--" => {
                prompt.extend(it.by_ref().cloned());
                break;
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    let kind = kind.ok_or(format!("--to is required\n{USAGE}"))?;
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        return Err(format!("no prompt\n{USAGE}"));
    }
    Ok(Args {
        kind,
        cwd,
        timeout,
        json,
        result,
        prompt,
    })
}

/// The tool use of a run, counted by tool.
#[derive(Default)]
struct ToolUse {
    /// Tool name and how many calls, in first-use order.
    counts: Vec<(String, u64)>,
    calls: u64,
    failed: u64,
}

impl ToolUse {
    fn called(&mut self, name: &str) {
        self.calls += 1;
        match self.counts.iter_mut().find(|(n, _)| n == name) {
            Some((_, c)) => *c += 1,
            None => self.counts.push((name.to_owned(), 1)),
        }
    }

    /// "Read ×2, Grep (1 failed)", or "none".
    fn summary(&self) -> String {
        if self.calls == 0 {
            return "none".into();
        }
        let list: Vec<String> = self
            .counts
            .iter()
            .map(|(n, c)| {
                if *c > 1 {
                    format!("{n} ×{c}")
                } else {
                    n.clone()
                }
            })
            .collect();
        let failed = if self.failed > 0 {
            format!(" ({} failed)", self.failed)
        } else {
            String::new()
        };
        format!("{}{failed}", list.join(", "))
    }
}

/// The JSON result: `ok` with `answer`, or `error` (and the CLI's `state`
/// when it was not ready to run).
fn outcome(
    kind: Kind,
    answer: Result<String, String>,
    state: Option<&str>,
    model: Option<String>,
    tools: &ToolUse,
    usage: Option<Value>,
) -> Value {
    let mut v = json!({
        "delegate": kind.name(),
        "ok": answer.is_ok(),
        "billed_to": kind.billed_to(),
        "model": model,
        "tools": {
            "calls": tools.calls,
            "failed": tools.failed,
            "summary": tools.summary(),
        },
        "usage": usage,
    });
    match answer {
        Ok(a) => v["answer"] = Value::String(a),
        Err(e) => v["error"] = Value::String(e),
    }
    if let Some(s) = state {
        v["state"] = Value::String(s.to_owned());
    }
    v
}

/// Write `v` to `path` whole: a temporary file, then a rename.
fn write_result(path: &Path, v: &Value) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, format!("{text}\n"))
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The CLI to run: `agents.NAME.command`'s program when set, else the
/// override variable or `PATH`.
fn program(kind: Kind, h: Option<&workspace_config::agents::Harness>) -> Option<PathBuf> {
    match h.and_then(|h| h.command.as_ref()) {
        Some(argv) => crate::harness::resolve(&argv[0]),
        None => providers::locate(kind),
    }
}

pub fn delegate(args: &[String]) -> Result<ExitCode, String> {
    let a = parse(args)?;
    let settings = crate::harness::agent_settings()?;
    let name = a.kind.name();
    if !settings.pi_delegates().iter().any(|d| d == name) {
        return Err(format!(
            "delegating to {name} is turned off (agents.pi.delegates in dreamer.yaml)"
        ));
    }
    let h = settings.harness(name);
    let program = program(a.kind, h.as_ref());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut tools = ToolUse::default();
    let mut model = None;
    let mut usage = None;
    let (answer, state) = rt.block_on(async {
        // Ready means installed, a version the adapter reads, and logged
        // in on the CLI's own account (not an API key).
        let probe = providers::probe_program(a.kind, program.clone()).await;
        if probe.state != State::Ready {
            let s = crate::ask::state_text(&probe.state);
            let why = match &probe.state {
                State::NotInstalled => format!("{name} is not installed (not found on PATH)"),
                State::LoggedOut => format!(
                    "{name} is not logged in; log in with `{name}` itself (Dreamer never uses an API key instead)"
                ),
                _ => format!("{name} cannot run: {s}"),
            };
            return (Err(why), Some(s));
        }
        let Some(program) = program else {
            return (Err(format!("{name} is not installed")), None);
        };
        let req = Request {
            prompt: a.prompt.clone(),
            cwd: a.cwd.clone(),
            read_files: true,
            model: h.as_ref().and_then(|h| h.model.clone()),
            timeout: Some(a.timeout),
            ..Request::default()
        };
        let mut run = match providers::start_program(a.kind, &program, &req) {
            Ok(r) => r,
            Err(e) => return (Err(format!("{name}: {e}")), None),
        };
        let mut interrupted = std::pin::pin!(tokio::signal::ctrl_c());
        let mut cancelled = false;
        let mut err = std::io::stderr();
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
                return (Err(format!("{name}: the run ended without a result")), None);
            };
            match e {
                Event::Started { model: m, .. } => {
                    let _ = writeln!(
                        err,
                        "[{name}{}, billed to {}]",
                        m.as_deref().map(|m| format!(" {m}")).unwrap_or_default(),
                        a.kind.billed_to()
                    );
                    model = m;
                }
                Event::Text(t) => {
                    let _ = write!(err, "{t}");
                    let _ = err.flush();
                }
                Event::ToolCall { name: tool, input, .. } => {
                    let _ = writeln!(err, "\n→ {tool} {input}");
                    tools.called(&tool);
                }
                Event::ToolResult { ok, text, .. } => {
                    if !ok {
                        tools.failed += 1;
                    }
                    let first = text.lines().next().unwrap_or_default();
                    let _ = writeln!(err, "← {} {first}", if ok { "ok" } else { "failed:" });
                }
                Event::Limit(l) => {
                    let _ = writeln!(err, "[limit: {l}]");
                }
                Event::Usage {
                    input_tokens,
                    output_tokens,
                    cost_usd,
                } => {
                    usage = Some(json!({
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cost_usd_equivalent": cost_usd,
                    }));
                }
                Event::Done(answer) => return (Ok(answer), None),
                Event::Failed(why) => return (Err(format!("{name}: {why}")), None),
            }
        }
    });
    let ok = answer.is_ok();
    let v = outcome(a.kind, answer, state.as_deref(), model, &tools, usage);
    let mut targets: Vec<PathBuf> = a.result.into_iter().collect();
    if let Some(dir) = std::env::var_os("DREAMER_JOB_ARTIFACTS").filter(|d| !d.is_empty()) {
        targets.push(Path::new(&dir).join(RESULT_FILE));
    }
    for t in &targets {
        write_result(t, &v)?;
    }
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!();
        match (&v["answer"], &v["error"]) {
            (Value::String(answer), _) => println!("{answer}"),
            (_, Value::String(e)) => eprintln!("dream: {e}"),
            _ => {}
        }
        eprintln!("[tools: {}]", tools.summary());
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_prompt_is_everything_after_the_separator() {
        let a = parse(&args(&[
            "--to", "codex", "--cwd", "/w", "--", "fix", "--to", "-L",
        ]))
        .expect("parse");
        assert_eq!(a.kind, Kind::CodexCli);
        assert_eq!(a.cwd, Some(PathBuf::from("/w")));
        assert_eq!(a.prompt, "fix --to -L");
        assert!(parse(&args(&["--", "x"])).is_err(), "--to is required");
        assert!(parse(&args(&["--to", "gemini", "--", "x"])).is_err());
        assert!(parse(&args(&["--to", "claude", "--"])).is_err());
    }

    #[test]
    fn tool_use_is_summarised_by_tool() {
        let mut t = ToolUse::default();
        assert_eq!(t.summary(), "none");
        t.called("Read");
        t.called("Grep");
        t.called("Read");
        t.failed = 1;
        assert_eq!(t.summary(), "Read ×2, Grep (1 failed)");
    }
}
