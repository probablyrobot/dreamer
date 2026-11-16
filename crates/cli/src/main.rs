//! `dream`: Dreamer's command-line interface.
//!
//! Implemented so far:
//!
//! ```text
//! dream config-dir                      print the configuration directory
//! dream themes                        built-in terminal color schemes, the configured one marked
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
//! dream tier %PANE hot|warm|cold        set a pane's observation tier
//! dream bridge --stdio [-L socket | --tmux-socket-path P] [--tmux PATH]
//!                                       the daemon's protocol over stdin/stdout (for SSH)
//! dream hosts connect|list|call|disconnect ...
//!                                       the local daemon's connections to other hosts
//! dream service install [--no-start] [tmux options] | status | uninstall [--no-stop]
//!                                       an optional per-user service for dreamerd
//! dream app run APP_ID [--method M]... -- PROGRAM ARGS
//!                                       start a program as a registered app with a narrow grant
//! dream --version
//! ```

mod agents;
mod app;
mod ask;
mod bridge;
mod build;
mod daemon;
mod extprobe;
mod harness;
mod hosts;
mod jobrun;
mod mcp;
mod profile;
mod remote;
mod service;

use std::process::ExitCode;

use tmux_backend::workspace::{ApplyError, apply, apply_adopted, new_session_args};
use tmux_backend::{Connection, ConnectionConfig, Socket};
use workspace_config::{Plan, Start, config_dir, load, workspace_dir, workspace_path};

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
}

const USAGE: &str = "usage:
  dream config-dir
  dream themes
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
  dream tier %PANE hot|warm|cold
  dream bridge --stdio [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream rpc [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream extensions [--json] | dream extensions restart NAME
  dream mcp [--level observe|own|elsewhere] [--method M]... [--pane %N]... [--all-panes]
  dream approvals [--json] | allow ID [--pane] | deny ID
  dream agents status [--json] | update [--version X.Y.Z] | rollback   (the pi Dreamer installs)
  dream agents pi [ARGS...] | dream agents profile init|status|log|diff|revert|commit|check   (its profile)
  dream agents runs [--host NAME]... [--json] | dream agents hooks [--write FILE]
  dream agents providers [--json]
  dream agents ask [--provider claude|codex] [--pane %N]... [--all-panes] [--no-tools] [--timeout S] -- PROMPT
  dream jobs [--json] | start [--cwd DIR] [--name NAME] -- PROGRAM [ARGS...] | cancel ID | artifacts ID
  dream scripts [--json] | check FILE | load NAME FILE [options] | reload NAME FILE | stop NAME
  dream hosts connect|list|call|disconnect ... (see `dream hosts`)
  dream service install [--no-start] [-L socket | --tmux-socket-path PATH] [--tmux PATH]
  dream service status
  dream service uninstall [--no-stop]
  dream app check|preview|install|rollback|list|launch ... (see `dream app`)
  dream app run APP_ID [--method M]... [--all-panes | --pane %N...] [--session $N] -- PROGRAM [ARGS...]
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
        ["themes"] => themes(),
        ["config", "check"] => config_check(),
        ["skills"] | ["skills", "list"] => skills_list(),
        ["skills", "show", name] => skills_show(name),
        ["skills", "install", rest @ ..] => skills_install(rest),
        ["config", "schema"] => config_schema(false),
        ["config", "schema", "--json"] => config_schema(true),
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
        ["app", "check", rest @ ..] => build::check(&to_owned(rest)),
        ["app", "preview", rest @ ..] => build::preview(&to_owned(rest)),
        ["app", "install", rest @ ..] => build::install(&to_owned(rest)),
        ["app", "rollback", rest @ ..] => build::rollback(&to_owned(rest)),
        ["app", "list"] => build::list(),
        ["app", "launch", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| build::launch(&d, &extra))
        }
        ["app"] => Err(build::USAGE.into()),
        ["app", "run", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| app::run(&d, &extra))
        }
        ["approvals", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::approvals(&d, &extra))
        }
        ["jobs", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::jobs(&d, &extra))
        }
        ["scripts", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::scripts(&d, &extra))
        }
        ["extensions", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| remote::extensions(&d, &extra))
        }
        ["mcp", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| mcp::run(&d, &extra))
        }
        ["hook", rest @ ..] => {
            return agents::hook(&rest.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        }
        ["agents", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| agents::run(&d, &extra))
        }
        ["job-run", rest @ ..] => {
            return match jobrun::run(&rest.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("dream: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        ["extension-probe", rest @ ..] => {
            extprobe::run(&rest.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        }
        ["app", "probe", rest @ ..] => {
            app::probe(&rest.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        }
        ["hosts", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| hosts::run(&d, &extra))
        }
        ["service", "install", rest @ ..] => {
            daemon::split_daemon_args(rest).and_then(|(d, extra)| match extra.as_slice() {
                [] => service::install(&d.start, true),
                [n] if n == "--no-start" => service::install(&d.start, false),
                _ => Err(format!("unexpected arguments {extra:?}\n{USAGE}")),
            })
        }
        ["service", "status"] => service::status(),
        ["service", "uninstall"] => service::uninstall(true),
        ["service", "uninstall", "--no-stop"] => service::uninstall(false),
        ["rpc", rest @ ..] => daemon::split_daemon_args(rest).and_then(|(d, extra)| {
            if extra.is_empty() {
                bridge::rpc(&d)
            } else {
                Err(format!("unexpected arguments {extra:?}\n{USAGE}"))
            }
        }),
        ["bridge", rest @ ..] => daemon::split_daemon_args(rest).and_then(|(d, extra)| {
            if extra == ["--stdio"] {
                bridge::stdio(&d)
            } else {
                Err(format!("dream bridge needs --stdio\n{USAGE}"))
            }
        }),
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

/// Every built-in color scheme with a swatch of its colors, the configured
/// one marked. Swatches use 24-bit color only on a terminal.
/// Agent skills that ship with Dreamer: instructions an agent (Claude
/// Code, Codex) reads to help with Dreamer, so people need not edit its
/// files by hand. The same files are part of Dreamer's pi profile.
fn skills() -> Vec<(&'static str, &'static str)> {
    providers::pi_profile::shared_skills()
}

fn skill_description(text: &str) -> &str {
    text.lines()
        .find_map(|l| l.strip_prefix("description: "))
        .map(|d| d.trim_matches('"'))
        .unwrap_or_default()
}

fn skills_list() -> Result<(), String> {
    for (name, text) in skills() {
        println!("{name}\n    {}", skill_description(text));
    }
    println!("\nInstall for your agents with `dream skills install` (Claude Code and Codex).");
    Ok(())
}

fn skills_show(name: &str) -> Result<(), String> {
    let (_, text) = skills()
        .into_iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| format!("no skill `{name}`; see `dream skills`"))?;
    print!("{text}");
    Ok(())
}

/// `dream skills install [--claude] [--codex]`: write the skills where the
/// agents look for them (`~/.claude/skills/NAME/SKILL.md`,
/// `~/.codex/skills/NAME/SKILL.md`). Both by default. Only what Dreamer
/// ships is written, each skill in its own directory.
fn skills_install(args: &[&str]) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or("HOME is not set")?;
    let mut targets = Vec::new();
    for a in args {
        match *a {
            "--claude" => targets.push(home.join(".claude/skills")),
            "--codex" => targets.push(home.join(".codex/skills")),
            other => {
                return Err(format!(
                    "unknown option `{other}`; use --claude, --codex, or neither for both"
                ));
            }
        }
    }
    if targets.is_empty() {
        targets = vec![home.join(".claude/skills"), home.join(".codex/skills")];
    }
    for dir in targets {
        for (name, text) in skills() {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
            let f = d.join("SKILL.md");
            std::fs::write(&f, text).map_err(|e| format!("{}: {e}", f.display()))?;
            println!("installed {}", f.display());
        }
    }
    Ok(())
}

/// `dream config check`: every configuration file, checked with the
/// parsers the programs use. Exit status 1 if any has a problem.
fn config_check() -> Result<(), String> {
    let dir = config_dir().ok_or("cannot determine the configuration directory (HOME unset)")?;
    let results = workspace_config::check::check(&dir);
    let mut bad = 0;
    for c in &results {
        match (&c.problem, c.exists) {
            (Some(p), _) => {
                bad += 1;
                println!("✗ {}\n  {p}", c.file.display());
            }
            (None, true) => println!("✓ {}", c.file.display()),
            (None, false) => println!("· {} (absent: defaults)", c.file.display()),
        }
    }
    if let Ok(s) = workspace_config::load_settings() {
        harness::print_check(&s.agents);
    }
    if bad > 0 {
        return Err(format!(
            "{bad} file(s) with problems; Dreamer keeps the settings in force until they are fixed"
        ));
    }
    Ok(())
}

/// `dream config schema [--json]`: every key of dreamer.yaml with its kind,
/// default, meaning, an example, and whether it applies live.
fn config_schema(json: bool) -> Result<(), String> {
    let keys = workspace_config::check::SCHEMA;
    if json {
        let v: Vec<serde_json::Value> = keys
            .iter()
            .map(|k| {
                serde_json::json!({
                    "path": k.path, "kind": k.kind, "default": k.default,
                    "doc": k.doc, "example": k.example, "live": k.live,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for k in keys {
        println!("{}  ({})", k.path, k.kind);
        println!("    {}", k.doc);
        println!(
            "    default: {}   example: {}{}",
            k.default,
            k.example,
            if k.live {
                "   applies live"
            } else {
                "   applies to what starts next"
            }
        );
    }
    println!(
        "\nThe file: {}. Theme files: {}/themes. Check with `dream config check`.",
        workspace_config::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "dreamer.yaml".into()),
        config_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "<config dir>".into())
    );
    Ok(())
}

fn themes() -> Result<(), String> {
    use std::io::IsTerminal as _;
    let configured = workspace_config::load_settings()
        .map(|s| s.scheme().name)
        .unwrap_or(terminal_core::schemes::default().name);
    let color = std::io::stdout().is_terminal();
    let dir = workspace_config::themes::themes_dir();
    let (listed, problems) = workspace_config::themes::all(dir.as_deref());
    for l in &listed {
        let s = l.scheme;
        let mark = if s.name == configured { "*" } else { " " };
        let kind = match s.appearance {
            terminal_core::schemes::Appearance::Dark => "dark ",
            terminal_core::schemes::Appearance::Light => "light",
        };
        let swatch = if color {
            let bg = s.background;
            let mut out = format!("\x1b[48;2;{};{};{}m ", bg.r, bg.g, bg.b);
            for c in &s.ansi[..8] {
                out.push_str(&format!("\x1b[38;2;{};{};{}m●", c.r, c.g, c.b));
            }
            let fg = s.foreground;
            out.push_str(&format!("\x1b[38;2;{};{};{}m Aa \x1b[0m", fg.r, fg.g, fg.b));
            out
        } else {
            String::new()
        };
        let mine = if l.file.is_some() { "  (yours)" } else { "" };
        println!("{mark} {:<22} {kind} {swatch} {}{mine}", s.name, s.title);
    }
    for p in &problems {
        eprintln!("dream: theme {p}");
    }
    println!(
        "\nSet one with `terminal: {{theme: NAME}}` in {}; it applies when dreamerd starts \
         (`dream shutdown` restarts it on next use; tmux keeps running).\n\
         Add your own: an alacritty-theme .toml or a Ghostty theme file in {}.",
        workspace_config::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "dreamer.yaml".into()),
        dir.map(|d| d.display().to_string())
            .unwrap_or_else(|| "<config dir>/themes".into())
    );
    Ok(())
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

fn to_owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dream "));
    }
}
