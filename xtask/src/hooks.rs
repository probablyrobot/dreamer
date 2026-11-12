//! `cargo xtask hooks install`: point git at the versioned hooks in `.githooks/`.

use std::process::Command;

use crate::util::repo_root;

const HOOKS_PATH: &str = ".githooks";

pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("install") => install(),
        Some("status") | None => status(),
        Some(other) => Err(format!("unknown hooks command `{other}` (install, status)")),
    }
}

fn current() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--local", "--get", "core.hooksPath"])
        .current_dir(repo_root())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn install() -> Result<(), String> {
    let root = repo_root();
    for hook in ["pre-commit", "pre-push"] {
        let path = root.join(HOOKS_PATH).join(hook);
        if !path.is_file() {
            return Err(format!("missing hook script {}", path.display()));
        }
        make_executable(&path)?;
    }
    let status = Command::new("git")
        .args(["config", "--local", "core.hooksPath", HOOKS_PATH])
        .current_dir(&root)
        .status()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !status.success() {
        return Err("git config core.hooksPath failed".into());
    }
    println!("hooks installed: core.hooksPath = {HOOKS_PATH} (pre-commit, pre-push)");
    Ok(())
}

fn status() -> Result<(), String> {
    match current() {
        Some(path) if path == HOOKS_PATH => {
            println!("hooks installed ({HOOKS_PATH})");
            Ok(())
        }
        Some(other) => Err(format!(
            "core.hooksPath is `{other}`, not `{HOOKS_PATH}`; run `cargo xtask hooks install`"
        )),
        None => Err("hooks not installed; run `cargo xtask hooks install`".into()),
    }
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_: &std::path::Path) -> Result<(), String> {
    Ok(())
}
