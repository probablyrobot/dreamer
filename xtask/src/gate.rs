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
            name: "api",
            action: Cmd("cargo", &["xtask", "api", "--check"]),
            when: When::Code,
        },
        Stage {
            name: "schemes",
            action: Cmd("cargo", &["xtask", "schemes", "--check"]),
            when: When::Code,
        },
        Stage {
            name: "icons",
            action: Cmd("cargo", &["xtask", "icons", "--check"]),
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
            name: "matrix",
            action: Builtin(render_matrix),
            when: When::Code,
        });
        stages.push(Stage {
            name: "cache",
            action: Builtin(shaping_cache),
            when: When::Code,
        });
        stages.push(Stage {
            name: "live",
            action: Builtin(live_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "input",
            action: Builtin(input_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "workspace",
            action: Builtin(workspace_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "remote",
            action: Builtin(remote_smoke),
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

/// Checks that need the display to keep refreshing (repaints, idle paint
/// counts) are recorded as not run while the screen is locked.
fn require_refreshing_display() -> Result<bool, String> {
    if !require_gui()? {
        return Ok(false);
    }
    match crate::package::screen_locked() {
        Some(true) => {
            println!(
                "gate: the screen is locked, so windows do not repaint; recorded as not run \
                 (unlock and rerun for evidence)"
            );
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Debug bundle, ad hoc signed and verified, launched to a presented frame.
/// Every renderer fixture paints with the statistics derived from its
/// content (spec 18.3 text matrix). First paints happen even on a locked
/// screen, so this runs whenever a GUI session exists.
fn render_matrix(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let app = dir.join("target/debug/dreamer");
    let mut failures = Vec::new();
    for (path, expected) in crate::render::expectations(dir)? {
        let out = Command::new(&app)
            .args(["--smoke", "--fixture"])
            .arg(&path)
            .output()
            .map_err(|e| format!("dreamer: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        match crate::render::check(&stdout, &expected) {
            Ok(()) if out.status.success() => println!("gate:   {:<22} as expected", expected.name),
            Ok(()) => failures.push(format!("{}: exit {}", expected.name, out.status)),
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        Ok(Done::Passed)
    } else {
        Err(failures.join("\n"))
    }
}

/// Painting an unchanged frame again reshapes nothing. Needs a display that
/// keeps refreshing.
fn shaping_cache(dir: &Path) -> Result<Done, String> {
    if !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    let out = Command::new(dir.join("target/debug/dreamer"))
        .arg("--smoke-repaint")
        .output()
        .map_err(|e| format!("dreamer: {e}"))?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    match out.status.code() {
        Some(0) => Ok(Done::Passed),
        Some(5) => Ok(Done::NotRun),
        _ => Err(format!(
            "the repaint reshaped rows or failed ({})",
            out.status
        )),
    }
}

/// The desktop app showing a live tmux pane through dreamerd: an isolated
/// tmux server and private daemon directories; the pane's text must be
/// drawn and an idle pane must cause no view wakeups or paints.
fn live_smoke(dir: &Path) -> Result<Done, String> {
    if !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    let marker = "dreamer-live-ok";
    isolated_app_run(
        dir,
        "live",
        &format!("sh -c 'printf \"{marker}\\n\"; exec sleep 600'"),
        None,
        &["--smoke", "--expect", marker],
    )
}

/// Typing and IME composition through the window reach the pane (only
/// committed text; preedit is never sent). Needs a GUI session but not a
/// refreshing display: keys are dispatched, frames arrive from the feed.
fn input_smoke(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let out = input_smoke_output(dir, &[])?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    if out.status.success() {
        Ok(Done::Passed)
    } else {
        Err(format!(
            "input smoke failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run `dreamer --attach 0 <args>` against an isolated tmux server whose
/// pane runs `command` (optionally after typing `type_first` into it),
/// with private daemon directories and the default-tmux guard.
fn isolated_app_run(
    dir: &Path,
    name: &str,
    command: &str,
    type_first: Option<&str>,
    args: &[&str],
) -> Result<Done, String> {
    let out = isolated_app_output(dir, name, command, type_first, &[], args, &[], &[])?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    if out.status.code() == Some(5) {
        println!("gate: the display did not refresh; {name} paint checks recorded as not run");
        return Ok(Done::NotRun);
    }
    if out.status.success() {
        Ok(Done::Passed)
    } else {
        Err(format!(
            "{name} smoke failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The input smoke: typing, IME composition, click, and wheel through the
/// window must reach a shell in the pane.
pub fn input_smoke_output(dir: &Path, env_remove: &[&str]) -> Result<std::process::Output, String> {
    isolated_app_output(
        dir,
        "input",
        "env PS1='$ ' /bin/sh",
        Some("echo READY-FOR-INPUT"),
        &[],
        &["--smoke-input", "--expect", "READY-FOR-INPUT"],
        env_remove,
        &[],
    )
}

/// The workspace smoke: session `main` with two windows, the first split
/// into three panes, every pane having printed `READY-%<id>`. Needs a
/// refreshing display for its click and keyboard part (exit 5 otherwise).
pub fn workspace_smoke_output(
    dir: &Path,
    env_remove: &[&str],
    state: &Path,
) -> Result<std::process::Output, String> {
    const SH: &str = "env PS1='$ ' /bin/sh";
    const READY: &str = "echo READY-$TMUX_PANE";
    isolated_app_output(
        dir,
        "workspace",
        SH,
        None,
        &[
            &["split-window", "-t", "main:0", SH],
            &["split-window", "-h", "-t", "main:0", SH],
            &["new-window", "-t", "main", SH],
            &["send-keys", "-t", "%0", READY, "Enter"],
            &["send-keys", "-t", "%1", READY, "Enter"],
            &["send-keys", "-t", "%2", READY, "Enter"],
            &["send-keys", "-t", "%3", READY, "Enter"],
            &["select-window", "-t", "main:0"],
            &["new-session", "-d", "-s", "other", SH],
        ],
        &["--smoke-workspace"],
        env_remove,
        &[("DREAMER_DESKTOP_STATE_DIR", &state.to_string_lossy())],
    )
}

/// The workspace smoke's second launch, against a new tmux server (a new
/// incarnation): the arrangement saved in `state` must come back.
pub fn workspace_restore_output(
    dir: &Path,
    env_remove: &[&str],
    state: &Path,
) -> Result<std::process::Output, String> {
    const SH: &str = "env PS1='$ ' /bin/sh";
    isolated_app_output(
        dir,
        "workspace-restore",
        SH,
        None,
        // `decoy` takes the id `other` had on the first server: only a
        // restore by name, not by the stale id, lands on `other`.
        &[
            &["new-session", "-d", "-s", "decoy", SH],
            &["new-session", "-d", "-s", "other", SH],
        ],
        &["--smoke-workspace-restore"],
        env_remove,
        &[("DREAMER_DESKTOP_STATE_DIR", &state.to_string_lossy())],
    )
}

/// A private directory for the desktop state of one gate run.
pub fn desktop_state_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dreamer-gate-state-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Tabs and panes laid out from tmux, an external split, a tab switch,
/// and (with a refreshing display) click-to-focus and typing.
/// A host's session in a desktop window, through the local daemon's
/// coordinator: typing reaches the host, a disconnect marks the window
/// stale, a reconnect resumes it. Needs a GUI session but not a refreshing
/// display (text arrives from the feeds).
fn remote_smoke(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let out = remote_smoke_output(dir, &[])?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    if out.status.success() {
        Ok(Done::Passed)
    } else {
        Err(format!(
            "remote smoke failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run `dreamer --smoke-remote --host devbox`. The "host" is a second
/// isolated tmux server and daemon with private directories, reached only
/// through a stand-in `ssh` that runs the remote command in a shell, as
/// sshd does, and a `hosts.yaml` profile in a private configuration
/// directory. Nothing touches the user's tmux, SSH, or settings.
pub fn remote_smoke_output(
    dir: &Path,
    env_remove: &[&str],
) -> Result<std::process::Output, String> {
    let build = clean_git_env(Command::new("cargo"))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
            "-p",
            "dreamer-cli",
        ])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer, dreamerd, and dream failed".into());
    }
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let id = std::process::id();
    let (near, far) = (
        format!("dreamer-gate-near-{id}"),
        format!("dreamer-gate-far-{id}"),
    );
    let root = PathBuf::from(format!("/tmp/drm-gate-remote-{id}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).map_err(|e| e.to_string())?;
    for (socket, ready) in [(&near, "echo READY-LOCAL"), (&far, "echo READY-REMOTE")] {
        let started = Command::new(&tmux)
            .args([
                "-L",
                socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "main",
            ])
            .args(["-x", "100", "-y", "30", "env PS1='$ ' /bin/sh"])
            .status()
            .map_err(|e| format!("{tmux}: {e}"))?;
        if !started.success() {
            return Err("could not start an isolated tmux server".into());
        }
        let _ = Command::new(&tmux)
            .args(["-L", socket, "send-keys", "-t", "main", ready, "Enter"])
            .status();
    }
    let bin = dir.join("target/debug");
    let ssh = root.join("ssh");
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\n\
             export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}'\n\
             export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'\n\
             exec /bin/sh -c \"exec $3\"\n",
            rt = root.join("far/rt").display(),
            st = root.join("far/st").display(),
            daemon = bin.join("dreamerd").display(),
        ),
    )
    .map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let tmux_path = which(&tmux).unwrap_or_else(|| PathBuf::from(&tmux));
    std::fs::write(
        root.join("config/hosts.yaml"),
        format!(
            "hosts:\n  - name: devbox\n    destination: devbox\n    dream: '{}'\n    tmux: '{}'\n    tmux_socket: '{far}'\n",
            bin.join("dream").display(),
            tmux_path.display(),
        ),
    )
    .map_err(|e| e.to_string())?;
    let mut app = Command::new(bin.join("dreamer"));
    for var in env_remove {
        app.env_remove(var);
    }
    let out = app
        .args([
            "--smoke-remote",
            "--host",
            "devbox",
            "-L",
            &near,
            "--tmux",
            &tmux,
        ])
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_DESKTOP_STATE_DIR", root.join("desktop"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .env("DREAMER_SSH", &ssh)
        .env("DREAMER_TEST_BACKOFF_MS", "200")
        .output();
    for lock in ["rt/dreamerd.lock", "far/rt/dreamerd.lock"] {
        if let Some(pid) = std::fs::read_to_string(root.join(lock))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
    for socket in [&near, &far] {
        let _ = Command::new(&tmux)
            .args(["-L", socket, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = std::fs::remove_dir_all(&root);
    out.map_err(|e| format!("dreamer: {e}"))
}

/// `program` resolved on `PATH` (the remote side gets an absolute path).
fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    })
}

fn workspace_smoke(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let state = desktop_state_dir("mac");
    let out = workspace_smoke_output(dir, &[], &state)?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    let first = match out.status.code() {
        Some(0) => Done::Passed,
        Some(5) => {
            println!(
                "gate: the display did not refresh; workspace sizing, click, keyboard, and drag checks recorded as not run"
            );
            Done::NotRun
        }
        _ => {
            let _ = std::fs::remove_dir_all(&state);
            return Err(format!(
                "workspace smoke failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    };
    // A second launch on a new tmux server restores the arrangement.
    let again = workspace_restore_output(dir, &[], &state);
    let _ = std::fs::remove_dir_all(&state);
    let again = again?;
    for line in String::from_utf8_lossy(&again.stdout).lines() {
        println!("gate:   {line}");
    }
    if !again.status.success() {
        return Err(format!(
            "workspace restore smoke failed ({}): {}",
            again.status,
            String::from_utf8_lossy(&again.stderr).trim()
        ));
    }
    Ok(first)
}

/// Like [`isolated_app_run`], returning the app's output; `env_remove`
/// names variables the app must not see (to pick a display backend).
#[allow(clippy::too_many_arguments)]
fn isolated_app_output(
    dir: &Path,
    name: &str,
    command: &str,
    type_first: Option<&str>,
    setup: &[&[&str]],
    args: &[&str],
    env_remove: &[&str],
    env: &[(&str, &str)],
) -> Result<std::process::Output, String> {
    let build = clean_git_env(Command::new("cargo"))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
        ])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer and dreamerd failed".into());
    }
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let socket = format!("dreamer-gate-{name}-{}", std::process::id());
    let root = PathBuf::from(format!("/tmp/drm-gate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let started = Command::new(&tmux)
        .args([
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "100",
            "-y",
            "30",
        ])
        .arg(command)
        .status()
        .map_err(|e| format!("{tmux}: {e}"))?;
    if !started.success() {
        return Err("could not start an isolated tmux server".into());
    }
    for cmd in setup {
        let _ = Command::new(&tmux)
            .args(["-L", &socket])
            .args(*cmd)
            .status();
    }
    if let Some(line) = type_first {
        let _ = Command::new(&tmux)
            .args(["-L", &socket, "send-keys", "-t", "main", line, "Enter"])
            .status();
    }
    let mut app = Command::new(dir.join("target/debug/dreamer"));
    for var in env_remove {
        app.env_remove(var);
    }
    for (k, v) in env {
        app.env(k, v);
    }
    let out = app
        .args(args)
        .args(["--attach", "0", "-L", &socket, "--tmux", &tmux])
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output();
    // Clean up the daemon and the isolated server whatever happened.
    if let Some(pid) = std::fs::read_to_string(root.join("rt/dreamerd.lock"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = Command::new(&tmux)
        .args(["-L", &socket, "kill-server"])
        .status();
    let _ = std::fs::remove_dir_all(&root);
    out.map_err(|e| format!("dreamer: {e}"))
}

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
    // `ulimit -c 0`: one test makes dreamerd abort on purpose, and the
    // container would otherwise write its core into the mounted tree.
    let script = "set -e; ulimit -c 0; export DEBIAN_FRONTEND=noninteractive; \
        apt-get update -qq && apt-get install -y -qq --no-install-recommends \
          build-essential pkg-config curl ca-certificates xz-utils git libclang-dev \
          libevent-dev libncurses-dev bison python3 >/dev/null; \
        command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
        . \"$HOME/.cargo/env\"; \
        cargo xtask deps prepare --locked; \
        export DREAMER_TMUX=$(cargo xtask deps tmux | tail -n 1); \
        cargo test --locked $(cargo xtask headless --cargo-args)";
    let mut docker = Command::new("docker");
    docker
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
        ]);
    // Kept when the suite fails, so a failure that does not reproduce on
    // a rerun can still be read.
    let log = crate::util::repo_root().join("target/gate/logs/arm64-last-failure.log");
    let ok = run_teed(docker, &log).map_err(|e| format!("docker run: {e}"))?;
    if ok {
        let _ = std::fs::remove_file(&log);
        Ok(Done::Passed)
    } else {
        Err(format!(
            "ARM64 Linux container suite failed; output saved to {}",
            log.display()
        ))
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
            // Built-in stages run only inside the gate itself.
            Action::Builtin(_) => format!(
                "cargo xtask gate --full   # built-in stage `{}`",
                stage.name
            ),
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
                let log = root.join("target/gate/logs").join(format!(
                    "{}-{}.log",
                    &tree[..12.min(tree.len())],
                    stage.name
                ));
                let mut cmd = clean_git_env(Command::new(program));
                cmd.args(args).current_dir(&worktree);
                let ok =
                    run_teed(cmd, &log).map_err(|e| format!("could not run `{command}`: {e}"))?;
                if ok {
                    let _ = std::fs::remove_file(&log);
                    Outcome::Passed
                } else {
                    println!("gate: {} output saved to {}", stage.name, log.display());
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

/// Run `cmd`, echoing its output as it arrives and keeping a copy in `log`
/// so a failure can be read after the fact. Returns whether it succeeded.
fn run_teed(mut cmd: Command, log: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = Arc::new(Mutex::new(std::fs::File::create(log)?));
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    fn pump<R: Read + Send + 'static>(
        mut from: R,
        to_stderr: bool,
        file: Arc<Mutex<std::fs::File>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = from.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if to_stderr {
                    let _ = std::io::stderr().write_all(&buf[..n]);
                } else {
                    let _ = std::io::stdout().write_all(&buf[..n]);
                }
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(&buf[..n]);
                }
            }
        })
    }
    let out = child.stdout.take().map(|o| pump(o, false, file.clone()));
    let err = child.stderr.take().map(|e| pump(e, true, file.clone()));
    let status = child.wait()?;
    for t in [out, err].into_iter().flatten() {
        let _ = t.join();
    }
    Ok(status.success())
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
