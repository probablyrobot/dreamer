//! `dream agents profile ...`: Dreamer's pi profile, its history, and its
//! check; and `dream agents pi`, which starts pi with it.

use std::path::PathBuf;

use providers::pi::Install;
use providers::pi_profile::{Ensured, Profile, Synced};

pub const USAGE: &str = "usage:
  dream agents profile init [--dir DIR]
  dream agents profile status [--dir DIR] [--json]
  dream agents profile log [--dir DIR] [-n N]
  dream agents profile diff [REV] [--dir DIR]
  dream agents profile revert REV [--dir DIR]
  dream agents profile commit [-m MESSAGE] [--session ID] [--no-check] [--dir DIR]
  dream agents profile check [--no-restore] [--dir DIR]
  dream agents pi [PI ARGUMENTS...]";

/// The version of the base layer this `dream` ships.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--dir DIR` and the remaining arguments.
fn split_dir(args: &[String]) -> Result<(Profile, Vec<String>), String> {
    let mut dir = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dir" {
            dir = Some(PathBuf::from(it.next().ok_or(USAGE)?));
        } else {
            rest.push(a.clone());
        }
    }
    Ok((dir.map_or_else(Profile::default_dir, Profile::new), rest))
}

/// The pi that sessions run, for the check.
fn pi_program() -> Result<PathBuf, String> {
    let settings = crate::harness::agent_settings()?;
    let h = settings.harness("pi").unwrap_or_default();
    let install = Install::default();
    match crate::harness::pi_program(&h, &install) {
        (Some(p), _) => Ok(p),
        (None, source) => Err(format!(
            "pi is not there ({source}); install it with `dream agents update`"
        )),
    }
}

fn print_synced(p: &Profile, s: &Synced) {
    println!(
        "profile {}: Dreamer's files for {}",
        p.dir.display(),
        s.version
    );
    if !s.written.is_empty() {
        println!("  written: {}", s.written.join(", "));
    }
    for k in &s.kept {
        println!("  kept the agent's version of {k}; Dreamer's newer one was not applied");
    }
    for k in &s.left_removed {
        println!("  left removed: {k}");
    }
    for k in &s.dropped {
        println!("  removed (no longer shipped): {k}");
    }
    match &s.commit {
        Some(c) => println!("  commit {c}"),
        None if p.versioned() => {}
        None => println!("  no history: git is not installed"),
    }
}

fn print_ensured(p: &Profile, e: &Ensured) {
    if let Some(s) = &e.synced {
        print_synced(p, s);
    }
    if let Some(c) = &e.committed {
        println!("committed changes made outside a pi session: {c}");
    }
    if let Some((to, why)) = &e.restored {
        println!("restored the profile to {to}, the last version pi loaded ({why})");
    }
    print_check(&e.check);
}

fn print_check(c: &providers::pi_profile::Check) {
    if c.ok {
        println!(
            "check: ok, pi loads the profile ({} commands)",
            c.commands.len()
        );
    } else {
        println!(
            "check: pi does not load the profile:\n  {}",
            c.problem
                .as_deref()
                .unwrap_or("no answer")
                .replace('\n', "\n  ")
        );
    }
    for n in &c.not_loaded {
        println!("  not loaded by pi (check its front matter): {n}");
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (profile, rest) = split_dir(args)?;
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match rest.as_slice() {
        ["init"] => {
            let s = profile.sync_base(VERSION)?;
            print_synced(&profile, &s);
            Ok(())
        }
        ["status", flags @ ..] => status(&profile, flags == ["--json"]),
        ["log", flags @ ..] => {
            let n = match flags {
                [] => 20,
                ["-n", n] => n.parse().map_err(|_| USAGE.to_owned())?,
                _ => return Err(USAGE.into()),
            };
            for e in profile.log(n)? {
                println!(
                    "{} {} {}{}",
                    e.commit,
                    time_text(e.time_unix),
                    e.subject,
                    if e.good { "  [loads]" } else { "" }
                );
            }
            Ok(())
        }
        ["diff"] => {
            print!("{}", profile.diff(None)?);
            Ok(())
        }
        ["diff", rev] => {
            print!("{}", profile.diff(Some(rev))?);
            Ok(())
        }
        ["revert", rev] => {
            let c = profile.revert(rev)?;
            println!("reverted {rev} as {c}");
            check_and_mark(&profile)
        }
        ["commit", flags @ ..] => commit(&profile, flags),
        ["check", flags @ ..] => {
            let restore = match flags {
                [] => true,
                ["--no-restore"] => false,
                _ => return Err(USAGE.into()),
            };
            let program = pi_program()?;
            if restore {
                let e = profile.ensure(&program, VERSION)?;
                print_ensured(&profile, &e);
                if e.check.ok {
                    Ok(())
                } else {
                    Err("pi does not load the profile".into())
                }
            } else {
                let c = profile.check(&program)?;
                print_check(&c);
                if c.ok {
                    Ok(())
                } else {
                    Err("pi does not load the profile".into())
                }
            }
        }
        _ => Err(USAGE.into()),
    }
}

fn commit(profile: &Profile, flags: &[&str]) -> Result<(), String> {
    let mut message = None;
    let mut session = None;
    let mut check = true;
    let mut it = flags.iter();
    while let Some(a) = it.next() {
        match *a {
            "-m" => message = Some(*it.next().ok_or(USAGE)?),
            "--session" => session = Some(*it.next().ok_or(USAGE)?),
            "--no-check" => check = false,
            _ => return Err(USAGE.into()),
        }
    }
    match profile.commit(message, session)? {
        Some(c) => println!("commit {c}"),
        None => println!("nothing to commit"),
    }
    if check {
        check_and_mark(profile)?;
    }
    Ok(())
}

/// Check the committed profile and mark it good when pi loads it. Without
/// an installed pi there is nothing to check with.
fn check_and_mark(profile: &Profile) -> Result<(), String> {
    let Ok(program) = pi_program() else {
        println!("check: not run, pi is not installed");
        return Ok(());
    };
    let c = profile.check(&program)?;
    print_check(&c);
    if c.ok {
        profile.mark_good()?;
    } else {
        println!(
            "fix it and commit again: at the next start Dreamer restores the last version pi loaded"
        );
    }
    Ok(())
}

fn time_text(unix: i64) -> String {
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - unix)
        .unwrap_or(0)
        .max(0);
    match ago {
        0..60 => format!("{ago}s ago"),
        60..3600 => format!("{}m ago", ago / 60),
        3600..86400 => format!("{}h ago", ago / 3600),
        _ => format!("{}d ago", ago / 86400),
    }
}

fn status(profile: &Profile, json: bool) -> Result<(), String> {
    let git = providers::pi_profile::git_available();
    let uncommitted = if profile.versioned() {
        profile.uncommitted()?
    } else {
        Vec::new()
    };
    let head = profile.log(1).ok().and_then(|l| l.into_iter().next());
    let good = profile.versioned().then(|| profile.last_good()).flatten();
    let edited = profile.edited_base_files();
    if json {
        let v = serde_json::json!({
            "dir": profile.dir.display().to_string(),
            "exists": profile.exists(),
            "base_version": profile.base_version(),
            "shipped_version": VERSION,
            "git": git,
            "versioned": profile.versioned(),
            "head": head.as_ref().map(|h| &h.commit),
            "last_good": good.as_ref().map(|g| g.chars().take(12).collect::<String>()),
            "uncommitted": uncommitted,
            "edited_base_files": edited,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("profile: {}", profile.dir.display());
    if !profile.exists() {
        println!(
            "  not laid out yet; `dream agents profile init` or `dream agents update` does it"
        );
        return Ok(());
    }
    println!(
        "  Dreamer's files: {}{}",
        profile.base_version().unwrap_or_else(|| "?".into()),
        if profile.base_version().as_deref() == Some(VERSION) {
            String::new()
        } else {
            format!(" (this dream ships {VERSION}; they update when pi next starts)")
        }
    );
    if !git {
        println!("  history: none, git is not installed");
    } else if let Some(h) = &head {
        println!("  head:      {} {}", h.commit, h.subject);
        println!(
            "  last good: {}",
            good.map(|g| g.chars().take(12).collect::<String>())
                .unwrap_or_else(|| "(none checked yet)".into())
        );
    }
    for e in &edited {
        println!("  changed by the agent: {e}");
    }
    if !uncommitted.is_empty() {
        println!("  uncommitted:");
        for u in &uncommitted {
            println!("    {u}");
        }
    }
    Ok(())
}

/// `dream agents pi [ARGS]`: start pi the way Dreamer runs it. With
/// Dreamer's profile, the profile is brought up to date and checked first,
/// and restored to the last version pi loaded if it no longer loads. With
/// `agents.pi.agent_dir` set, pi uses that directory and Dreamer's
/// extension is added with `--extension`.
pub fn launch(args: &[String]) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let settings = crate::harness::agent_settings()?;
    let h = settings.harness("pi").unwrap_or_default();
    let install = Install::default();
    let (program, extra) = match &h.command {
        Some(argv) => (
            crate::harness::pi_program(&h, &install)
                .0
                .ok_or_else(|| format!("agents.pi.command {argv:?} was not found"))?,
            argv[1..].to_vec(),
        ),
        None => (pi_program()?, Vec::new()),
    };
    let profile = Profile::default_dir();
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&extra);
    let agent_dir = match &h.agent_dir {
        Some(dir) => {
            if !profile.base_is_current(VERSION) {
                profile.sync_base(VERSION)?;
            }
            cmd.arg("--extension")
                .arg(profile.dir.join("extensions/dreamer/index.ts"));
            dir.clone()
        }
        None => {
            let e = profile.ensure(&program, VERSION)?;
            if e.synced.is_some() || e.restored.is_some() || !e.check.ok {
                print_ensured(&profile, &e);
            }
            if !e.check.ok {
                eprintln!("dream: starting pi anyway; it will report the problem");
            }
            profile.dir.clone()
        }
    };
    if let Some(m) = &h.model {
        cmd.arg("--model").arg(m);
    }
    cmd.args(args)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("DREAMER_RPC_SOCKET", platform::rpc_socket_path());
    if let Ok(me) = std::env::current_exe() {
        cmd.env("DREAMER_DREAM_BIN", me);
    }
    // pi uses its own login, never an API key from the environment.
    for v in providers::pi::SCRUBBED {
        cmd.env_remove(v);
    }
    Err(format!("{}: {}", program.display(), cmd.exec()))
}
