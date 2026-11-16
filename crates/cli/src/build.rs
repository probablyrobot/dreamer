//! The small-app builder's steps (spec 13.3), for people and agents alike:
//! check a package, preview it from its fixtures, install it (keeping the
//! version it replaces as the last good one), roll back, list, and launch
//! it in a real tmux pane with exactly the grant it declares.
//!
//! Checking and drawing are `dreamer-termapp`'s (`--check --json`,
//! `--preview`); this only runs it and moves files.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::daemon::DaemonArgs;

pub const USAGE: &str = "usage:
  dream app check PATH [--json]           validate a package (a directory or its app.toml)
  dream app preview PATH [--size WxH]     draw it once from fixtures/<source>.json, no daemon
  dream app install PATH [--name NAME]    check, preview, and install; the replaced version is kept
  dream app rollback NAME                 put the last good version back
  dream app list                          installed packages
  dream app launch NAME [--session $N]    run it in a new tmux window with the grant it declares
  dream app run APP_ID [--method M]... [--all-panes] [--session $N] -- PROGRAM [ARGS...]";

/// `dreamer-termapp` beside this program, else on `PATH`.
fn termapp() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("dreamer-termapp")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("dreamer-termapp"))
}

/// A package's manifest: `PATH/app.toml` for a directory.
fn manifest(path: &str) -> PathBuf {
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
fn checked(manifest: &Path) -> Result<Value, String> {
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

fn ok_or_error(v: &Value) -> Result<(), String> {
    if v["ok"] == Value::Bool(true) {
        Ok(())
    } else {
        Err(v["error"].as_str().unwrap_or("invalid package").to_owned())
    }
}

fn preview_text(manifest: &Path, size: Option<&str>) -> Result<String, String> {
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
    let v = checked(&manifest(path))?;
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

pub fn preview(args: &[String]) -> Result<(), String> {
    let (path, size) = match args {
        [p] => (p, None),
        [p, f, s] if f == "--size" => (p, Some(s.as_str())),
        _ => return Err(USAGE.into()),
    };
    print!("{}", preview_text(&manifest(path), size)?);
    Ok(())
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

pub fn launch(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let [name, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    valid_name(name)?;
    let m = apps()?.join(name).join("app.toml");
    if !m.is_file() {
        return Err(format!("{name} is not installed (see `dream app list`)"));
    }
    let v = checked(&m)?;
    ok_or_error(&v)?;
    let n = &v["needs"];
    let mut run: Vec<String> = vec![v["id"].as_str().unwrap_or(name).to_owned()];
    for method in n["methods"].as_array().into_iter().flatten() {
        run.extend(["--method".into(), method.as_str().unwrap_or("").to_owned()]);
    }
    if n["all_panes"] == Value::Bool(true) {
        run.push("--all-panes".into());
    }
    for p in n["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
    {
        run.extend(["--pane".into(), format!("%{p}")]);
    }
    run.extend(["--name".into(), name.clone()]);
    run.extend(rest.iter().cloned());
    run.push("--".into());
    run.push(termapp().display().to_string());
    run.push(m.display().to_string());
    crate::app::run(d, &run)
}
