//! `cargo xtask gate <mac|linux> [--staged|--full]`: the local verification gate.
//!
//! `--staged` (pre-commit) tests exactly what is staged: the index is written
//! to a tree and checked out into a persistent gate worktree with its own warm
//! target directory, so unstaged edits can neither break nor rescue a commit.
//! `--full` (pre-push) tests `HEAD` and runs every stage regardless of paths.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::util::repo_root;

/// Environment variables git sets for hooks. They must not leak into git
/// commands aimed at the gate worktree, or those commands would act on the
/// main repository's index instead.
const GIT_HOOK_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_PREFIX",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Mac,
    Linux,
}

impl Platform {
    fn name(self) -> &'static str {
        match self {
            Platform::Mac => "mac",
            Platform::Linux => "linux",
        }
    }

    fn host() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => Some(Platform::Mac),
            "linux" => Some(Platform::Linux),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Staged,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Only documentation changed: skip build and test stages.
    Docs,
    /// Anything else: run every stage.
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    Skipped,
    /// The stage could not run on this machine; recorded, not a pass.
    NotRun,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
            Outcome::NotRun => "not_run",
        }
    }
}

/// When a stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    /// Every gate run, even for documentation-only changes.
    Always,
    /// Any change that is not documentation only.
    Code,
    /// Only `--full` (pre-push).
    Full,
}

/// What a built-in stage reports when it does not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    Passed,
    NotRun,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Cmd(&'static str, &'static [&'static str]),
    Builtin(fn(&Path) -> Result<Done, String>),
}

#[derive(Debug)]
struct Stage {
    name: &'static str,
    action: Action,
    when: When,
}

#[derive(Debug)]
struct StageResult {
    name: &'static str,
    command: String,
    outcome: Outcome,
    millis: u128,
}

fn stages(platform: Platform) -> Vec<Stage> {
    use Action::{Builtin, Cmd};
    let mut stages = vec![
        Stage {
            name: "policy",
            action: Cmd("cargo", &["xtask", "policy"]),
            when: When::Always,
        },
        Stage {
            name: "boundary",
            action: Cmd("cargo", &["xtask", "headless", "--check"]),
            when: When::Code,
        },
        Stage {
            name: "deps",
            action: Cmd("cargo", &["xtask", "deps", "prepare", "--locked"]),
            when: When::Code,
        },
        Stage {
            name: "bindings",
            action: Cmd("cargo", &["xtask", "bindings", "--check"]),
            when: When::Code,
        },
        Stage {
            name: "fmt",
            action: Cmd("cargo", &["fmt", "--all", "--check"]),
            when: When::Always,
        },
        Stage {
            name: "clippy",
            action: Cmd(
                "cargo",
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            when: When::Code,
        },
        Stage {
            name: "test",
            action: Cmd("cargo", &["test", "--workspace", "--locked"]),
            when: When::Code,
        },
        Stage {
            name: "headless",
            action: Builtin(headless_build),
            when: When::Code,
        },
        Stage {
            name: "gui",
            action: Cmd("cargo", &["build", "--locked", "--package", "dreamer-app"]),
            when: When::Code,
        },
    ];
    if platform == Platform::Mac {
        stages.push(Stage {
            name: "bundle",
            action: Builtin(bundle_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "package",
            action: Builtin(package_away_from_tree),
            when: When::Full,
        });
    }
    stages.push(Stage {
        name: "arm64",
        action: Builtin(arm64_container),
        when: When::Full,
    });
    stages
}

/// Build the headless packages alone, so a GUI-only dependency cannot hide
/// behind the workspace build.
fn headless_build(dir: &Path) -> Result<Done, String> {
    let mut cmd = clean_git_env(Command::new("cargo"));
    cmd.args(["build", "--locked"]).current_dir(dir);
    for p in crate::headless::PACKAGES {
        cmd.args(["--package", p]);
    }
    let status = cmd.status().map_err(|e| format!("cargo build: {e}"))?;
    if status.success() {
        Ok(Done::Passed)
    } else {
        Err("headless build failed".into())
    }
}

/// The launch smoke needs a logged-in GUI session. Without one the stage
/// fails unless `DREAMER_GATE_ALLOW_NO_GUI=1` is set for this run, in which
/// case it is recorded as not run.
fn require_gui() -> Result<bool, String> {
    if crate::package::gui_session_available() {
        return Ok(true);
    }
    if std::env::var_os("DREAMER_GATE_ALLOW_NO_GUI").is_some() {
        println!("gate: no GUI session; launch smoke recorded as not run");
        return Ok(false);
    }
    Err(
        "no logged-in GUI session for the launch smoke; run the gate from the desktop session \
         or set DREAMER_GATE_ALLOW_NO_GUI=1 to record it as not run"
            .into(),
    )
}

/// Debug bundle, ad hoc signed and verified, launched to a presented frame.
fn bundle_smoke(dir: &Path) -> Result<Done, String> {
    use crate::package::{Profile, build_bundle, launch_smoke, verify_bundle};
    let out = dir.join("target").join("gate-bundle");
    let app = build_bundle(dir, Profile::Debug, &out)?;
    verify_bundle(&app)?;
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let output = launch_smoke(&app)?;
    for line in output.lines() {
        println!("gate:   {line}");
    }
    Ok(Done::Passed)
}

/// Release bundle and zip, unpacked and launched outside the source tree.
fn package_away_from_tree(dir: &Path) -> Result<Done, String> {
    use crate::package::{Profile, build_bundle, smoke_from_zip, verify_bundle, zip_bundle};
    let out = dir.join("target").join("package");
    let app = build_bundle(dir, Profile::Release, &out)?;
    verify_bundle(&app)?;
    let zip = zip_bundle(&app, &out)?;
    println!("gate:   {}", zip.display());
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let output = smoke_from_zip(&zip)?;
    for line in output.lines() {
        println!("gate:   {line}");
    }
    Ok(Done::Passed)
}

/// Native ARM64 Linux tests in a container when a container runtime is
/// running; otherwise recorded as not run (spec 17.4).
fn arm64_container(dir: &Path) -> Result<Done, String> {
    // `docker info` exits 0 even when Docker Desktop failed to start, so
    // require an actual server version.
    let docker_up = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty());
    if !docker_up {
        println!("gate: no container runtime running; ARM64 Linux suite recorded as not run");
        return Ok(Done::NotRun);
    }
    let script = "set -e; export DEBIAN_FRONTEND=noninteractive; \
        apt-get update -qq && apt-get install -y -qq --no-install-recommends \
          build-essential pkg-config curl ca-certificates xz-utils git libclang-dev \
          libevent-dev libncurses-dev bison python3 >/dev/null; \
        command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
        . \"$HOME/.cargo/env\"; \
        cargo xtask deps prepare --locked; \
        export DREAMER_TMUX=$(cargo xtask deps tmux | tail -n 1); \
        cargo test --locked $(cargo xtask headless --cargo-args)";
    let status = Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/arm64"])
        .arg("-v")
        .arg(format!("{}:/src", dir.display()))
        .args([
            "-v",
            "dreamer-arm64-cargo:/root/.cargo",
            "-v",
            "dreamer-arm64-rustup:/root/.rustup",
            "-v",
            "dreamer-arm64-cache:/root/.cache",
            "-e",
            "CARGO_TARGET_DIR=/root/.cache/dreamer-target",
            "-w",
            "/src",
            "ubuntu:24.04",
            "bash",
            "-c",
            script,
        ])
        .status()
        .map_err(|e| format!("docker run: {e}"))?;
    if status.success() {
        Ok(Done::Passed)
    } else {
        Err("ARM64 Linux container suite failed".into())
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut platform = None;
    let mut mode = Mode::Staged;
    for arg in args {
        match arg.as_str() {
            "mac" => platform = Some(Platform::Mac),
            "linux" => platform = Some(Platform::Linux),
            "--staged" => mode = Mode::Staged,
            "--full" => mode = Mode::Full,
            other => return Err(format!("unknown gate argument `{other}`")),
        }
    }
    let host = Platform::host().ok_or("the gate runs on macOS or Linux only")?;
    let platform = platform.unwrap_or(host);
    if platform != host {
        return Err(format!(
            "cannot run the {} gate on this {} host",
            platform.name(),
            host.name()
        ));
    }

    let root = repo_root();
    let tree = match mode {
        // Uses the hook's own GIT_INDEX_FILE, so partial and `-a` commits are
        // tested exactly as they will be recorded.
        Mode::Staged => git_inherited(&root, &["write-tree"])?,
        Mode::Full => git_clean(&root, &["rev-parse", "HEAD^{tree}"])?,
    };
    let scope = match mode {
        Mode::Full => Scope::Code,
        Mode::Staged => {
            let changed = git_inherited(&root, &["diff", "--cached", "--name-only", "-z"])?;
            scope_for(changed.split('\0').filter(|p| !p.is_empty()))
        }
    };

    let worktree = prepare_worktree(&root, &tree)?;
    println!(
        "gate {} {}: tree {} ({})",
        platform.name(),
        if mode == Mode::Staged {
            "--staged"
        } else {
            "--full"
        },
        &tree[..12.min(tree.len())],
        if scope == Scope::Docs {
            "documentation only"
        } else {
            "code"
        },
    );

    let mut results = Vec::new();
    let mut failed = None;
    for stage in stages(platform) {
        let command = match stage.action {
            Action::Cmd(program, args) => format!("{program} {}", args.join(" ")),
            Action::Builtin(_) => format!("cargo xtask gate stage `{}`", stage.name),
        };
        let wanted = match stage.when {
            When::Always => true,
            When::Code => scope == Scope::Code,
            When::Full => mode == Mode::Full,
        };
        let skip = failed.is_some() || !wanted;
        if skip {
            results.push(StageResult {
                name: stage.name,
                command,
                outcome: Outcome::Skipped,
                millis: 0,
            });
            continue;
        }
        let started = Instant::now();
        let outcome = match stage.action {
            Action::Cmd(program, args) => {
                let status = clean_git_env(Command::new(program))
                    .args(args)
                    .current_dir(&worktree)
                    .status()
                    .map_err(|e| format!("could not start `{command}`: {e}"))?;
                if status.success() {
                    Outcome::Passed
                } else {
                    Outcome::Failed
                }
            }
            Action::Builtin(f) => match f(&worktree) {
                Ok(Done::Passed) => Outcome::Passed,
                Ok(Done::NotRun) => Outcome::NotRun,
                Err(e) => {
                    eprintln!("gate: {}: {e}", stage.name);
                    Outcome::Failed
                }
            },
        };
        let millis = started.elapsed().as_millis();
        if outcome == Outcome::Failed {
            failed = Some(stage.name);
        }
        println!(
            "gate: {:<8} {:<7} {:>6} ms",
            stage.name,
            outcome.as_str(),
            millis
        );
        results.push(StageResult {
            name: stage.name,
            command,
            outcome,
            millis,
        });
    }

    let report = write_report(&root, platform, mode, &tree, scope, &results)?;
    println!("gate: report {}", report.display());

    match failed {
        None => Ok(()),
        Some(name) => {
            let command = results
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.command.as_str())
                .unwrap_or_default();
            Err(format!(
                "gate stage `{name}` failed.\n  reproduce: cd {} && {command}\n  \
                 fix the staged change; do not bypass the hook with --no-verify",
                worktree.display()
            ))
        }
    }
}

/// Classify changed paths. Anything that is not plainly documentation counts
/// as code, so an unknown file type never skips the build.
fn scope_for<'a>(paths: impl IntoIterator<Item = &'a str>) -> Scope {
    let is_doc = |p: &str| {
        p.starts_with("docs/")
            || p.ends_with(".md")
            || matches!(p, "LICENSE" | "NOTICE" | ".gitignore")
    };
    let mut any = false;
    for path in paths {
        any = true;
        if !is_doc(path) {
            return Scope::Code;
        }
    }
    if any { Scope::Docs } else { Scope::Code }
}

/// Check the tree out into `target/gate/worktree`, creating the worktree on
/// first use. Ignored files (its `target/` directory) survive between runs.
fn prepare_worktree(root: &Path, tree: &str) -> Result<PathBuf, String> {
    let dir = root.join("target").join("gate").join("worktree");
    if !dir.join(".git").exists() {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
        }
        // A stale registration (e.g. after `cargo clean`) would block `add`.
        git_clean(root, &["worktree", "prune"])?;
        let dir_str = dir.to_str().ok_or("non-UTF-8 repository path")?;
        git_clean(
            root,
            &["worktree", "add", "--detach", "--quiet", dir_str, "HEAD"],
        )?;
    }
    git_clean(&dir, &["read-tree", "-u", "--reset", tree])?;
    Ok(dir)
}

fn write_report(
    root: &Path,
    platform: Platform,
    mode: Mode,
    tree: &str,
    scope: Scope,
    results: &[StageResult],
) -> Result<PathBuf, String> {
    let dir = root.join("target").join("gate").join(platform.name());
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mode_name = if mode == Mode::Staged {
        "staged"
    } else {
        "full"
    };
    let path = dir.join(format!("{tree}-{mode_name}.json"));

    let overall = if results.iter().any(|r| r.outcome == Outcome::Failed) {
        "failed"
    } else {
        "passed"
    };
    let mut json = String::new();
    let _ = write!(
        json,
        "{{\n  \"tree\": \"{tree}\",\n  \"platform\": \"{}\",\n  \"mode\": \"{}\",\n  \
         \"scope\": \"{}\",\n  \"result\": \"{overall}\",\n  \"host\": \"{}-{}\",\n  \
         \"unix_time\": {},\n  \"stages\": [",
        platform.name(),
        if mode == Mode::Staged {
            "staged"
        } else {
            "full"
        },
        if scope == Scope::Docs { "docs" } else { "code" },
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    for (i, r) in results.iter().enumerate() {
        let _ = write!(
            json,
            "{}\n    {{ \"name\": \"{}\", \"result\": \"{}\", \"millis\": {}, \"command\": \"{}\" }}",
            if i == 0 { "" } else { "," },
            r.name,
            r.outcome.as_str(),
            r.millis,
            json_escape(&r.command),
        );
    }
    json.push_str("\n  ]\n}\n");
    std::fs::write(&path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn clean_git_env(mut cmd: Command) -> Command {
    for var in GIT_HOOK_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn git_output(mut cmd: Command, dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = cmd
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Git with the caller's environment (the hook's index and repository).
fn git_inherited(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_output(Command::new("git"), dir, args)
}

/// Git with hook variables removed, resolving the repository from `dir`.
fn git_clean(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_output(clean_git_env(Command::new("git")), dir, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documentation_only_changes_skip_builds() {
        assert_eq!(scope_for(["README.md", "docs/adr/0001-x.md"]), Scope::Docs);
        assert_eq!(scope_for(["LICENSE", "NOTICE"]), Scope::Docs);
    }

    #[test]
    fn any_code_or_unknown_path_runs_everything() {
        assert_eq!(scope_for(["README.md", "xtask/src/main.rs"]), Scope::Code);
        assert_eq!(scope_for(["Cargo.lock"]), Scope::Code);
        assert_eq!(scope_for([".githooks/pre-commit"]), Scope::Code);
        assert_eq!(scope_for(["docs.rs"]), Scope::Code);
    }

    #[test]
    fn an_empty_change_set_is_treated_as_code() {
        assert_eq!(scope_for(std::iter::empty()), Scope::Code);
    }

    #[test]
    fn json_escaping_handles_quotes_and_controls() {
        assert_eq!(json_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
        assert_eq!(json_escape("x\ny"), "x\\u000ay");
    }
}
