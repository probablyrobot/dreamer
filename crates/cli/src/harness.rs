//! `dream agents status|update|rollback`: the agent harness Dreamer manages
//! (pi), and the harness lines of `dream config check`.

use std::path::PathBuf;

use providers::pi::{self, Install};
use workspace_config::agents::{AgentSettings, Harness, PiVersion};

pub const USAGE: &str = "usage:
  dream agents status [--harness pi] [--json]
  dream agents update [--harness pi] [--version X.Y.Z]
  dream agents rollback [--harness pi]";

/// pi's providers whose login `status` reports: OpenAI (a ChatGPT
/// subscription signs in here) and Anthropic.
const LOGIN_PROVIDERS: [&str; 2] = ["openai", "anthropic"];

fn agent_settings() -> Result<AgentSettings, String> {
    workspace_config::load_settings()
        .map(|s| s.agents)
        .map_err(|e| e.to_string())
}

/// `--harness NAME` (only pi is managed) and the remaining arguments.
fn harness_arg(args: &[String]) -> Result<Vec<String>, String> {
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--harness" {
            match it.next().map(String::as_str) {
                Some("pi") => {}
                Some("claude" | "codex") => {
                    return Err(
                        "Dreamer installs only pi; claude and codex are the CLIs you installed \
                         (see `dream agents providers`)"
                            .into(),
                    );
                }
                Some(other) => return Err(format!("Dreamer does not install `{other}`; only pi")),
                None => return Err(USAGE.into()),
            }
        } else {
            rest.push(a.clone());
        }
    }
    Ok(rest)
}

/// The pi agent directory Dreamer's sessions use.
fn agent_dir(h: &Harness) -> PathBuf {
    h.agent_dir.clone().unwrap_or_else(pi::profile_dir)
}

/// A configured program: a path as given, a bare name from `PATH`.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        Some(PathBuf::from(program)).filter(|p| p.is_file())
    } else {
        providers::locate_in_path(program)
    }
}

/// The pi program sessions would run, and where it comes from.
fn pi_program(h: &Harness, install: &Install) -> (Option<PathBuf>, String) {
    match &h.command {
        Some(argv) => (resolve(&argv[0]), format!("agents.pi.command {argv:?}")),
        None => {
            let c = install.command();
            (
                c.is_file().then_some(c),
                format!("Dreamer's install in {}", install.root.display()),
            )
        }
    }
}

fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn status(args: &[String]) -> Result<(), String> {
    let json = match harness_arg(args)?.as_slice() {
        [] => false,
        [j] if j == "--json" => true,
        _ => return Err(USAGE.into()),
    };
    let settings = agent_settings()?;
    let h = settings.harness("pi").unwrap_or_default();
    let install = Install::default();
    let (program, source) = pi_program(&h, &install);
    let runs = program.as_deref().map(pi::run_version);
    let dir = agent_dir(&h);
    let logins: Vec<pi::Login> = match (&program, &runs) {
        (Some(p), Some(Ok(_))) => LOGIN_PROVIDERS
            .iter()
            .map(|prov| pi::login(p, &dir, prov))
            .collect(),
        _ => Vec::new(),
    };
    let wanted = match settings.pi_version() {
        PiVersion::Latest => "latest".to_owned(),
        PiVersion::Exact(v) => v,
    };
    let login_hint = program.as_ref().map(|p| {
        format!(
            "PI_CODING_AGENT_DIR={} {} auth login openai   (or /login inside pi)",
            shell_word(&dir.display().to_string()),
            shell_word(&p.display().to_string())
        )
    });
    if json {
        let v = serde_json::json!({
            "harness": "pi",
            "default": settings.default_harness(),
            "source": source,
            "program": program.as_ref().map(|p| p.display().to_string()),
            "root": install.root.display().to_string(),
            "active": install.current(),
            "previous": install.previous(),
            "installed": install.installed(),
            "version_setting": wanted,
            "runs": matches!(runs, Some(Ok(_))),
            "version": runs.as_ref().and_then(|r| r.as_ref().ok()),
            "error": runs.as_ref().and_then(|r| r.as_ref().err()),
            "agent_dir": dir.display().to_string(),
            "logins": logins.iter().map(|l| serde_json::json!({
                "provider": l.provider, "status": l.status, "reason": l.reason,
            })).collect::<Vec<_>>(),
            "login_command": login_hint,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("pi: {source}");
    if h.command.is_none() {
        let installed = install.installed();
        if installed.is_empty() {
            println!("  not installed; install it with `dream agents update`");
        } else {
            println!(
                "  active:    {}",
                install.current().as_deref().unwrap_or("(none)")
            );
            println!(
                "  previous:  {}",
                install.previous().as_deref().unwrap_or("(none kept)")
            );
            println!("  installed: {}", installed.join(", "));
        }
        println!("  version setting: {wanted}");
    }
    match (&program, &runs) {
        (None, _) if h.command.is_some() => println!("  runs:      no, the command was not found"),
        (None, _) => {}
        (Some(_), Some(Ok(v))) => println!("  runs:      yes, `pi --version` says {v}"),
        (Some(_), Some(Err(e))) => println!("  runs:      no: {e}"),
        (Some(_), None) => {}
    }
    println!("  agent dir: {}", dir.display());
    for l in &logins {
        println!(
            "  login {:<10} {}{}",
            l.provider,
            l.status,
            l.reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        );
    }
    if !logins.is_empty()
        && logins.iter().all(|l| l.status != "ready")
        && let Some(hint) = &login_hint
    {
        println!("  log in:    {hint}");
    }
    println!("  default harness: {}", settings.default_harness());
    Ok(())
}

pub fn update(args: &[String]) -> Result<(), String> {
    let mut version = None;
    let rest = harness_arg(args)?;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => version = Some(it.next().ok_or(USAGE)?.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    let settings = agent_settings()?;
    if version.is_none()
        && let PiVersion::Exact(v) = settings.pi_version()
    {
        version = Some(v);
    }
    let install = Install::default();
    let done = install.update(version.as_deref(), &mut |m| eprintln!("{m}"))?;
    if done.unchanged {
        println!("pi {} is already the active release", done.version);
    } else {
        println!(
            "pi {} is active{}",
            done.version,
            done.previous
                .as_deref()
                .map(|p| format!("; {p} is kept for `dream agents rollback`"))
                .unwrap_or_default()
        );
    }
    println!("  {}", install.command().display());
    if settings.harness("pi").is_some_and(|h| h.command.is_some()) {
        println!("note: agents.pi.command is set, so sessions run that command, not this install");
    }
    Ok(())
}

pub fn rollback(args: &[String]) -> Result<(), String> {
    if !harness_arg(args)?.is_empty() {
        return Err(USAGE.into());
    }
    let install = Install::default();
    let v = install.rollback()?;
    println!(
        "pi {v} is active again{}",
        install
            .previous()
            .map(|p| format!("; {p} is kept (roll back again to return to it)"))
            .unwrap_or_default()
    );
    Ok(())
}

/// One harness as `dream config check` reports it.
#[derive(Debug)]
pub struct HarnessCheck {
    pub name: String,
    pub default: bool,
    /// The program sessions would run, when found.
    pub program: Option<PathBuf>,
    /// Where it was looked for.
    pub looked: String,
    /// Login state in the harness's own words, when it can say.
    pub login: String,
}

/// Every harness: built-in ones and configured ones, whether its program
/// is there, and its login state by its own status command.
pub fn check(settings: &AgentSettings) -> Vec<HarnessCheck> {
    let install = Install::default();
    settings
        .all()
        .into_iter()
        .map(|(name, h)| {
            let default = name == settings.default_harness();
            let (program, looked, login) = match name.as_str() {
                "pi" => {
                    let (program, looked) = pi_program(&h, &install);
                    let login = match &program {
                        Some(p) => LOGIN_PROVIDERS
                            .iter()
                            .map(|prov| {
                                let l = pi::login(p, &agent_dir(&h), prov);
                                format!("{} {}", l.provider, l.status)
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        None => "-".into(),
                    };
                    (program, looked, login)
                }
                "claude" | "codex" => {
                    let kind = providers::Kind::parse(&name).unwrap_or(providers::Kind::ClaudeCli);
                    let (program, looked) = match &h.command {
                        Some(argv) => {
                            (resolve(&argv[0]), format!("agents.{name}.command {argv:?}"))
                        }
                        None => (providers::locate(kind), format!("`{name}` on PATH")),
                    };
                    let login = match &program {
                        Some(p) => tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map(|rt| {
                                let probe =
                                    rt.block_on(providers::probe_program(kind, Some(p.clone())));
                                crate::ask::state_text(&probe.state)
                            })
                            .unwrap_or_else(|e| e.to_string()),
                        None => "-".into(),
                    };
                    (program, looked, login)
                }
                _ => {
                    let argv = h.command.clone().unwrap_or_default();
                    let program = argv.first().and_then(|p| resolve(p));
                    (
                        program,
                        format!("agents.{name}.command {argv:?}"),
                        "reported by the agent when a session starts".into(),
                    )
                }
            };
            HarnessCheck {
                name,
                default,
                program,
                looked,
                login,
            }
        })
        .collect()
}

/// The lines `dream config check` prints for the harnesses.
pub fn print_check(settings: &AgentSettings) {
    println!("agent harnesses (default: {}):", settings.default_harness());
    for c in check(settings) {
        let mark = if c.program.is_some() { "✓" } else { "·" };
        let star = if c.default { " (default)" } else { "" };
        match &c.program {
            Some(p) => println!(
                "{mark} {}{star}: {}\n  login: {}",
                c.name,
                p.display(),
                c.login
            ),
            None => {
                let hint = if c.name == "pi" && c.looked.starts_with("Dreamer") {
                    "; install it with `dream agents update`"
                } else {
                    ""
                };
                println!("{mark} {}{star}: not found ({}){hint}", c.name, c.looked);
            }
        }
    }
}
