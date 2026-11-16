//! Agent runs on the command line, and the provider hook helper.
//!
//! `dream hook claude` is what Claude Code runs as a command hook. It reads
//! the hook's JSON from standard input, sends a small report to the local
//! daemon with a short deadline, and exits 0 every time, quickly, whether
//! or not the daemon runs: a hook must never hold up or block the CLI. It
//! never starts the daemon and never writes to standard output (which
//! Claude Code would read as a decision).

use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::daemon::{DaemonArgs, call};
use crate::remote::session;

/// The whole budget of one hook report.
const DEADLINE: Duration = Duration::from_millis(800);
/// Input larger than this is not read.
const MAX_INPUT: u64 = 1 << 20;

/// Events whose reports change a run's state.
pub const EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Notification",
    "Stop",
    "SessionEnd",
];

pub fn hook(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) != Some("claude") {
        eprintln!("dream hook: only `claude` is supported");
        return ExitCode::SUCCESS;
    }
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut input);
    let Ok(v) = serde_json::from_str::<Value>(&input) else {
        return ExitCode::SUCCESS;
    };
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let (Some(session_id), Some(event)) = (text("session_id"), text("hook_event_name")) else {
        return ExitCode::SUCCESS;
    };
    let report = api::AgentEvent {
        provider: "claude".into(),
        detail: text("notification_type").or_else(|| text("reason")),
        event,
        session_id,
        cwd: text("cwd"),
        hook_pid: std::process::id(),
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::SUCCESS;
    };
    // The timer is made inside the runtime.
    let _ = rt.block_on(async {
        tokio::time::timeout(DEADLINE, async {
            let c = client::Client::connect(&platform::socket_path(), "hook")
                .await
                .ok()?;
            c.call(api::Request::AgentsEvent(report)).await.ok()
        })
        .await
    });
    ExitCode::SUCCESS
}

/// The settings that make Claude Code report to `dream`.
pub fn settings(dream: &str) -> Value {
    let group = json!([{ "hooks": [{
        "type": "command",
        "command": format!("{} hook claude", shell_word(dream)),
        "timeout": 5,
    }] }]);
    let mut hooks = serde_json::Map::new();
    for e in EVENTS {
        hooks.insert(e.to_owned(), group.clone());
    }
    json!({ "hooks": hooks })
}

fn shell_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Add `ours` to the settings in `existing`, keeping everything there and
/// adding each hook only once. The number of hooks added.
pub fn merge(existing: &mut Value, ours: &Value) -> Result<usize, String> {
    let root = existing
        .as_object_mut()
        .ok_or("the settings file is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("its `hooks` is not an object")?;
    let mut added = 0;
    for (event, groups) in ours["hooks"].as_object().into_iter().flatten() {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} is not a list"))?;
        let command = &groups[0]["hooks"][0]["command"];
        let present = list.iter().any(|g| {
            g["hooks"]
                .as_array()
                .is_some_and(|hs| hs.iter().any(|h| h["command"] == *command))
        });
        if !present {
            list.push(groups[0].clone());
            added += 1;
        }
    }
    Ok(added)
}

/// `dream agents status|update|rollback|hooks|runs|providers|ask`.
pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let usage = "usage:\n  dream agents status [--harness pi] [--json]\n  dream agents update [--harness pi] [--version X.Y.Z]\n  dream agents rollback [--harness pi]\n  dream agents pi [PI ARGUMENTS...]\n  dream agents profile init|status|log|diff|revert|commit|check ... (see `dream agents profile`)\n  dream agents runs [--host NAME]... [--json]\n  dream agents hooks [--write SETTINGS_FILE]\n  dream agents sessions [--json] | create | send | events | approve | deny | cancel | resume | close\n  dream agents providers [--json]\n  dream agents ask [options] -- PROMPT";
    match args.first().map(String::as_str) {
        Some("status") => crate::harness::status(&args[1..]),
        Some("update") => crate::harness::update(&args[1..]),
        Some("rollback") => crate::harness::rollback(&args[1..]),
        Some("profile") => crate::profile::run(&args[1..]),
        Some("pi") => crate::profile::launch(&args[1..]),
        Some("providers") => crate::ask::providers(&args[1..]),
        Some("sessions") => crate::sessions::run(d, &args[1..]),
        Some("ask") => {
            let code = crate::ask::ask(d, &args[1..])?;
            if code == std::process::ExitCode::SUCCESS {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Some("hooks") => {
            let dream = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "dream".into());
            let ours = settings(&dream);
            match &args[1..] {
                [] => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&ours).map_err(|e| e.to_string())?
                    );
                    eprintln!(
                        "Add these hooks to Claude Code's settings (for example ~/.claude/settings.json), \
                         or run `dream agents hooks --write FILE` to add them to FILE."
                    );
                    Ok(())
                }
                [flag, file] if flag == "--write" => {
                    write_settings(std::path::Path::new(file), &ours)
                }
                _ => Err(usage.into()),
            }
        }
        Some("runs") => {
            let mut hosts = Vec::new();
            let mut json_out = false;
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--host" => hosts.push(it.next().ok_or(usage)?.clone()),
                    "--json" => json_out = true,
                    _ => return Err(usage.into()),
                }
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                let mut c = session(d).await?;
                let api::Response::AgentsRuns(l) = call(
                    &mut c,
                    api::Request::AgentsRuns(api::AgentRunQuery {
                        hosts,
                        provider: None,
                    }),
                )
                .await?
                else {
                    return Err("unexpected reply".into());
                };
                if json_out {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
                    );
                    return Ok(());
                }
                if l.runs.is_empty() {
                    println!("no agent runs");
                }
                for r in &l.runs {
                    println!(
                        "{}{} pid {} {} ({}, {}) pane {} {}",
                        r.host
                            .as_deref()
                            .map(|h| format!("{h}: "))
                            .unwrap_or_default(),
                        r.provider,
                        r.pid,
                        r.state,
                        r.instrumentation,
                        r.confidence,
                        r.pane.map_or_else(|| "-".into(), |p| format!("%{p}")),
                        r.cwd.as_deref().unwrap_or("")
                    );
                }
                for n in &l.notes {
                    println!("note: {n}");
                }
                Ok(())
            })
        }
        _ => Err(usage.into()),
    }
}

/// Add the hooks to `file`, after a backup; only on explicit request.
fn write_settings(file: &std::path::Path, ours: &Value) -> Result<(), String> {
    let mut existing = match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("{}: not JSON ({e}); nothing changed", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let added = merge(&mut existing, ours)?;
    if added == 0 {
        println!("{}: the hooks are already there", file.display());
        return Ok(());
    }
    if file.exists() {
        let backup = file.with_extension("json.bak");
        std::fs::copy(file, &backup).map_err(|e| format!("{}: {e}", backup.display()))?;
        println!("backed up {} to {}", file.display(), backup.display());
    }
    let text = serde_json::to_string_pretty(&existing).map_err(|e| e.to_string())?;
    std::fs::write(file, format!("{text}\n")).map_err(|e| format!("{}: {e}", file.display()))?;
    println!("added {added} hooks to {}", file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_keeps_what_is_there_and_adds_each_hook_once() {
        let ours = settings("/opt/dream/bin/dream");
        let mut existing = json!({
            "model": "x",
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "notify-me" }] }] }
        });
        assert_eq!(merge(&mut existing, &ours).expect("merge"), EVENTS.len());
        assert_eq!(existing["model"], "x");
        assert_eq!(existing["hooks"]["Stop"].as_array().map(Vec::len), Some(2));
        assert_eq!(merge(&mut existing, &ours).expect("again"), 0, "idempotent");
        assert!(merge(&mut json!([1]), &ours).is_err());
    }
}
