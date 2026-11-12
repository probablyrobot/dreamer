//! `dream`: Dreamer's command-line interface.
//!
//! Implemented so far:
//!
//! ```text
//! dream config-dir                      print the configuration directory
//! dream workspace list                  list workspace files and whether they are valid
//! dream workspace check <name|path>     validate a workspace and show its plan
//! dream workspace up <name|path> [-L socket] [--tmux PATH]
//!                                       create the workspace's tmux session
//! dream version                       client and daemon versions
//! dream status [--json]                 daemon process, socket, log, tmux endpoint
//! dream ensure-running [-L socket | --tmux-socket-path P] [--tmux PATH]
//!                                       start dreamerd unless it is running
//! dream shutdown                        stop dreamerd (tmux keeps running)
//! dream ls [--json] [-L socket]         sessions, windows, and panes through dreamerd
//! dream act <action> ... [-L socket]    run a typed action (see `dream act`)
//! dream actions [ID] [--limit N] [--json]
//!                                       the action ledger
//! dream doctor                          diagnostics; never starts the daemon
//! dream read %PANE [--history N] [--json]
//!                                       a pane's text from the daemon's mirror
//! dream mirrors [--json]                mirror state, tier, and journal of every pane
//! dream tier %PANE hot|warm             set a pane's observation tier
//! dream --version
//! ```

mod daemon;
mod remote;

use std::process::ExitCode;

use tmux_backend::workspace::{ApplyError, apply, apply_adopted, new_session_args};
use tmux_backend::{Connection, ConnectionConfig, Socket};
use workspace_config::{Plan, Start, config_dir, load, workspace_dir, workspace_path};

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
}

const USAGE: &str = "usage:
  dream config-dir
  dream workspace list
  dream workspace check <name|path>
  dream workspace up <name|path> [-L socket] [--tmux PATH]
  dream version
  dream status [--json]
  dream ensure-running [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream shutdown
  dream ls [--json] [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream act <action> ... [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream actions [ID] [--limit N] [--json]
  dream doctor
  dream read %PANE [--history N] [--json]
  dream mirrors [--json]
  dream tier %PANE hot|warm
  dream --version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version" | "-V"] => {
            println!("{}", version_line());
            Ok(())
        }
        ["config-dir"] => config_dir()
            .map(|d| println!("{}", d.display()))
            .ok_or_else(|| "cannot determine the configuration directory (HOME unset)".into()),
        ["workspace", "list"] => list(),
        ["workspace", "check", target] => check(target),
        ["workspace", "up", target, rest @ ..] => up(target, rest),
        ["version"] => daemon::version(),
        ["status"] => daemon::status(false),
        ["status", "--json"] => daemon::status(true),
        ["shutdown"] => daemon::shutdown(),
        ["ls", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| match extra.as_slice() {
                [] => remote::ls(&d, false),
                [j] if j == "--json" => remote::ls(&d, true),
                _ => Err(format!("unexpected arguments {extra:?}\n{USAGE}")),
            })
        }
        ["act", rest @ ..] => {
            match daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::act(&d, &extra)) {
                Ok(code) => return ExitCode::from(code),
                Err(e) => Err(e),
            }
        }
        ["actions", rest @ ..] => {
            remote::actions(&rest.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        }
        ["doctor"] => remote::doctor(),
        ["read", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::read(&d, &extra))
        }
        ["tier", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::tier(&d, &extra))
        }
        ["mirrors", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| match extra.as_slice() {
                [] => remote::mirrors(&d, false),
                [j] if j == "--json" => remote::mirrors(&d, true),
                _ => Err(format!("unexpected arguments {extra:?}\n{USAGE}")),
            })
        }
        ["ensure-running", rest @ ..] => daemon::split_daemon_args(rest).and_then(|(d, extra)| {
            if extra.is_empty() {
                daemon::ensure(&d)
            } else {
                Err(format!("unexpected arguments {extra:?}\n{USAGE}"))
            }
        }),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dream: {e}");
            ExitCode::FAILURE
        }
    }
}

fn resolve(target: &str) -> Result<std::path::PathBuf, String> {
    workspace_path(target).ok_or_else(|| {
        format!(
            "no workspace `{target}` (looked for a file, then {}/{target}.yaml)",
            workspace_dir().map_or_else(|| "<config dir>".into(), |d| d.display().to_string())
        )
    })
}

fn load_plan(target: &str) -> Result<Plan, String> {
    let path = resolve(target)?;
    load(&path).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn list() -> Result<(), String> {
    let dir = workspace_dir().ok_or("cannot determine the configuration directory")?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("no workspaces yet; create {}/<name>.yaml", dir.display());
        return Ok(());
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match load(&path) {
            Ok(plan) => println!(
                "{name:<20} {} window(s), {} pane(s)",
                plan.windows.len(),
                plan.windows.iter().map(|w| w.panes.len()).sum::<usize>()
            ),
            Err(errors) => println!("{name:<20} invalid ({} error(s))", errors.len()),
        }
    }
    Ok(())
}

fn check(target: &str) -> Result<(), String> {
    let plan = load_plan(target)?;
    println!("session {}  (root {})", plan.session, plan.root.display());
    for (i, w) in plan.windows.iter().enumerate() {
        let focus = if plan.focus_window == Some(i) {
            " *"
        } else {
            ""
        };
        println!(
            "  window {}{focus}",
            w.name.as_deref().unwrap_or("(unnamed)")
        );
        for r in &w.declared {
            let p = &w.panes[r.0];
            let what = match &p.start {
                Start::Shell => "shell".to_owned(),
                Start::Exec(argv) => format!("exec {argv:?}"),
                Start::ShellCommand(c) => format!("sh -c {c:?}"),
                Start::Run(c) => format!("run {c:?}"),
            };
            println!(
                "    pane {:<14} {what}{}",
                p.title.as_deref().unwrap_or("-"),
                if p.keep { " (keep)" } else { "" }
            );
        }
    }
    Ok(())
}

fn up(target: &str, rest: &[&str]) -> Result<(), String> {
    let plan = load_plan(target)?;
    let mut socket = Socket::Default;
    let mut tmux = std::env::var_os("DREAMER_TMUX")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "tmux".into());
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "-L" => socket = Socket::Name(it.next().ok_or("-L needs a name")?.to_string()),
            "--tmux" => tmux = it.next().ok_or("--tmux needs a path")?.into(),
            other => return Err(format!("unknown option `{other}`\n{USAGE}")),
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let existing = first_session(&tmux, &socket);
        let applied = match existing {
            Some(session) => {
                let config = ConnectionConfig::new(&tmux, socket.clone(), session);
                let (conn, _events) = Connection::spawn(config).await.map_err(|e| e.to_string())?;
                apply(&conn, &plan).await
            }
            None => {
                let mut config = ConnectionConfig::new(&tmux, socket.clone(), &plan.session);
                config.new_session_args = Some(new_session_args(&plan));
                let (conn, _events) = Connection::spawn(config).await.map_err(|e| e.to_string())?;
                apply_adopted(&conn, &plan).await
            }
        };
        match applied {
            Ok(done) => {
                let panes: usize = done.windows.iter().map(|(_, p)| p.len()).sum();
                println!(
                    "created session `{}`: {} window(s), {panes} pane(s)",
                    plan.session,
                    done.windows.len()
                );
                let socket_arg = match &socket {
                    Socket::Default => String::new(),
                    Socket::Name(n) => format!(" -L {n}"),
                    Socket::Path(p) => format!(" -S {}", p.display()),
                };
                println!(
                    "attach with: tmux{socket_arg} attach -t '={}'",
                    plan.session
                );
                Ok(())
            }
            Err(ApplyError::SessionExists(name)) => Err(format!(
                "session `{name}` already exists; attach to it or rename the workspace"
            )),
            Err(e) => Err(e.to_string()),
        }
    })
}

/// The name of any session on the server, or `None` if no server runs.
fn first_session(tmux: &std::path::Path, socket: &Socket) -> Option<String> {
    let mut cmd = std::process::Command::new(tmux);
    match socket {
        Socket::Default => {}
        Socket::Name(n) => {
            cmd.args(["-L", n]);
        }
        Socket::Path(p) => {
            cmd.arg("-S").arg(p);
        }
    }
    let out = cmd
        .args(["list-sessions", "-F", "#{session_name}"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| format!("={s}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dream "));
    }
}
