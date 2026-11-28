//! The small-app builder's steps, for people and agents alike:
//! check a package, preview it from its fixtures, install it (keeping the
//! version it replaces as the last good one), roll back, list, and launch
//! it in a real tmux pane with exactly the grant it declares.
//!
//! Checking and drawing are `dreamer-termapp`'s (`--check --json`,
//! `--preview`); this runs it and moves files. For a script app it first
//! builds the JavaScript from the `app.ts` (`strip`), so check, preview,
//! and install always run what the TypeScript says now.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::daemon::DaemonArgs;

pub const USAGE: &str = "usage:
  dream app new NAME [--template table|form|chat] [--dir DIR]
                                          a working package to start from
  dream app check PATH [--json]           validate a package (a directory or its app.toml); build and check its app.ts
  dream app types DIR                     write types/dreamer.d.ts (and a tsconfig.json) for editors
  dream app preview PATH [--text] [--size WxH]
                                          draw it once from its fixtures as text, no daemon
  dream app preview PATH --open [--session $N]
                                          a live preview pane the desktop draws; reloads on save
  dream app preview PATH --png FILE [--level observe|own|elsewhere]
                                          the desktop's drawing of the preview pane as a PNG;
                                          with --level, as an agent at that level takes it
  dream app test PATH [--update] [--size WxH]
                                          replay fixtures/events.jsonl and compare each screen with fixtures/expect/
  dream app install PATH [--name NAME]    check, preview, and install; the replaced version is kept
  dream app rollback NAME                 put the last good version back
  dream app list                          installed packages
  dream app launch NAME [--session $N | --beside %N] [--request]
                                          run it in a new tmux window (or beside a pane) with the grant it declares;
                                          --request prints the apps.launch parameters instead
  dream app run APP_ID [--method M]... [--all-panes] [--session $N] -- PROGRAM [ARGS...]";

/// `dreamer-termapp` beside this program, else on `PATH`.
pub(crate) fn termapp() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("dreamer-termapp")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("dreamer-termapp"))
}

/// A package's manifest: `PATH/app.toml` for a directory.
pub(crate) fn manifest(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_dir() { p.join("app.toml") } else { p }
}

fn apps() -> Result<PathBuf, String> {
    workspace_config::config_dir()
        .map(|d| d.join("apps"))
        .ok_or_else(|| "cannot determine the configuration directory (HOME unset)".into())
}

/// The check's JSON: `ok`, and `id`, `version`, `title`, `needs`, or
/// `error`.
pub(crate) fn checked(manifest: &Path) -> Result<Value, String> {
    let out = Command::new(termapp())
        .arg("--check")
        .arg(manifest)
        .arg("--json")
        .output()
        .map_err(|e| format!("{}: {e}", termapp().display()))?;
    serde_json::from_slice(&out.stdout).map_err(|e| {
        format!(
            "dreamer-termapp --check: {e}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })
}

pub(crate) fn ok_or_error(v: &Value) -> Result<(), String> {
    if v["ok"] == Value::Bool(true) {
        Ok(())
    } else {
        Err(v["error"].as_str().unwrap_or("invalid package").to_owned())
    }
}

pub(crate) fn preview_text(manifest: &Path, size: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new(termapp());
    cmd.arg("--preview").arg(manifest);
    if let Some(s) = size {
        cmd.args(["--size", s]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", termapp().display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

pub fn check(args: &[String]) -> Result<(), String> {
    let (path, json) = match args {
        [p] => (p, false),
        [p, j] if j == "--json" => (p, true),
        _ => return Err(USAGE.into()),
    };
    let m = manifest(path);
    if let Err(e) = crate::strip::build(&m) {
        if json {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": e, "problems": [] })
            );
            std::process::exit(1);
        }
        return Err(e);
    }
    let problems = crate::strip::type_problems(&m);
    let v = match checked(&m) {
        Ok(v) => v,
        Err(e) if !problems.is_empty() => serde_json::json!({ "ok": false, "error": e }),
        Err(e) => return Err(e),
    };
    let v = with_problems(v, &problems);
    if !json && !problems.is_empty() {
        let mut lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
        if let Some(e) = v["error"].as_str().filter(|e| *e != lines[0]) {
            lines.push(e.to_owned());
        }
        return Err(lines.join("\n"));
    }
    if json {
        println!("{v}");
        if ok_or_error(&v).is_err() {
            // The JSON says why; only the exit status is left to give.
            std::process::exit(1);
        }
        return Ok(());
    }
    ok_or_error(&v)?;
    println!(
        "ok: {} {} ({}); needs {}",
        v["id"].as_str().unwrap_or(""),
        v["version"].as_str().unwrap_or(""),
        v["title"].as_str().unwrap_or(""),
        needs_text(&v["needs"])
    );
    Ok(())
}

/// The check's JSON with a script app's type-level mistakes as `problems`
/// (`file`, `line`, `column`, `message`); any of them fails the check, and
/// the first is the `error` when the package has none of its own.
fn with_problems(mut v: Value, problems: &[crate::typecheck::Problem]) -> Value {
    v["problems"] = serde_json::to_value(problems).unwrap_or_default();
    if let Some(first) = problems.first() {
        v["ok"] = Value::Bool(false);
        if !v["error"].is_string() {
            v["error"] = Value::String(first.to_string());
        }
    }
    v
}

/// The declarations an editor needs beside app.ts.
const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "es2023",
    "lib": ["es2023"],
    "module": "es2022",
    "moduleResolution": "bundler",
    "strict": true,
    "noEmit": true
  },
  "include": ["*.ts", "types/dreamer.d.ts"]
}
"#;

/// `dream app types DIR`: write `types/dreamer.d.ts` into a package, and a
/// `tsconfig.json` that uses it when the package has none.
pub fn types(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err(USAGE.into());
    };
    let dir = PathBuf::from(path);
    if !dir.is_dir() {
        return Err(format!("{path} is not a directory"));
    }
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let types = dir.join("types");
    std::fs::create_dir_all(&types).map_err(|e| fail(&types, e))?;
    let dts = types.join("dreamer.d.ts");
    std::fs::write(&dts, api::TYPESCRIPT_DECLARATIONS).map_err(|e| fail(&dts, e))?;
    println!("wrote {}", dts.display());
    let tsconfig = dir.join("tsconfig.json");
    if !tsconfig.exists() {
        std::fs::write(&tsconfig, TSCONFIG).map_err(|e| fail(&tsconfig, e))?;
        println!("wrote {}", tsconfig.display());
    }
    Ok(())
}

fn needs_text(n: &Value) -> String {
    let methods: Vec<&str> = n["methods"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let panes = if n["all_panes"] == Value::Bool(true) {
        "every pane".to_owned()
    } else {
        match n["panes"].as_array().map(Vec::len).unwrap_or(0) {
            0 => "no panes".to_owned(),
            k => format!("{k} panes"),
        }
    };
    format!("{} on {panes}", methods.join(", "))
}

/// A package path and its options: the flags in `switches`, and
/// `--size WxH`.
fn path_and_flags<'a>(
    args: &'a [String],
    switches: &[&str],
) -> Result<(&'a str, Vec<&'a str>, Option<&'a str>), String> {
    let [path, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    if path.starts_with('-') {
        return Err(USAGE.into());
    }
    let (mut on, mut size) = (Vec::new(), None);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" => size = Some(it.next().ok_or("--size needs WIDTHxHEIGHT")?.as_str()),
            s if switches.contains(&s) => on.push(s),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    Ok((path, on, size))
}

/// Where a package's expected screens are: `fixtures/expect/` beside its
/// manifest.
fn expect_dir(manifest: &Path) -> PathBuf {
    manifest
        .parent()
        .unwrap_or(Path::new("."))
        .join("fixtures")
        .join("expect")
}

/// `NNN.txt`: the screen after event `n` (0, the first view).
fn screen_file(n: usize) -> String {
    format!("{n:03}.txt")
}

/// The screens of a package's test, from `dreamer-termapp --replay`: the
/// first view, then one after each event.
fn replayed(manifest: &Path, size: Option<&str>) -> Result<Vec<String>, String> {
    let mut cmd = Command::new(termapp());
    cmd.arg("--replay").arg(manifest);
    if let Some(s) = size {
        cmd.args(["--size", s]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", termapp().display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("dreamer-termapp --replay: {e}"))?;
    Ok(v["screens"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| s.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default())
}

/// The expected screens kept now, by number.
fn kept_screens(dir: &Path) -> Vec<usize> {
    let mut kept: Vec<usize> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name.strip_suffix(".txt")?;
            (n.len() == 3).then(|| n.parse().ok())?
        })
        .collect();
    kept.sort_unstable();
    kept
}

/// Replay a package's events and write each screen as its expected one,
/// removing expected screens no event makes any more. How many there are.
pub fn record(manifest: &Path, size: Option<&str>) -> Result<usize, String> {
    let screens = replayed(manifest, size)?;
    let dir = expect_dir(manifest);
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    std::fs::create_dir_all(&dir).map_err(|e| fail(&dir, e))?;
    for (n, s) in screens.iter().enumerate() {
        let file = dir.join(screen_file(n));
        std::fs::write(&file, s).map_err(|e| fail(&file, e))?;
    }
    for n in kept_screens(&dir) {
        if n >= screens.len() {
            let file = dir.join(screen_file(n));
            std::fs::remove_file(&file).map_err(|e| fail(&file, e))?;
        }
    }
    Ok(screens.len())
}

/// The events of a test as written, for naming them in a failure.
fn event_lines(manifest: &Path) -> Vec<String> {
    let file = manifest
        .parent()
        .unwrap_or(Path::new("."))
        .join("fixtures")
        .join("events.jsonl");
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The rows that differ between two screens: each row's number, then the
/// expected (`-`) and the drawn (`+`) row.
fn screen_diff(expected: &str, drawn: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), drawn.lines().collect());
    let mut out = String::new();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i), b.get(i));
        if x != y {
            out.push_str(&format!(
                "  {:>3} - {}\n  {:>3} + {}\n",
                i + 1,
                x.copied().unwrap_or("<no row>"),
                i + 1,
                y.copied().unwrap_or("<no row>")
            ));
        }
    }
    out
}

/// `dream app test PATH [--update] [--size WxH]`.
pub fn test(args: &[String]) -> Result<(), String> {
    let (path, on, size) = path_and_flags(args, &["--update"])?;
    let m = manifest(path);
    crate::strip::build(&m)?;
    if on.contains(&"--update") {
        let n = record(&m, size)?;
        println!(
            "recorded {n} expected screens in {}",
            expect_dir(&m).display()
        );
        return Ok(());
    }
    let screens = replayed(&m, size)?;
    let dir = expect_dir(&m);
    let events = event_lines(&m);
    let mut failures = Vec::new();
    for (n, drawn) in screens.iter().enumerate() {
        let what = match n {
            0 => "the first view".to_owned(),
            n => format!(
                "after event {n} {}",
                events.get(n - 1).map_or("", String::as_str)
            ),
        };
        let file = dir.join(screen_file(n));
        match std::fs::read_to_string(&file) {
            Err(_) => failures.push(format!(
                "{what}: {} is missing; `dream app test {path} --update` records it",
                file.display()
            )),
            Ok(expected) if expected != *drawn => failures.push(format!(
                "{what}: the screen differs from {}\n{}",
                file.display(),
                screen_diff(&expected, drawn)
            )),
            Ok(_) => {}
        }
    }
    for n in kept_screens(&dir) {
        if n >= screens.len() {
            failures.push(format!(
                "{}: no event draws it any more",
                dir.join(screen_file(n)).display()
            ));
        }
    }
    if failures.is_empty() {
        println!(
            "ok: {} screens match ({} events)",
            screens.len(),
            screens.len().saturating_sub(1)
        );
        return Ok(());
    }
    Err(format!(
        "{}\n{} of {} screens differ; after a deliberate change, `dream app test {path} --update` records them",
        failures.join("\n"),
        failures.len(),
        screens.len()
    ))
}

/// Copy a package's regular files and directories; a symbolic link or
/// anything else is refused, so an install copies only what it shows.
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    std::fs::create_dir_all(to).map_err(|e| fail(to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| fail(from, e))? {
        let entry = entry.map_err(|e| fail(from, e))?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type().map_err(|e| fail(&src, e))?;
        if kind.is_dir() {
            copy_dir(&src, &dst)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| fail(&src, e))?;
        } else {
            return Err(format!(
                "{}: only files and directories are installed",
                src.display()
            ));
        }
    }
    Ok(())
}

/// A name for the installed directory: letters, digits, `-`, `_`, `.`.
fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a package name (letters, digits, -, _, .)"
        ))
    }
}

pub fn install(args: &[String]) -> Result<(), String> {
    let (path, name) = match args {
        [p] => (p, None),
        [p, f, n] if f == "--name" => (p, Some(n.clone())),
        _ => return Err(USAGE.into()),
    };
    let m = manifest(path);
    let dir = m.parent().ok_or("no package directory")?.to_path_buf();
    crate::strip::build(&m)?;
    let problems = crate::strip::type_problems(&m);
    if !problems.is_empty() {
        let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
        return Err(lines.join("\n"));
    }
    let v = checked(&m)?;
    ok_or_error(&v)?;
    let id = v["id"].as_str().unwrap_or_default().to_owned();
    // A package with fixtures must draw from them.
    if dir.join("fixtures").is_dir() {
        preview_text(&m, None).map_err(|e| format!("the preview failed: {e}"))?;
    }
    let name = name.unwrap_or_else(|| id.clone());
    valid_name(&name)?;
    let apps = apps()?;
    let dest = apps.join(&name);
    if m.file_name().and_then(|n| n.to_str()) != Some("app.toml") {
        return Err("install a package directory, whose manifest is app.toml".into());
    }
    let staging = apps.join(format!(".staging-{name}"));
    let _ = std::fs::remove_dir_all(&staging);
    copy_dir(&dir, &staging)?;
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if dest.exists() {
        let last = apps.join(".last-good");
        std::fs::create_dir_all(&last).map_err(|e| fail(&last, e))?;
        let keep = last.join(&name);
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::rename(&dest, &keep).map_err(|e| fail(&dest, e))?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| fail(&staging, e))?;
    println!(
        "installed {id} {} as {} ({})",
        v["version"].as_str().unwrap_or(""),
        name,
        dest.display()
    );
    crate::env::autosave(
        &[&format!("apps/{name}")],
        &format!("Install app {name} {}", v["version"].as_str().unwrap_or("")),
    );
    Ok(())
}

pub fn rollback(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err(USAGE.into());
    };
    valid_name(name)?;
    let apps = apps()?;
    let (dest, keep) = (apps.join(name), apps.join(".last-good").join(name));
    if !keep.is_dir() {
        return Err(format!("{name} has no last good version"));
    }
    ok_or_error(&checked(&keep.join("app.toml"))?)?;
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let swap = apps.join(format!(".staging-{name}"));
    let _ = std::fs::remove_dir_all(&swap);
    if dest.exists() {
        std::fs::rename(&dest, &swap).map_err(|e| fail(&dest, e))?;
    }
    std::fs::rename(&keep, &dest).map_err(|e| fail(&keep, e))?;
    // The version rolled back from becomes the one to return to.
    if swap.exists() {
        std::fs::rename(&swap, &keep).map_err(|e| fail(&swap, e))?;
    }
    println!("rolled {name} back ({})", dest.display());
    crate::env::autosave(&[&format!("apps/{name}")], &format!("Roll back app {name}"));
    Ok(())
}

pub fn list() -> Result<(), String> {
    let apps = apps()?;
    let mut names: Vec<String> = std::fs::read_dir(&apps)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && apps.join(n).join("app.toml").is_file())
        .collect();
    names.sort();
    if names.is_empty() {
        println!("no packages in {}", apps.display());
    }
    for n in names {
        let v = checked(&apps.join(&n).join("app.toml"))?;
        let last = if apps.join(".last-good").join(&n).is_dir() {
            "  (last good kept)"
        } else {
            ""
        };
        match ok_or_error(&v) {
            Ok(()) => println!(
                "{n}  {} {}  {}{last}",
                v["id"].as_str().unwrap_or(""),
                v["version"].as_str().unwrap_or(""),
                v["title"].as_str().unwrap_or("")
            ),
            Err(e) => println!("{n}  invalid: {e}"),
        }
    }
    Ok(())
}

/// The `apps.launch` request that runs the installed package `name` with
/// the grant it declares: in a new window of session 0 until the caller
/// says where (`session`, or `split` to open it beside a pane).
pub(crate) fn launch_request(name: &str) -> Result<api::LaunchApp, String> {
    valid_name(name)?;
    let m = apps()?.join(name).join("app.toml");
    if !m.is_file() {
        return Err(format!("{name} is not installed (see `dream app list`)"));
    }
    let v = checked(&m)?;
    ok_or_error(&v)?;
    let n = &v["needs"];
    let grant = api::Grant {
        methods: n["methods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        all_panes: n["all_panes"] == Value::Bool(true),
        panes: n["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter_map(|p| u32::try_from(p).ok())
            .collect(),
        ..api::Grant::default()
    };
    Ok(api::LaunchApp {
        app_id: v["id"].as_str().unwrap_or(name).to_owned(),
        grant: Some(grant),
        argv: vec![termapp().display().to_string(), m.display().to_string()],
        name: Some(name.to_owned()),
        ..api::LaunchApp::default()
    })
}

/// The session of `pane`, else the first session.
pub(crate) async fn session_of(c: &client::Client, pane: Option<u32>) -> Result<u32, String> {
    let api::Response::TmuxTopology(t) = c
        .call(api::Request::TmuxTopology(api::TopologyParams::default()))
        .await
        .map_err(crate::daemon::explain)?
    else {
        return Err("unexpected reply".into());
    };
    let window = pane.and_then(|p| t.panes.iter().find(|x| x.id == p).map(|x| x.window));
    window
        .and_then(|w| t.windows.iter().find(|x| x.id == w))
        .and_then(|w| w.sessions.first().copied())
        .or_else(|| t.sessions.first().map(|s| s.id))
        .ok_or_else(|| "tmux has no sessions".into())
}

/// `dream app launch NAME [--session $N | --beside %N] [--request]`.
pub fn launch(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let [name, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let (mut session_id, mut beside, mut request) = (None, None, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--session" => {
                let v = value()?;
                session_id = Some(
                    v.trim_start_matches('$')
                        .parse::<u32>()
                        .map_err(|_| format!("`{v}` is not a session id"))?,
                );
            }
            "--beside" => {
                let v = value()?;
                beside = Some(
                    v.trim_start_matches('%')
                        .parse::<u32>()
                        .map_err(|_| format!("`{v}` is not a pane (%N)"))?,
                );
            }
            "--request" => request = true,
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    let mut r = launch_request(name)?;
    r.split = beside;
    if request {
        // For a client that launches on a connection of its own (Dreamer's
        // pi extension): it says where.
        println!("{}", serde_json::to_string(&r).map_err(|e| e.to_string())?);
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let c = crate::remote::session(d).await?;
        r.session = match session_id {
            Some(s) => s,
            None => session_of(&c, beside).await?,
        };
        let api::Response::AppsLaunch(l) = c
            .call(api::Request::AppsLaunch(r.clone()))
            .await
            .map_err(crate::daemon::explain)?
        else {
            return Err("unexpected reply".into());
        };
        println!(
            "launched {} in pane %{} (launch {})",
            r.app_id, l.pane, l.launch_id
        );
        Ok(())
    })
}
