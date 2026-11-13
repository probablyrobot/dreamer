//! `cargo xtask scenes`: the reference scenes of the design review gate
//! (spec 14.4), one after another, in both default skins, for the product
//! owner to capture and approve. Each scene runs against an isolated tmux
//! server with private daemon directories and a temporary config; nothing
//! touches the user's own tmux, state, or settings.
//!
//! This is interactive: it opens the app, says which capture to save, and
//! waits for the window to be closed. Captures belong outside the
//! repository, in `~/Devel/analysis/dreamer/design/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::repo_root;

const SH: &str = "env PS1='$ ' /bin/sh";

struct Scene {
    name: &'static str,
    what: &'static str,
    /// tmux commands after the `main` session exists.
    setup: &'static [&'static [&'static str]],
    /// Text typed into every pane, then Enter.
    type_all: Option<&'static str>,
    /// Extra `dreamer` arguments.
    args: &'static [&'static str],
    /// tmux commands once the app is up.
    after: &'static [&'static [&'static str]],
}

const SCENES: &[Scene] = &[
    Scene {
        name: "single-pane",
        what: "one pane with colored text, wide characters, and a prompt",
        setup: &[],
        type_all: Some("ls -la /; printf '\\033[1mbold\\033[0m \\033[32mgreen\\033[0m 世界 ✓\\n'"),
        args: &[],
        after: &[],
    },
    Scene {
        name: "dense-splits",
        what: "six panes in one window",
        setup: &[
            &["split-window", "-h", "-t", "main:0", SH],
            &["split-window", "-v", "-t", "main:0.0", SH],
            &["split-window", "-v", "-t", "main:0.2", SH],
            &["split-window", "-v", "-t", "main:0.1", SH],
            &["split-window", "-v", "-t", "main:0.3", SH],
        ],
        type_all: Some("echo pane $TMUX_PANE; date; uname -a"),
        args: &[],
        after: &[],
    },
    Scene {
        name: "sidebar-open",
        what: "the sessions sidebar open, with three sessions",
        setup: &[
            &["new-session", "-d", "-s", "logs", SH],
            &["new-session", "-d", "-s", "build", SH],
            &["new-window", "-t", "main", "-n", "editor", SH],
        ],
        type_all: Some("echo session $(tmux display -p '#S' 2>/dev/null)"),
        args: &["--open-sidebar"],
        after: &[],
    },
    Scene {
        name: "command-palette",
        what: "the command palette open over two panes",
        setup: &[&["split-window", "-h", "-t", "main:0", SH]],
        type_all: Some("echo ready"),
        args: &["--open-palette"],
        after: &[],
    },
    Scene {
        name: "error-state",
        what: "a pane in tmux copy mode, marked with the attach fallback",
        setup: &[&["split-window", "-h", "-t", "main:0", SH]],
        type_all: Some("seq 1 50"),
        args: &[],
        after: &[&["copy-mode", "-t", "%1"]],
    },
];

pub fn run(args: &[String]) -> Result<(), String> {
    let only = args.first().cloned();
    let root = repo_root();
    let build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
        ])
        .current_dir(&root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer and dreamerd failed".into());
    }
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    println!("scenes: save each capture to ~/Devel/analysis/dreamer/design/<name> as shown");
    println!("scenes: the agentic pane scene is not available yet (M6)");
    for skin in ["dark", "light"] {
        for scene in SCENES {
            if only.as_deref().is_some_and(|o| o != scene.name) {
                continue;
            }
            let capture = format!("{}-{skin}-{os}.png", scene.name);
            println!("\nscenes: {} ({skin}): {}", scene.name, scene.what);
            println!("scenes: capture the window as {capture}, then close it to continue");
            show(&root, scene, skin)?;
        }
    }
    Ok(())
}

fn show(root: &Path, scene: &Scene, skin: &str) -> Result<(), String> {
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let id = format!("{}-{skin}-{}", scene.name, std::process::id());
    let socket = format!("dreamer-scene-{id}");
    let dir = std::env::temp_dir().join(format!("dreamer-scene-{id}"));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    std::fs::write(config.join("dreamer.yaml"), format!("appearance: {skin}\n"))
        .map_err(|e| format!("config: {e}"))?;
    let t = |args: &[&str]| {
        let _ = Command::new(&tmux)
            .args(["-L", &socket])
            .args(args)
            .status();
    };
    t(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "140",
        "-y",
        "40",
        SH,
    ]);
    for cmd in scene.setup {
        t(cmd);
    }
    if let Some(text) = scene.type_all {
        let panes = Command::new(&tmux)
            .args(["-L", &socket, "list-panes", "-a", "-F", "#{pane_id}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for p in panes.lines() {
            t(&["send-keys", "-t", p, text, "Enter"]);
        }
    }
    t(&["select-window", "-t", "main:0"]);
    let mut app = Command::new(root.join("target/debug/dreamer"))
        .arg("--workspace")
        .args(scene.args)
        .args(["-L", &socket, "--tmux", &tmux])
        .env("DREAMER_RUNTIME_DIR", dir.join("rt"))
        .env("DREAMER_STATE_DIR", dir.join("st"))
        .env("DREAMER_DESKTOP_STATE_DIR", dir.join("desktop"))
        .env("DREAMER_CONFIG_DIR", &config)
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .spawn()
        .map_err(|e| format!("dreamer: {e}"))?;
    if !scene.after.is_empty() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        for cmd in scene.after {
            t(cmd);
        }
    }
    let _ = app.wait();
    if let Some(pid) = std::fs::read_to_string(dir.join("rt/dreamerd.lock"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    t(&["kill-server"]);
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
    Ok(())
}
