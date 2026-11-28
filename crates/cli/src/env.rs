//! `dream env`: the configuration directory, versioned with git (see
//! docs/environment.md).

use std::path::{Path, PathBuf};

use environment::{Change, Environment, InitOptions, Problem, Restored, Saved, Status};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Save what a command of Dreamer's just changed (`env.autosave`). Never
/// fails the command: a problem is reported and the command carries on.
pub(crate) fn autosave(paths: &[&str], message: &str) {
    let Some(dir) = workspace_config::config_dir() else {
        return;
    };
    match Environment::new(&dir).autosave(paths, message) {
        Ok(Some(save)) => println!("environment saved: {} {}", save.short, save.message),
        Ok(None) => {}
        Err(e) => eprintln!("dream: the environment was not saved: {e}"),
    }
}

const USAGE: &str = "\
dream env init [--name NAME] [--nested]   make the configuration directory an environment (or adopt its repository)
dream env status [--json]                 what changed since the last save, and whether it validates
dream env save [-m TEXT] [--force]        check everything, then save what changed
dream env log [-n N] [--json]             saves, newest first
dream env diff [REV] [--patch] [--json]   what changed since the last save (or REV)
dream env restore REV [--dry-run] [--json] bring the files back to save REV (saving what is there first)";

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let dir = workspace_config::config_dir()
        .ok_or("cannot determine the configuration directory (HOME unset)")?;
    let env = Environment::new(&dir);
    let (verb, rest) = args
        .split_first()
        .map_or(("status", &[][..]), |(v, r)| (v.as_str(), r));
    match verb {
        "init" => init(&env, rest),
        "status" => status(&env, rest),
        "save" => save(&env, rest),
        "log" => log(&env, rest),
        "diff" => diff(&env, rest),
        "restore" => restore(&env, rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown `dream env {other}`\n{USAGE}")),
    }
}

/// Flags of one command: `--name value` pairs and bare switches; the
/// rest are positional. Unknown flags are errors.
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
    positional: Vec<String>,
}

fn flags(args: &[String], with_value: &[&str], switches: &[&str]) -> Result<Flags, String> {
    let mut f = Flags {
        values: Vec::new(),
        switches: Vec::new(),
        positional: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if with_value.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
            f.values.push((a.clone(), v.clone()));
        } else if switches.contains(&a.as_str()) {
            f.switches.push(a.clone());
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unknown option {a}\n{USAGE}"));
        } else {
            f.positional.push(a.clone());
        }
    }
    Ok(f)
}

impl Flags {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn on(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn none_positional(&self) -> Result<(), String> {
        match self.positional.first() {
            Some(p) => Err(format!("unexpected argument {p:?}\n{USAGE}")),
            None => Ok(()),
        }
    }
}

fn json(v: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(v).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn init(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &["--name"], &["--nested"])?;
    f.none_positional()?;
    let done = env.init(
        &InitOptions {
            name: f.value("--name").map(str::to_owned),
            nested: f.on("--nested"),
        },
        VERSION,
    )?;
    let dir = env.dir().display();
    match (&done.saved, done.adopted) {
        (None, _) => println!("{dir} is already an environment; nothing changed"),
        (Some(s), false) => println!(
            "{dir} is now an environment: saved as {} ({})",
            s.short, s.message
        ),
        (Some(s), true) => println!(
            "{dir}: adopted its git repository, history unchanged; saved {} ({})",
            s.short, s.message
        ),
    }
    if !done.ignored.is_empty() {
        println!("never versioned: {}", done.ignored.join(" "));
    }
    let problems = problems(env.dir());
    if !problems.is_empty() {
        println!("does not validate yet (saves will refuse until it does):");
        print_problems(&problems);
    }
    Ok(())
}

fn status(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &[], &["--json"])?;
    f.none_positional()?;
    let s = env.status()?;
    let problems = problems(env.dir());
    if f.on("--json") {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            #[serde(flatten)]
            status: &'a Status,
            valid: bool,
            problems: &'a [Problem],
        }
        return json(&Out {
            status: &s,
            valid: problems.is_empty(),
            problems: &problems,
        });
    }
    let branch = s.branch.as_deref().unwrap_or("(no branch)");
    match &s.head {
        Some(h) => println!(
            "{} on {branch}; last saved {} {} {}",
            env.dir().display(),
            h.short,
            day(&h.date),
            h.message
        ),
        None => println!("{} on {branch}; nothing saved yet", env.dir().display()),
    }
    if s.changes.is_empty() {
        println!("no changes since the last save");
    } else {
        println!("changed since the last save:");
        print_changes(&s.changes);
    }
    if !s.tracked_but_ignored.is_empty() {
        println!(
            "saved although the ignore list keeps them out (they would be pushed): {}",
            s.tracked_but_ignored.join(" ")
        );
    }
    if problems.is_empty() {
        println!("validates");
    } else {
        println!("does not validate:");
        print_problems(&problems);
    }
    if let Some(b) = &s.blocked {
        println!("{b}");
    }
    Ok(())
}

fn save(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &["-m", "--message"], &["--force"])?;
    f.none_positional()?;
    let message = f.value("-m").or_else(|| f.value("--message"));
    let problems = problems(env.dir());
    match env.save(message, &problems, f.on("--force"))? {
        Saved::Nothing => println!("nothing changed since the last save"),
        Saved::Saved { save, changes } => {
            println!("saved {} {}", save.short, save.message);
            print_changes(&changes);
            if !problems.is_empty() {
                println!("saved with --force although it does not validate:");
                print_problems(&problems);
            }
        }
    }
    Ok(())
}

fn log(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &["-n"], &["--json"])?;
    f.none_positional()?;
    let n = match f.value("-n") {
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| format!("-n needs a number, not {v:?}"))?,
        None => 20,
    };
    let saves = env.log(n)?;
    if f.on("--json") {
        return json(&saves);
    }
    if saves.is_empty() {
        println!("nothing saved yet");
    }
    for s in saves {
        let auto = if s.automatic { "  (automatic)" } else { "" };
        println!("{}  {}  {}{auto}", s.short, day(&s.date), s.message);
    }
    Ok(())
}

fn diff(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &[], &["--patch", "--json"])?;
    if f.positional.len() > 1 {
        return Err(format!("one revision at most\n{USAGE}"));
    }
    let rev = f.positional.first().map(String::as_str);
    if f.on("--patch") {
        print!("{}", env.patch(rev)?);
        return Ok(());
    }
    let changes = env.diff(rev)?;
    if f.on("--json") {
        return json(&changes);
    }
    if changes.is_empty() {
        println!("no changes");
    } else {
        print_changes(&changes);
    }
    Ok(())
}

fn restore(env: &Environment, args: &[String]) -> Result<(), String> {
    let f = flags(args, &[], &["--dry-run", "--json"])?;
    let [rev] = f.positional.as_slice() else {
        return Err(format!(
            "which save? `dream env restore REV` (see `dream env log`)\n{USAGE}"
        ));
    };
    let dry = f.on("--dry-run");
    let done: Restored = env.restore(rev, &problems, dry)?;
    if f.on("--json") {
        return json(&done);
    }
    if dry {
        if done.changes.is_empty() {
            println!("restoring {} would change nothing", done.revision);
        } else {
            println!("restoring {} would change:", done.revision);
            print_changes(&done.changes);
        }
        return Ok(());
    }
    if let Some(b) = &done.saved_before {
        println!("saved what was there first: {} {}", b.short, b.message);
    }
    match &done.restored {
        Some(r) => {
            println!(
                "restored {}: saved as {} {}",
                done.revision, r.short, r.message
            );
            print_changes(&done.changes);
            if let Some(b) = &done.saved_before {
                println!("to undo: dream env restore {}", b.short);
            }
        }
        None => println!("the files already match {}; nothing changed", done.revision),
    }
    Ok(())
}

/// What stops a save: Dreamer's own checks and each installed app's.
fn problems(dir: &Path) -> Vec<Problem> {
    let mut out = environment::validate(dir, VERSION);
    let Ok(entries) = std::fs::read_dir(dir.join("apps")) else {
        return out;
    };
    let mut apps: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    apps.sort();
    for app in apps {
        let manifest = app.join("app.toml");
        if !manifest.is_file() {
            continue;
        }
        let verdict = crate::build::checked(&manifest).and_then(|v| crate::build::ok_or_error(&v));
        if let Err(message) = verdict {
            out.push(Problem {
                file: manifest,
                message,
            });
        }
    }
    out
}

fn print_changes(changes: &[Change]) {
    for c in changes {
        println!("  {:<12} {} {}", c.kind.label(), c.what.sign(), c.path);
    }
}

fn print_problems(problems: &[Problem]) {
    for p in problems {
        println!("  {}: {}", p.file.display(), p.message);
    }
}

/// The date of an RFC 3339 time.
fn day(rfc3339: &str) -> &str {
    rfc3339.get(..10).unwrap_or(rfc3339)
}
