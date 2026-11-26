//! `cargo xtask gate <mac|linux> [--staged|--full]`: the local verification gate.
//!
//! `--staged` (pre-commit) tests exactly what is staged: the index is written
//! to a tree and checked out into a persistent gate worktree with its own warm
//! target directory, so unstaged edits can neither break nor rescue a commit.
//! `--full` (pre-push) tests `HEAD` and runs every stage regardless of paths,
//! except that stages a passing `--staged` run of the same tree passed are
//! reused when the gate worktree is still on that tree (see
//! [`reusable_stages`]).

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
    /// `--full` only: passed in a `--staged` run of the same tree.
    Reused,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
            Outcome::NotRun => "not_run",
            Outcome::Reused => "reused",
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
            name: "plugin",
            action: Cmd("cargo", &["xtask", "plugin", "--check"]),
            when: When::Always,
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
            name: "clipboard",
            action: Builtin(clipboard_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "screens",
            action: Builtin(screens),
            when: When::Code,
        });
        stages.push(Stage {
            name: "preview",
            action: Builtin(app_preview),
            when: When::Code,
        });
        stages.push(Stage {
            name: "remote",
            action: Builtin(remote_smoke),
            when: When::Code,
        });
        stages.push(Stage {
            name: "web",
            action: Builtin(web_panes),
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

/// Forces every pane a desktop run shows onto screen frames.
const FRAMES_ENV: &str = "DREAMER_RENDER_FRAMES";

/// Debug bundle, ad hoc signed and verified, launched to a presented frame.
/// Every renderer fixture paints with the statistics derived from its
/// content (text matrix), drawn from the engine and again from a
/// frame-backed view (`DREAMER_RENDER_FRAMES=always`). First paints happen
/// even on a locked screen, so this runs whenever a GUI session exists.
fn render_matrix(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let app = dir.join("target/debug/dreamer");
    let mut failures = Vec::new();
    for (path, expected) in crate::render::expectations(dir)? {
        for frames in [false, true] {
            let out = Command::new(&app)
                .env(FRAMES_ENV, if frames { "always" } else { "off" })
                .args(["--smoke", "--fixture"])
                .arg(&path)
                .output()
                .map_err(|e| format!("dreamer: {e}"))?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let how = if frames { "frames" } else { "engine" };
            let on_frames = stdout.contains("smoke: render frames Always");
            match crate::render::check(&stdout, &expected) {
                Ok(()) if frames != on_frames => failures.push(format!(
                    "{} ({how}): the run did not say it drew from frames",
                    expected.name
                )),
                Ok(()) if out.status.success() => {
                    println!("gate:   {:<22} as expected ({how})", expected.name)
                }
                Ok(()) => failures.push(format!("{} ({how}): exit {}", expected.name, out.status)),
                Err(e) => failures.push(format!("{e} ({how})")),
            }
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
/// drawn and an idle pane must cause no view wakeups and at most one stray
/// paint (see `idle_check` in the app).
///
/// The same check runs again with the pane on screen frames
/// (`DREAMER_RENDER_FRAMES=always`), which must paint what the replica
/// painted.
fn live_smoke(dir: &Path) -> Result<Done, String> {
    if !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    let marker = "dreamer-live-ok";
    let command = format!("sh -c 'printf \"{marker}\\n\"; exec sleep 600'");
    let args = ["--smoke", "--expect", marker];
    let mut painted = Vec::new();
    for (name, frames) in [("live", "off"), ("live-frames", "always")] {
        let env = [(FRAMES_ENV, frames)];
        let out = isolated_app_output(dir, name, &command, None, &[], &args, &[], &env)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            println!("gate:   {line}");
        }
        if out.status.code() == Some(5) {
            println!("gate: the display did not refresh; {name} paint checks recorded as not run");
            return Ok(Done::NotRun);
        }
        if !out.status.success() {
            return Err(format!(
                "{name} smoke failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if (frames == "always") != stdout.contains("smoke: render frames Always") {
            return Err(format!("{name}: the run did not follow the pane as asked"));
        }
        painted.push(crate::render::painted(&stdout, name)?);
    }
    let (records, frames) = (&painted[0], &painted[1]);
    let same = crate::render::Expected {
        name: records.name.clone(),
        ..frames.clone()
    };
    if same != *records {
        return Err(format!(
            "the pane on frames painted {frames:?}, its replica {records:?}"
        ));
    }
    Ok(Done::Passed)
}

/// Typing and IME composition through the window reach the pane (only
/// committed text; preedit is never sent). Needs a GUI session, and a
/// window that is being drawn: a step that stalls while the window does
/// not repaint when asked (behind the frontmost app, or the display
/// asleep) exits 5 and is recorded as not run.
fn input_smoke(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let out = input_smoke_output(dir, &[])?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    if out.status.code() == Some(5) {
        println!(
            "gate: the input smoke window was not being drawn; input checks recorded as not run"
        );
        return Ok(Done::NotRun);
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
        &[
            ("DREAMER_DESKTOP_STATE_DIR", &state.to_string_lossy()),
            // Installed app packages for the contributions check; never
            // the user's configuration.
            (
                "DREAMER_CONFIG_DIR",
                &state.join("config").to_string_lossy(),
            ),
        ],
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

/// The clipboard smoke: copy and paste in a terminal pane, through tmux's
/// buffers, in the harness drawer (a shell here), and in a native app
/// pane, with an in-memory clipboard. Session `main` has one shell pane
/// that printed `READY-%0`; the configuration is private.
pub fn clipboard_smoke_output(
    dir: &Path,
    env_remove: &[&str],
    state: &Path,
) -> Result<std::process::Output, String> {
    const SH: &str = "env PS1='$ ' /bin/sh";
    // The app pane's host runs beside the app.
    let build = clean_git_env(Command::new("cargo"))
        .args(["build", "--locked", "-p", "dreamer-termapp"])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer-termapp failed".into());
    }
    let config = state.join("config");
    std::fs::create_dir_all(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    // The drawer's harness is a plain shell: the check is about the
    // drawer's terminal, not about any agent.
    std::fs::write(
        config.join("dreamer.yaml"),
        "agents:\n  pi:\n    command: [env, 'PS1=$ ', /bin/sh]\n",
    )
    .map_err(|e| format!("{}: {e}", config.display()))?;
    isolated_app_output(
        dir,
        "clipboard",
        SH,
        None,
        &[&["send-keys", "-t", "%0", "echo READY-$TMUX_PANE", "Enter"]],
        &["--smoke-clipboard"],
        env_remove,
        &[
            ("DREAMER_DESKTOP_STATE_DIR", &state.to_string_lossy()),
            ("DREAMER_CONFIG_DIR", &config.to_string_lossy()),
        ],
    )
}

/// Copy and paste in every kind of pane there is, with an in-memory
/// clipboard. Needs a window that is being drawn: one that is not (screen
/// locked, display asleep) exits 5 and is recorded as not run.
fn clipboard_smoke(dir: &Path) -> Result<Done, String> {
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let state = desktop_state_dir("clipboard");
    let out = clipboard_smoke_output(dir, &[], &state);
    let _ = std::fs::remove_dir_all(&state);
    let out = out?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("gate:   {line}");
    }
    match out.status.code() {
        Some(0) => Ok(Done::Passed),
        Some(5) => {
            println!(
                "gate: the clipboard smoke window was not being drawn; copy and paste checks recorded as not run"
            );
            Ok(Done::NotRun)
        }
        _ => Err(format!(
            "clipboard smoke failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
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

/// Pixel evidence (#89): reference scenes captured in both of Dreamer's
/// schemes and checked: the scheme's background is the main color (one
/// theme across terminals and chrome) and its green, which the scene
/// prints, is drawn. The images stay in `target/gate/screens/`. Not run on
/// a locked screen, or without Screen Recording permission.
fn screens(dir: &Path) -> Result<Done, String> {
    if !cfg!(target_os = "macos") || !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    // Dreamer's two schemes (terminal-core's DREAMER_DARK and DREAMER_LIGHT):
    // background, and green (ANSI 2), which single-pane prints.
    let out = crate::util::repo_root().join("target/gate/screens");
    for (skin, background, green) in [
        ("dark", 0x14_16_1a, 0x81_c9_95),
        ("light", 0xf6_f6_f4, 0x1e_7a_3c),
    ] {
        let path = out.join(format!("single-pane-{skin}.png"));
        let check = |p: &Path| crate::scenes::check_capture(p, background, green);
        let summary = match crate::scenes::capture_checked(dir, "single-pane", skin, &path, check) {
            Ok(summary) => summary,
            Err(e) if e.contains("could not capture window") => {
                println!(
                    "gate: screen capture is not allowed for this terminal (System Settings > \
                     Privacy & Security > Screen & System Audio Recording); recorded as not run"
                );
                return Ok(Done::NotRun);
            }
            Err(e) => return Err(e),
        };
        println!("gate:   screen: {} ({summary})", path.display());
    }
    if components_scene(dir, &out)? == Done::NotRun {
        return Ok(Done::NotRun);
    }
    // The theme switched live (#265): every pane and the chrome take the
    // new scheme in the same window, and a program's own background
    // (OSC 11) stays as it set it.
    let (before, after) = (
        out.join("theme-switch-before.png"),
        out.join("theme-switch-after.png"),
    );
    // The same app just opened and drew two windows, so a third that does
    // not appear means the display stopped serving it (asleep, or locked
    // between captures): not run, never a pass.
    match crate::scenes::capture_switch(dir, "gruvbox-dark", &before, &after) {
        Ok(()) => {}
        Err(e) if e.contains("opened no window") => {
            println!(
                "gate: the theme-switch window never appeared after two that did (display asleep or locked?); the theme switch is recorded as not run"
            );
            return Ok(Done::NotRun);
        }
        Err(e) => return Err(e),
    }
    let own = 0x40_20_20;
    let summary = crate::scenes::check_capture(&before, 0x14_16_1a, 0x81_c9_95)?;
    println!("gate:   screen: {} ({summary})", before.display());
    // The pane sets its own background after the window's first frame. If
    // even that is not in the first capture, the window is not being
    // repainted (the screen locked, or another app came to the front), and
    // the switch cannot be judged: not run, never a failure or a pass.
    if crate::scenes::share_of(&before, own)? < 0.05 {
        println!(
            "gate: the window stopped repainting after its first frame (screen locked, or not in front); the theme switch is recorded as not run"
        );
        return Ok(Done::NotRun);
    }
    // The marker is printed after the switch. Without it the window
    // stopped repainting between the captures (#370): not run, never a
    // pass, and not a failure of the switch.
    if !crate::scenes::switch_marked(&after)? {
        println!(
            "gate: the window stopped repainting after the theme switch (screen locked, or not in front); the theme switch is recorded as not run"
        );
        return Ok(Done::NotRun);
    }
    let summary = crate::scenes::check_capture(&after, 0x28_28_28, 0x98_97_1a)?;
    println!("gate:   screen: {} ({summary})", after.display());
    let old = crate::scenes::share_of(&after, 0x14_16_1a)?;
    if old > 0.005 {
        return Err(format!(
            "{}: {:.1}% of the window still has the old background",
            after.display(),
            old * 100.0
        ));
    }
    for p in [&before, &after] {
        let kept = crate::scenes::share_of(p, own)?;
        if kept < 0.05 {
            return Err(format!(
                "{}: the pane's own OSC 11 background covers only {:.1}%",
                p.display(),
                kept * 100.0
            ));
        }
    }
    println!(
        "gate:   screen: the switch to gruvbox-dark reached every pane and the chrome; the OSC 11 background stayed"
    );
    Ok(Done::Passed)
}

/// Live and image previews: a template app opened as a preview pane in
/// the desktop (`dream app preview --open`), drawn by the desktop as a PNG
/// (`--png`) whose pixels are the scheme's in both skins, and a save to
/// its `app.ts` shown in the pane within a second. The images stay in
/// `target/gate/screens/`. Not run on a locked screen, without Screen
/// Recording permission, or when the window stops repainting.
fn app_preview(dir: &Path) -> Result<Done, String> {
    if !cfg!(target_os = "macos") || !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    let out = crate::util::repo_root().join("target/gate/screens");
    match crate::preview::check(dir, &out)? {
        crate::preview::Outcome::Passed(lines) => {
            for l in lines {
                println!("gate:   {l}");
            }
            Ok(Done::Passed)
        }
        crate::preview::Outcome::NotRun(why) => {
            println!("gate: {why}; the preview check is recorded as not run");
            Ok(Done::NotRun)
        }
    }
}

/// The reference app's components in both skins: the app as a registered
/// app pane, drawn natively, beside the same app started from a shell,
/// drawn as a terminal interface. Checked: the scheme's background (or the
/// app pane's surface) is the main color, the accent is drawn, a native
/// badge is on its danger surface, and the terminal's badge is in the
/// scheme's red. A window that drew neither app (it stopped repainting
/// after its first frame) is recorded as not run.
fn components_scene(dir: &Path, out: &Path) -> Result<Done, String> {
    // Per skin: background, the chrome's surface, the accent, the danger
    // surface (a native badge), and ANSI red (the terminal's badge).
    for (skin, background, surface, accent, danger_surface, red) in [
        (
            "dark", 0x14_16_1a, 0x10_1216, 0x8a_b4_f8, 0x5a_2a_2a, 0xf2_8b_82,
        ),
        (
            "light", 0xf6_f6_f4, 0xec_eb_e7, 0x2f_6f_de, 0xfb_e3_e0, 0xb3_26_1e,
        ),
    ] {
        let path = out.join(format!("app-components-{skin}.png"));
        crate::scenes::capture(dir, "app-components", skin, &path)?;
        let native = crate::scenes::share_of(&path, danger_surface)?;
        // The terminal's red is saturated: allow for color management.
        let terminal = crate::scenes::share_near(&path, red, 28)?;
        if native < 0.000_2 && terminal < 0.000_2 {
            println!(
                "gate: the window did not draw the reference app (screen locked, or not in front); the component scenes are recorded as not run"
            );
            return Ok(Done::NotRun);
        }
        let summary = crate::scenes::check_capture(&path, background, accent)
            .or_else(|_| crate::scenes::check_capture(&path, surface, accent))?;
        for (share, what) in [(native, "natively"), (terminal, "as a terminal interface")] {
            if share < 0.000_2 {
                return Err(format!(
                    "{}: the reference app's badges are not drawn {what} ({:.3}%)",
                    path.display(),
                    share * 100.0
                ));
            }
        }
        println!(
            "gate:   screen: {} ({summary}; badges natively {:.2}%, in the terminal {:.2}%)",
            path.display(),
            native * 100.0,
            terminal * 100.0
        );
    }
    Ok(Done::Passed)
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
                "gate: the display did not refresh; workspace sizing, click, keyboard, drag, and decoration checks recorded as not run"
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
    match again.status.code() {
        Some(0) => Ok(first),
        // The same display that did not refresh for the first launch: not
        // run, like it, rather than failed.
        Some(5) => {
            println!(
                "gate: the display did not refresh; the workspace restore check recorded as not run"
            );
            Ok(Done::NotRun)
        }
        _ => Err(format!(
            "workspace restore smoke failed ({}): {}",
            again.status,
            String::from_utf8_lossy(&again.stderr).trim()
        )),
    }
}

/// Run `dreamer --attach 0 <args>` against an isolated tmux server whose
/// pane runs `command` (optionally after typing `type_first` into it),
/// with private daemon directories and the default-tmux guard, returning
/// the app's output. `env_remove` names variables the app must not see,
/// and `env` sets others.
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

/// Web panes and the Dreamer Web component (macOS), when `cargo xtask deps
/// web` has provisioned the pinned CEF, so this is the stage a Chromium
/// update passes: the desktop with its `web` feature passes clippy; it
/// bundles without Chromium, and the component (the framework and its
/// helper apps, each signed) is built beside it, and a helper loads the
/// framework; the bundle's `dream web status` says, with an empty data
/// directory, how to add the component, and `dream web install` adds it;
/// and, with a refreshing display, the desktop draws pages with Chromium
/// from the installed component, checked in captures of the window.
/// Without CEF, or without a display that refreshes, recorded as not run.
fn web_panes(dir: &Path) -> Result<Done, String> {
    let Some(web) = crate::web::provisioned()? else {
        println!(
            "gate: the web engine is not provisioned (cargo xtask deps web); web panes recorded as not run"
        );
        return Ok(Done::NotRun);
    };
    let clippy = clean_git_env(Command::new("cargo"))
        .args([
            "clippy",
            "--locked",
            "--package",
            "dreamer-app",
            "--package",
            "web-engine",
            "--package",
            "cef-sys",
            "--package",
            "dreamer-web-component",
            "--all-targets",
            "--features",
            "dreamer-app/web",
            "--",
            "-D",
            "warnings",
        ])
        .envs(web.env())
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cargo clippy: {e}"))?;
    if !clippy.success() {
        return Err("clippy with the web feature failed".into());
    }
    let (app, component) = crate::web::bundle(dir, crate::package::Profile::Debug, &web)?;
    let data = dir.join("target/gate/web-data");
    let added = crate::web::check_install(&app, &component, &data);
    let _ = std::fs::remove_dir_all(&data);
    for line in added? {
        println!("gate:   {line}");
    }
    if !require_refreshing_display()? {
        return Ok(Done::NotRun);
    }
    match crate::web::smoke(&app, &component, &dir.join("target/gate/web"))? {
        crate::web::Smoke::Passed(lines) => {
            for line in lines {
                println!("gate:   {line}");
            }
            Ok(Done::Passed)
        }
        crate::web::Smoke::NotRun(lines) => {
            for line in lines.lines() {
                println!("gate:   {line}");
            }
            println!("gate: the web pane was not being drawn; web pane checks recorded as not run");
            Ok(Done::NotRun)
        }
    }
}

/// The release's two macOS artifacts: the base bundle (no Chromium) and
/// its zip, unpacked and launched outside the source tree, and the Dreamer
/// Web zip, added with the unpacked app's `dream web install` into an
/// empty data directory ([`crate::fresh::mac_check`]).
fn package_away_from_tree(dir: &Path) -> Result<Done, String> {
    use crate::package::{Profile, macos_packages, smoke_from_zip};
    let out = dir.join("target").join("package");
    let web = crate::deps::ensure_web(&crate::manifest::Manifest::load()?)?;
    let made = macos_packages(dir, Profile::Release, &out, &web, &Default::default())?;
    println!("gate:   {}", made.zip.display());
    println!("gate:   {}", made.component_zip.display());
    for line in crate::fresh::mac_check(dir)? {
        println!("gate:   {line}");
    }
    if !require_gui()? {
        return Ok(Done::NotRun);
    }
    let output = smoke_from_zip(&made.zip)?;
    for line in output.lines() {
        println!("gate:   {line}");
    }
    Ok(Done::Passed)
}

/// Packages the gate's ARM64 Linux image adds to `ubuntu:24.04`: the build
/// tools, GPUI's Linux build dependencies (for clippy; the same list as the
/// GUI dependencies in `.github/workflows/ci.yml`), the tmux baseline's
/// build dependencies, and OpenSSH plus `pkill` for remote acceptance.
const ARM64_PACKAGES: &[&str] = &[
    "build-essential",
    "pkg-config",
    "curl",
    "ca-certificates",
    "xz-utils",
    "git",
    "libclang-dev",
    "python3",
    "libfontconfig-dev",
    "libfreetype-dev",
    "libwayland-dev",
    "libxkbcommon-dev",
    "libxkbcommon-x11-dev",
    "libx11-xcb-dev",
    "libxcb1-dev",
    "libvulkan-dev",
    "libevent-dev",
    "libncurses-dev",
    "bison",
    "openssh-server",
    "openssh-client",
    "procps",
];

/// Raise when the image changes in a way the package list does not show.
const ARM64_IMAGE_REVISION: u32 = 1;

/// The Dockerfile of the gate's ARM64 image. Its hash names the image, so a
/// changed package list builds a new one.
fn arm64_dockerfile() -> String {
    format!(
        "# dreamer gate ARM64 Linux image, revision {ARM64_IMAGE_REVISION}\n\
         FROM ubuntu:24.04\n\
         ENV DEBIAN_FRONTEND=noninteractive\n\
         RUN apt-get update -qq \\\n \
         && apt-get install -y -qq --no-install-recommends {} \\\n \
         && rm -rf /var/lib/apt/lists/* \\\n \
         && mkdir -p /run/sshd\n",
        ARM64_PACKAGES.join(" ")
    )
}

/// `dreamer-gate-arm64:<hash of the Dockerfile>`. FNV-1a, so the name does
/// not change with the Rust release that built xtask.
fn arm64_image_tag(dockerfile: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dockerfile.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("dreamer-gate-arm64:{:012x}", h >> 16)
}

/// The gate's ARM64 image, built on first use (or when its Dockerfile
/// changed); older builds of it are removed.
pub(crate) fn ensure_arm64_image() -> Result<String, String> {
    use std::io::Write as _;
    let dockerfile = arm64_dockerfile();
    let tag = arm64_image_tag(&dockerfile);
    let present = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", &tag])
        .output()
        .is_ok_and(|o| o.status.success());
    if present {
        println!("gate:   image {tag}");
        return Ok(tag);
    }
    println!("gate:   building image {tag} (once per package list)");
    let mut child = Command::new("docker")
        .args(["build", "--platform", "linux/arm64", "-t", &tag, "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("docker build: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("docker build: no stdin")?
        .write_all(dockerfile.as_bytes())
        .map_err(|e| format!("docker build: {e}"))?;
    let status = child.wait().map_err(|e| format!("docker build: {e}"))?;
    if !status.success() {
        return Err(format!("building the image {tag} failed"));
    }
    if let Ok(out) = Command::new("docker")
        .args([
            "image",
            "ls",
            "dreamer-gate-arm64",
            "--format",
            "{{.Repository}}:{{.Tag}}",
        ])
        .output()
    {
        for old in String::from_utf8_lossy(&out.stdout).lines() {
            if old != tag {
                let _ = Command::new("docker")
                    .args(["image", "rm", old])
                    .stdout(std::process::Stdio::null())
                    .status();
            }
        }
    }
    Ok(tag)
}

/// Native ARM64 Linux checks in a container when a container runtime is
/// running; otherwise recorded as not run. In order: clippy on
/// Linux (code behind `cfg(target_os = "macos")` hides nothing there), the
/// headless crates' tests against the pinned tmux, and the remote
/// acceptance cases through real OpenSSH (`cargo xtask remote-acceptance`,
/// as root with a private sshd on a loopback port).
///
/// Only on an ARM64 host: elsewhere the container runs under qemu user
/// emulation, which takes most of an hour and shows the emulator as each
/// process's executable, so agent recognition fails for reasons that are
/// not the product's. `full.yml` covers the ARM64 cross-build there.
fn arm64_container(dir: &Path) -> Result<Done, String> {
    if std::env::consts::ARCH != "aarch64" {
        println!(
            "gate: {} host; the ARM64 Linux suite runs only on an ARM64 host (recorded as not run)",
            std::env::consts::ARCH
        );
        return Ok(Done::NotRun);
    }
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
    let image = ensure_arm64_image()?;
    // `ulimit -c 0`: one test makes dreamerd abort on purpose, and the
    // container would otherwise write its core into the mounted tree.
    // Only this checkout's target is kept in the shared cache volume; the
    // others belong to gate worktrees that have since been rebuilt.
    // `USER`: remote acceptance logs in to its sshd as this user.
    let script = "set -e; ulimit -c 0; export USER=root; mkdir -p /run/sshd; \
        find /root/.cache -maxdepth 1 -name 'dreamer-target-*' ! -path \"$CARGO_TARGET_DIR\" -exec rm -rf {} +; \
        command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
        . \"$HOME/.cargo/env\"; \
        cargo xtask deps prepare --locked; \
        export DREAMER_TMUX=$(cargo xtask deps tmux | tail -n 1); \
        echo 'gate:   arm64: clippy'; \
        cargo clippy --workspace --all-targets --locked -- -D warnings; \
        echo 'gate:   arm64: headless tests'; \
        cargo test --locked $(cargo xtask headless --cargo-args); \
        echo 'gate:   arm64: remote acceptance'; \
        cargo xtask remote-acceptance";
    // One target directory per checkout: every checkout is mounted at the
    // same /src, and cargo judges freshness by file times, so a build from
    // another checkout's newer files could otherwise pass for this one's.
    let target = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        dir.hash(&mut h);
        format!(
            "CARGO_TARGET_DIR=/root/.cache/dreamer-target-{:016x}",
            h.finish()
        )
    };
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
            &target,
            "-e",
            "CARGO_INCREMENTAL=0",
            "-w",
            "/src",
            &image,
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

    let reports = root.join("target").join("gate").join(platform.name());
    let record = reuse_record_path(&reports, &tree);
    let last_tree_path = reports.join("worktree-tree");
    // Later stages run what the reused ones built; with the build output
    // gone (cleared to free disk, say) nothing is reused.
    let built = root.join("target/gate/worktree/target/debug").is_dir();
    let reusable = match mode {
        Mode::Full if built => reusable_stages(
            std::fs::read_to_string(&record).ok().as_deref(),
            &tree,
            std::fs::read_to_string(&last_tree_path).ok().as_deref(),
        ),
        Mode::Full => Vec::new(),
        // A new staged run of this tree decides afresh what it passed.
        Mode::Staged => {
            let _ = std::fs::remove_file(&record);
            Vec::new()
        }
    };

    let worktree = prepare_worktree(&root, &tree)?;
    // Which tree the gate worktree (and so its build output) was last
    // checked out for: a `--full` run reuses only on top of that tree.
    std::fs::create_dir_all(&reports)
        .and_then(|()| std::fs::write(&last_tree_path, &tree))
        .map_err(|e| format!("cannot write {}: {e}", last_tree_path.display()))?;
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
        if stage.when != When::Full && reusable.iter().any(|r| r == stage.name) {
            println!(
                "gate: {:<8} reused  (passed in pre-commit for tree {})",
                stage.name,
                &tree[..12.min(tree.len())]
            );
            results.push(StageResult {
                name: stage.name,
                command,
                outcome: Outcome::Reused,
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
    if mode == Mode::Staged && failed.is_none() {
        std::fs::write(&record, reuse_record(&tree, &results))
            .map_err(|e| format!("cannot write {}: {e}", record.display()))?;
    }

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

/// Where a passing `--staged` run of `tree` records the stages it passed.
fn reuse_record_path(reports: &Path, tree: &str) -> PathBuf {
    reports.join(format!("{tree}-staged-passed.txt"))
}

/// The record a passing `--staged` run leaves: its tree, then each stage
/// that passed. Stages that were skipped or not run are not listed.
fn reuse_record(tree: &str, results: &[StageResult]) -> String {
    let mut out = format!("tree {tree}\n");
    for r in results.iter().filter(|r| r.outcome == Outcome::Passed) {
        let _ = writeln!(out, "passed {}", r.name);
    }
    out
}

/// The stages a `--full` run of `tree` may take as passed from a
/// `--staged` run: those listed in its record, only when the record is for
/// exactly this tree and the gate worktree was last checked out for this
/// tree (so no later run of another tree has rebuilt what those stages
/// built). Anything else reuses nothing.
fn reusable_stages(record: Option<&str>, tree: &str, worktree_tree: Option<&str>) -> Vec<String> {
    let Some(record) = record else {
        return Vec::new();
    };
    if tree.is_empty() || worktree_tree.map(str::trim) != Some(tree) {
        return Vec::new();
    }
    let mut lines = record.lines();
    if lines.next() != Some(&format!("tree {tree}")) {
        return Vec::new();
    }
    lines
        .filter_map(|l| l.strip_prefix("passed "))
        .map(str::to_owned)
        .collect()
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

pub(crate) fn json_escape(s: &str) -> String {
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
    // The gate builds each revision once; incremental caches only pile up
    // (tens of GB per day of commits) without making those builds faster.
    cmd.env("CARGO_INCREMENTAL", "0");
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
        assert_eq!(scope_for(["README.md", "docs/scripts.md"]), Scope::Docs);
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

    fn result(name: &'static str, outcome: Outcome) -> StageResult {
        StageResult {
            name,
            command: String::new(),
            outcome,
            millis: 0,
        }
    }

    #[test]
    fn full_runs_reuse_only_what_a_staged_run_of_the_same_tree_passed() {
        let record = reuse_record(
            "aaa",
            &[
                result("clippy", Outcome::Passed),
                result("live", Outcome::NotRun),
                result("arm64", Outcome::Skipped),
            ],
        );
        assert_eq!(
            reusable_stages(Some(&record), "aaa", Some("aaa\n")),
            ["clippy"]
        );
        // Another tree, a worktree since checked out for another tree, or
        // no record: nothing is reused.
        assert!(reusable_stages(Some(&record), "bbb", Some("bbb")).is_empty());
        assert!(reusable_stages(Some(&record), "aaa", Some("bbb")).is_empty());
        assert!(reusable_stages(Some(&record), "aaa", None).is_empty());
        assert!(reusable_stages(None, "aaa", Some("aaa")).is_empty());
        // A record whose content names another tree than its file name.
        let other = reuse_record("bbb", &[result("clippy", Outcome::Passed)]);
        assert!(reusable_stages(Some(&other), "aaa", Some("aaa")).is_empty());
    }

    #[test]
    fn json_escaping_handles_quotes_and_controls() {
        assert_eq!(json_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
        assert_eq!(json_escape("x\ny"), "x\\u000ay");
    }
}
