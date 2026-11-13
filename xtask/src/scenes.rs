//! `cargo xtask scenes`: the reference scenes of the design review gate
//! (spec 14.4), one after another, in both default skins, for the product
//! owner to capture and approve. Each scene runs against an isolated tmux
//! server with private daemon directories and a temporary config; nothing
//! touches the user's own tmux, state, or settings.
//!
//! This is interactive: it opens the app, says which capture to save, and
//! waits for the window to be closed. Captures belong outside the
//! repository, in `~/Devel/analysis/dreamer/design/`.
//!
//! `cargo xtask screenshot` opens the same scenes and captures the app's
//! own window by itself (see [`screenshot`]).

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

fn build(root: &Path) -> Result<(), String> {
    let build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
        ])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer and dreamerd failed".into());
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let only = args.first().cloned();
    let root = repo_root();
    build(&root)?;
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
    let mut running = launch(root, scene, skin, "")?;
    let _ = running.app.wait();
    running.finish();
    Ok(())
}

/// A scene's app, tmux server, and private directories.
struct Running {
    app: std::process::Child,
    tmux: String,
    socket: String,
    dir: PathBuf,
}

impl Running {
    /// Stop the app, its daemon, and the tmux server; remove the files.
    fn finish(mut self) {
        let _ = self.app.kill();
        let _ = self.app.wait();
        if let Some(pid) = std::fs::read_to_string(self.dir.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        // The server may already be gone with its last session.
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start `scene` in `skin`, with `extra` appended to its `dreamer.yaml`.
fn launch(root: &Path, scene: &Scene, skin: &str, extra: &str) -> Result<Running, String> {
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let id = format!("{}-{skin}-{}", scene.name, std::process::id());
    let socket = format!("dreamer-scene-{id}");
    let dir = std::env::temp_dir().join(format!("dreamer-scene-{id}"));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    std::fs::write(
        config.join("dreamer.yaml"),
        format!("appearance: {skin}\n{extra}\n"),
    )
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
    let app = Command::new(root.join("target/debug/dreamer"))
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
    Ok(Running {
        app,
        tmux,
        socket,
        dir,
    })
}

const SCREENSHOT_USAGE: &str = "\
usage: cargo xtask screenshot [--scene NAME] [--skin dark|light] [--out DIR]
                              [--settings YAML] [--settle SECONDS]

Opens a reference scene (default single-pane) against an isolated tmux
server and saves a PNG of the app's own window, at device resolution, to
DIR (default target/screenshots). --settings is appended to the scene's
dreamer.yaml, e.g. --settings 'font: {size: 15, letter_spacing: -0.3}'.
Only Dreamer's window is captured. macOS only for now.";

/// `cargo xtask screenshot`: open a scene and capture the app's window.
///
/// macOS: the window is found by the app's process id in the window list
/// and captured with `screencapture -l`, so nothing else on the screen is
/// recorded. The first run may ask to allow screen recording for the
/// terminal. A locked screen or a sleeping display gives no image.
pub fn screenshot(args: &[String]) -> Result<(), String> {
    let root = repo_root();
    let mut scene_name = "single-pane".to_owned();
    let mut skin = "dark".to_owned();
    let mut out = root.join("target/screenshots");
    let mut extra = String::new();
    let mut settle = 2.0f64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{SCREENSHOT_USAGE}"))
        };
        match a.as_str() {
            "--scene" => scene_name = value()?,
            "--skin" => skin = value()?,
            "--out" => out = PathBuf::from(value()?),
            "--settings" => extra = value()?,
            "--settle" => {
                settle = value()?
                    .parse()
                    .map_err(|_| "--settle takes seconds".to_owned())?;
            }
            "-h" | "--help" => {
                println!("{SCREENSHOT_USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n\n{SCREENSHOT_USAGE}")),
        }
    }
    if !cfg!(target_os = "macos") {
        return Err("screenshot: only macOS is supported so far".into());
    }
    if !["dark", "light"].contains(&skin.as_str()) {
        return Err(format!("--skin {skin}: use dark or light"));
    }
    let scene = SCENES
        .iter()
        .find(|s| s.name == scene_name)
        .ok_or_else(|| {
            let names: Vec<_> = SCENES.iter().map(|s| s.name).collect();
            format!("no scene `{scene_name}`; scenes: {}", names.join(", "))
        })?;
    if crate::package::screen_locked() == Some(true) {
        return Err("screenshot: the screen is locked, so the window is not drawn".into());
    }
    build(&root)?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let path = out.join(format!("{}-{skin}.png", scene.name));
    let running = launch(&root, scene, &skin, &extra)?;
    let result = capture_window(running.app.id(), &path, settle);
    running.finish();
    result?;
    println!("screenshot: {}", path.display());
    Ok(())
}

/// The on-screen, normal-layer window owned by `pid`, if any.
fn window_of(pid: u32) -> Option<u32> {
    let script = format!(
        "ObjC.import('CoreGraphics');\
         const all = ObjC.deepUnwrap(ObjC.castRefToObject(\
           $.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));\
         const w = all.filter(x => x.kCGWindowOwnerPID == {pid} && x.kCGWindowLayer == 0);\
         w.length ? String(w[0].kCGWindowNumber) : ''"
    );
    let out = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn capture_window(pid: u32, path: &Path, settle: f64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let id = loop {
        if let Some(id) = window_of(pid) {
            break id;
        }
        if std::time::Instant::now() > deadline {
            return Err("screenshot: the app opened no window within 20 s".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    // Let the panes draw what the scene typed.
    std::thread::sleep(std::time::Duration::from_secs_f64(settle.max(0.0)));
    let _ = std::fs::remove_file(path);
    let status = Command::new("screencapture")
        .args(["-x", "-o", &format!("-l{id}")])
        .arg(path)
        .output()
        .map_err(|e| format!("screencapture: {e}"))?;
    if !path.is_file() {
        return Err(format!(
            "screencapture could not capture window {id}: {} \
             (allow screen recording for this terminal in System Settings > \
             Privacy & Security > Screen & System Audio Recording)",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
}
