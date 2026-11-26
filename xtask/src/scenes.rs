//! `cargo xtask scenes`: the reference scenes of the design review gate,
//! one after another, in both default skins, for the product
//! owner to capture and approve. Each scene runs against an isolated tmux
//! server with private daemon directories and a temporary config; nothing
//! touches the user's own tmux, state, or settings.
//!
//! This is interactive: it opens the app, says which capture to save, and
//! waits for the window to be closed. Captures belong outside the
//! repository.
//!
//! `cargo xtask screenshot` opens the same scenes and captures the app's
//! own window by itself (see [`screenshot`]).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::repo_root;

const SH: &str = "env PS1='$ ' /bin/sh";

pub(crate) struct Scene {
    pub(crate) name: &'static str,
    pub(crate) what: &'static str,
    /// tmux commands after the `main` session exists.
    pub(crate) setup: &'static [&'static [&'static str]],
    /// Text typed into every pane, then Enter.
    pub(crate) type_all: Option<&'static str>,
    /// Extra `dreamer` arguments.
    pub(crate) args: &'static [&'static str],
    /// tmux commands once the app is up.
    pub(crate) after: &'static [&'static [&'static str]],
    /// An app package (its manifest, from the repository root) shown in a
    /// new window once the app is up: on the left as a registered app,
    /// which the desktop draws natively; on the right started from a
    /// shell, unregistered, so the desktop shows its terminal interface.
    pub(crate) app: Option<&'static str>,
    /// A web page opened in a new pane once the app is up (`dream web
    /// open`): the scene needs the app built with web panes.
    pub(crate) web: Option<&'static str>,
}

const SCENES: &[Scene] = &[
    Scene {
        name: "single-pane",
        what: "one pane with colored text, wide characters, and a prompt",
        setup: &[],
        type_all: Some("ls -la /; printf '\\033[1mbold\\033[0m \\033[32mgreen\\033[0m 世界 ✓\\n'"),
        args: &[],
        after: &[],
        app: None,
        web: None,
    },
    Scene {
        name: "theme-switch",
        what: "two panes, the narrow one with its own background (OSC 11), for switching the theme live",
        setup: &[&["split-window", "-h", "-l", "40", "-t", "main:0", SH]],
        type_all: Some("printf '\\033[32mgreen\\033[0m text in the theme\\n'"),
        args: &[],
        // The narrow pane's own background is set by `capture_switch` once
        // the pane is attached: a color set before the daemon attaches is
        // not restored yet.
        after: &[],
        app: None,
        web: None,
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
        app: None,
        web: None,
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
        app: None,
        web: None,
    },
    Scene {
        name: "command-palette",
        what: "the command palette open over two panes",
        setup: &[&["split-window", "-h", "-t", "main:0", SH]],
        type_all: Some("echo ready"),
        args: &["--open-palette"],
        after: &[],
        app: None,
        web: None,
    },
    Scene {
        name: "error-state",
        what: "a pane in tmux copy mode, marked with the attach fallback",
        setup: &[&["split-window", "-h", "-t", "main:0", SH]],
        type_all: Some("seq 1 50"),
        args: &[],
        after: &[&["copy-mode", "-t", "%1"]],
        app: None,
        web: None,
    },
    Scene {
        name: "app-components",
        what: "the reference app with every text, input, and status component: drawn natively (left) and as a terminal interface (right)",
        setup: &[],
        type_all: None,
        args: &[],
        after: &[],
        app: Some(COMPONENTS_APP),
        web: None,
    },
    Scene {
        name: "web-pane",
        what: "a web page drawn by Chromium in a pane beside a terminal",
        setup: &[],
        type_all: Some("echo terminal beside a web page"),
        args: &[],
        after: &[],
        app: None,
        web: Some(WEB_PAGE),
    },
];

/// The `web-pane` scene's page: one color (`WEB_PAGE_COLOR`) over all of
/// it, and a heading.
const WEB_PAGE: &str = "data:text/html,<title>Dreamer web pane</title>\
<body style='margin:0;background:%233a7d44;color:white;font:32px sans-serif'>\
<h1 style='margin:24px'>Drawn by Chromium in a pane</h1></body>";

/// The `web-pane` page's color, which no scheme of the scene uses.
pub const WEB_PAGE_COLOR: u32 = 0x3a_7d_44;

/// The reference app with every text, input, and status component.
pub const COMPONENTS_APP: &str = "examples/apps/components/app.toml";

pub(crate) fn build(root: &Path) -> Result<(), String> {
    let build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-app",
            "-p",
            "dreamer-server",
            "-p",
            "dreamer-cli",
            "-p",
            "dreamer-termapp",
        ])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("building dreamer, dreamerd, dream, and dreamer-termapp failed".into());
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
    println!("scenes: save each capture under the name shown, outside the repository");
    println!("scenes: the agentic pane scene is not available yet (M6)");
    for skin in ["dark", "light"] {
        for scene in SCENES {
            if only.as_deref().is_some_and(|o| o != scene.name) {
                continue;
            }
            // Needs the app built with web panes: `screenshot --scene web-pane`.
            if scene.web.is_some() {
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
pub(crate) struct Running {
    app: std::process::Child,
    pub(crate) tmux: String,
    pub(crate) socket: String,
    pub(crate) dir: PathBuf,
}

impl Running {
    /// Stop the app, its daemon, and the tmux server; remove the files.
    pub(crate) fn finish(mut self) {
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
pub(crate) fn launch(
    root: &Path,
    scene: &Scene,
    skin: &str,
    extra: &str,
) -> Result<Running, String> {
    launch_on(root, scene, skin, extra, false)
}

/// Like [`launch`]; with `x11`, the app runs on X11 (`WAYLAND_DISPLAY`
/// removed: under a Wayland desktop that is XWayland).
pub(crate) fn launch_on(
    root: &Path,
    scene: &Scene,
    skin: &str,
    extra: &str,
    x11: bool,
) -> Result<Running, String> {
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let id = format!("{}-{skin}-{}", scene.name, std::process::id());
    let socket = format!("dreamer-scene-{id}");
    // Short, under /tmp: the daemon's sockets go in here, and a socket
    // path is limited to 104 bytes on macOS (the per-user temp directory
    // alone takes about 50).
    let dir = std::path::PathBuf::from(format!(
        "/tmp/drm-scene-{}-{}",
        &skin[..1],
        std::process::id()
    ));
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
    // A scene with a page gets a data directory of its own, with the
    // Dreamer Web component `cargo xtask web bundle` built added to it.
    let data = dir.join("data");
    if scene.web.is_some() {
        let component = crate::web::linux_component_dir(root);
        if component.is_dir() {
            crate::web::dream_at(
                &crate::util::cargo_target(root).join("debug/dream"),
                &["web", "install"],
                Some(&component),
                &[("DREAMER_DATA_DIR", data.clone().into_os_string())],
            )?;
        }
    }
    let mut app = Command::new(crate::util::cargo_target(root).join("debug/dreamer"));
    if x11 {
        app.env_remove("WAYLAND_DISPLAY");
    }
    if scene.web.is_some() {
        app.env("DREAMER_DATA_DIR", &data);
    }
    let app = app
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
    if let Some(app) = scene.app {
        show_app(root, &dir, &tmux, &socket, app)?;
    }
    if let Some(url) = scene.web {
        open_web(root, &dir, url)?;
    }
    Ok(Running {
        app,
        tmux,
        socket,
        dir,
    })
}

/// Open `url` in a new pane of session `main` with the scene's `dream web
/// open`, once the app's daemon answers.
fn open_web(root: &Path, dir: &Path, url: &str) -> Result<(), String> {
    let rt = dir.join("rt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !rt.join("dreamerd.sock").exists() {
        if std::time::Instant::now() > deadline {
            return Err("the scene's daemon did not start".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let out = Command::new(crate::util::cargo_target(root).join("debug/dream"))
        .args(["web", "open", url, "--session", "main"])
        .env("DREAMER_RUNTIME_DIR", &rt)
        .env("DREAMER_STATE_DIR", dir.join("st"))
        .env("DREAMER_CONFIG_DIR", dir.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output()
        .map_err(|e| format!("dream web open: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dream web open: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Capture the `web-pane` scene (the app built with web panes; Linux) on
/// X11 (`x11`) or Wayland, at `path`, and check that the page was drawn:
/// the capture is Dreamer's window and shows the page's color. A busy
/// machine may take a while to start Chromium, so the window is captured
/// again for up to 20 s while the page is missing. What it found.
pub fn capture_web(root: &Path, x11: bool, path: &Path) -> Result<String, String> {
    let scene = SCENES
        .iter()
        .find(|s| s.name == "web-pane")
        .ok_or("no web-pane scene")?;
    // Builds the component too, which the scene adds to its own data
    // directory.
    crate::web::linux_bundle(root, crate::package::Profile::Debug)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let running = launch_on(root, scene, "dark", "", x11)?;
    let check = |p: &Path| -> Result<String, String> {
        let window = share_near(p, 0x14_16_1a, 3)?;
        let page = share_of(p, WEB_PAGE_COLOR)?;
        let found = format!(
            "Dreamer's background {:.1}%, the page's #3a7d44 {:.1}%",
            window * 100.0,
            page * 100.0
        );
        if window < 0.01 {
            Err(format!(
                "the capture does not show Dreamer's window ({found})"
            ))
        } else if page < 0.02 {
            Err(format!("the page was not drawn ({found})"))
        } else {
            Ok(found)
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut result = capture_window(running.app.id(), path, 4.0).and_then(|()| check(path));
    while result
        .as_ref()
        .is_err_and(|e| e.contains("the page was not drawn"))
        && std::time::Instant::now() < deadline
    {
        result = capture_window(running.app.id(), path, 1.0).and_then(|()| check(path));
    }
    running.finish();
    result
}

/// Show the app package `app` in a new window of session `main`, once the
/// app's daemon answers: registered (`dream app run`) on the left, and
/// started from a shell on the right. Both get only the scene's private
/// directories.
fn show_app(root: &Path, dir: &Path, tmux: &str, socket: &str, app: &str) -> Result<(), String> {
    let debug = crate::util::cargo_target(root).join("debug");
    let manifest = root.join(app);
    let rt = dir.join("rt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !rt.join("dreamerd.sock").exists() {
        if std::time::Instant::now() > deadline {
            return Err("the scene's daemon did not start".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let env = [
        ("DREAMER_RUNTIME_DIR", rt.clone()),
        ("DREAMER_STATE_DIR", dir.join("st")),
        ("DREAMER_CONFIG_DIR", dir.join("config")),
    ];
    let termapp = debug.join("dreamer-termapp");
    let out = Command::new(debug.join("dream"))
        .args([
            "app",
            "run",
            "dreamer.components",
            "-L",
            socket,
            "--tmux",
            tmux,
        ])
        .args(["--name", "components", "--"])
        .arg(&termapp)
        .arg(&manifest)
        .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output()
        .map_err(|e| format!("dream app run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dream app run: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut split = Command::new(tmux);
    split.args(["-L", socket, "split-window", "-h", "-t", "main:{end}"]);
    for (k, v) in &env {
        split.args(["-e", &format!("{k}={}", v.display())]);
    }
    split
        .args(["-e", "DREAMER_FORBID_DEFAULT_TMUX=1"])
        .arg(format!("{} {}", termapp.display(), manifest.display()));
    let _ = split.status();
    let _ = Command::new(tmux)
        .args(["-L", socket, "select-window", "-t", "main:{end}"])
        .status();
    Ok(())
}

const SCREENSHOT_USAGE: &str = "\
usage: cargo xtask screenshot [--scene NAME] [--skin dark|light] [--out DIR]
                              [--settings YAML] [--settle SECONDS] [--x11]
       cargo xtask screenshot --switch THEME [--out DIR]

Opens a reference scene (default single-pane) against an isolated tmux
server and saves a PNG of the app's own window, at device resolution, to
DIR (default target/screenshots). --settings is appended to the scene's
dreamer.yaml, e.g. --settings 'font: {size: 15, letter_spacing: -0.3}'.
Only Dreamer's window is captured: on macOS with screencapture, on Linux\nunder Hyprland (Wayland or XWayland) with grim.

--x11 runs the app on X11 (WAYLAND_DISPLAY removed; XWayland under a
Wayland desktop). The web-pane scene (Linux) builds the app with web panes
(`cargo xtask deps web` first), opens a one-color page in a pane, and
checks that the capture shows it; it is saved as web-pane-wayland.png or
web-pane-x11.png.

--switch opens the theme-switch scene in the dark scheme, captures it,
changes terminal.theme to THEME in the running window's dreamer.yaml (as
the palette's Theme command does), and captures it again.";

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
    let mut switch: Option<String> = None;
    let mut x11 = false;
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
            "--switch" => switch = Some(value()?),
            "--x11" => x11 = true,
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
    if !cfg!(target_os = "macos") && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return Err("screenshot: on Linux, only Hyprland is supported so far".into());
    }
    if display_asleep() {
        return Err("screenshot: the display is asleep, so the window is not drawn".into());
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
    if let Some(theme) = switch {
        let (before, after) = (
            out.join("theme-switch-before.png"),
            out.join(format!("theme-switch-{theme}.png")),
        );
        capture_switch(&root, &theme, &before, &after)?;
        for p in [&before, &after] {
            println!(
                "screenshot: {} (Dreamer dark background {:.1}%, the pane's own #402020 {:.1}%)",
                p.display(),
                share_of(p, 0x14_16_1a)? * 100.0,
                share_of(p, 0x40_20_20)? * 100.0
            );
        }
        return Ok(());
    }
    if scene.web.is_some() {
        if cfg!(target_os = "macos") {
            return Err(
                "screenshot: the web-pane scene is for Linux; on macOS, `cargo xtask web smoke`"
                    .into(),
            );
        }
        let backend = if x11 { "x11" } else { "wayland" };
        let path = out.join(format!("{}-{backend}.png", scene.name));
        let found = capture_web(&root, x11, &path)?;
        println!("screenshot: {} ({found})", path.display());
        return Ok(());
    }
    if x11 && cfg!(target_os = "macos") {
        return Err("screenshot: --x11 is for Linux".into());
    }
    build(&root)?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let path = out.join(format!("{}-{skin}.png", scene.name));
    let running = launch_on(&root, scene, &skin, &extra, x11)?;
    let result = capture_window(running.app.id(), &path, settle);
    running.finish();
    result?;
    // A capture that is mostly not the skin's background did not get
    // Dreamer's window: a locked session draws its lock surface over it,
    // and Hyprland does not report the lock.
    let background = if skin == "dark" {
        0x14_16_1a
    } else {
        0xf6_f6_f4
    };
    // Within 3 per channel: a dark wallpaper comes within 12 of it.
    if share_near(&path, background, 3)? < 0.2 {
        let _ = std::fs::remove_file(&path);
        return Err("screenshot: the capture does not show Dreamer's window \
             (a locked session or another surface over it); nothing saved"
            .into());
    }
    println!("screenshot: {}", path.display());
    Ok(())
}

/// Capture `scene` in `skin` (built from `root`) to `path`: the gate's
/// pixel evidence (#89). The caller checks the display first.
pub fn capture(root: &Path, scene: &str, skin: &str, path: &Path) -> Result<(), String> {
    let scene = SCENES
        .iter()
        .find(|s| s.name == scene)
        .ok_or_else(|| format!("no scene `{scene}`"))?;
    build(root)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let running = launch(root, scene, skin, "")?;
    let result = capture_window(running.app.id(), path, 3.0);
    running.finish();
    result
}

/// Like [`capture`], then `check` the image; while it fails, capture the
/// same window again, for up to 15 s more. A busy machine can take longer
/// than the settle time to connect and draw the scene; a scene that never
/// draws still fails, with the last check's error.
pub fn capture_checked(
    root: &Path,
    scene: &str,
    skin: &str,
    path: &Path,
    check: impl Fn(&Path) -> Result<String, String>,
) -> Result<String, String> {
    let scene = SCENES
        .iter()
        .find(|s| s.name == scene)
        .ok_or_else(|| format!("no scene `{scene}`"))?;
    build(root)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let running = launch(root, scene, skin, "")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut result = capture_window(running.app.id(), path, 3.0).and_then(|()| check(path));
    while result
        .as_ref()
        .is_err_and(|e| !e.contains("could not capture window"))
        && std::time::Instant::now() < deadline
    {
        result = capture_window(running.app.id(), path, 1.0).and_then(|()| check(path));
    }
    running.finish();
    result
}

/// The live theme switch (#265): `theme-switch` captured in Dreamer's dark
/// scheme, then again after `terminal.theme` in its `dreamer.yaml` changes
/// to `theme` (what the palette's Theme command writes), in the same
/// running window.
pub fn capture_switch(root: &Path, theme: &str, before: &Path, after: &Path) -> Result<(), String> {
    let scene = SCENES
        .iter()
        .find(|s| s.name == "theme-switch")
        .ok_or("no theme-switch scene")?;
    build(root)?;
    if let Some(dir) = before.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let running = launch(root, scene, "dark", "")?;
    // Once the panes are live: the program sets its own background.
    std::thread::sleep(std::time::Duration::from_secs(4));
    let _ = Command::new(&running.tmux)
        .args(["-L", &running.socket, "send-keys", "-t", "main:0.1"])
        .args(["printf '\\033]11;#402020\\007'", "Enter"])
        .status();
    let result = capture_window(running.app.id(), before, 2.0).and_then(|()| {
        std::fs::write(
            running.dir.join("config/dreamer.yaml"),
            format!("appearance: dark\nterminal:\n  theme: {theme}\n"),
        )
        .map_err(|e| format!("config: {e}"))?;
        // Once the switch is applied (the config is read every second),
        // the wide pane prints a block in SWITCH_MARKER. An `after`
        // capture without it comes from a window that stopped repainting,
        // not from a switch that missed a pane.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = Command::new(&running.tmux)
            .args(["-L", &running.socket, "send-keys", "-t", "main:0.0"])
            .args(["printf '\\033[48;2;32;96;160m%16s\\033[0m\\n' ''", "Enter"])
            .status();
        // Capture again until the marker and the new scheme are both drawn,
        // for up to 12 s; the checks then judge the last capture.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        loop {
            capture_window(running.app.id(), after, 1.0)?;
            let marked = switch_marked(after)?;
            let switched = share_of(after, 0x14_16_1a)? <= 0.005;
            if (marked && switched) || std::time::Instant::now() > deadline {
                return Ok(());
            }
        }
    });
    running.finish();
    result
}

/// The block `capture_switch` prints after the theme switch: a blue no
/// scheme in the check uses.
pub const SWITCH_MARKER: u32 = 0x20_60_a0;

/// Whether a capture shows the switch marker. A saturated blue moves far
/// when the capture is in the display's color space (sRGB `#2060a0` is
/// captured as about `#375e9c` on a Display P3 screen), so it is matched
/// within 32 steps per channel; no color of the switched-to scheme is that
/// close.
pub fn switch_marked(path: &Path) -> Result<bool, String> {
    Ok(share_near(path, SWITCH_MARKER, 32)? >= 0.000_2)
}

/// The share of a capture's pixels within a few steps of `color` (12 per
/// channel: the display profile moves saturated colors more than grays).
pub fn share_of(path: &Path, color: u32) -> Result<f64, String> {
    share_near(path, color, 12)
}

/// Like [`share_of`], within `tolerance` per channel: a saturated color
/// can move further than a neutral one when the capture is color managed.
pub fn share_near(path: &Path, color: u32, tolerance: i32) -> Result<f64, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let channels = if info.color_type == png::ColorType::Rgba {
        4
    } else {
        3
    };
    let want = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
    let (mut near, mut all) = (0usize, 0usize);
    for px in buf[..info.buffer_size()].chunks_exact(channels) {
        all += 1;
        if px[..3]
            .iter()
            .zip(want)
            .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= tolerance)
        {
            near += 1;
        }
    }
    Ok(near as f64 / all.max(1) as f64)
}

/// The share of the pixels inside `rect` (points, from the window's top
/// left; `window_width` in points gives the capture's scale) within 30 per
/// channel of `color`, and the most common color there. A few points at
/// the edges are left out (borders, antialiasing).
pub fn share_in(
    path: &Path,
    color: u32,
    window_width: f64,
    rect: (f64, f64, f64, f64),
) -> Result<(f64, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => return Err(format!("{}: color type {other:?}", path.display())),
    };
    let scale = f64::from(info.width) / window_width.max(1.0);
    let inset = 4.0;
    let (x, y, w, h) = rect;
    let px = |v: f64, max: u32| ((v * scale).round().max(0.0) as u32).min(max);
    let (x0, x1) = (px(x + inset, info.width), px(x + w - inset, info.width));
    let (y0, y1) = (px(y + inset, info.height), px(y + h - inset, info.height));
    if x1 <= x0 || y1 <= y0 {
        return Err(format!("{}: the rectangle is empty", path.display()));
    }
    let want = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
    let mut counts: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
    let (mut near, mut all) = (0usize, 0usize);
    let stride = info.line_size;
    for row in y0..y1 {
        for col in x0..x1 {
            let at = row as usize * stride + col as usize * channels;
            let c = [buf[at], buf[at + 1], buf[at + 2]];
            *counts.entry(c).or_default() += 1;
            all += 1;
            if c.iter()
                .zip(want)
                .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 30)
            {
                near += 1;
            }
        }
    }
    let main = counts.iter().max_by_key(|(_, n)| **n).map_or(0, |(c, _)| {
        u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2])
    });
    Ok((near as f64 / all.max(1) as f64, main))
}

/// What a capture shows, checked against the scheme it was drawn in: the
/// dominant color must be the scheme's background (the theme reached the
/// terminals and the chrome alike), `ink` (a color the scene prints) must
/// appear, and the image must not be flat. A one-line summary, or why not.
pub fn check_capture(path: &Path, background: u32, ink: u32) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => {
            return Err(format!(
                "{}: unexpected color type {other:?}",
                path.display()
            ));
        }
    };
    let rgb = |hex: u32| [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8];
    let near = |a: [u8; 3], b: [u8; 3], t: i32| {
        a.iter()
            .zip(b.iter())
            .all(|(x, y)| (i32::from(*x) - i32::from(*y)).abs() <= t)
    };
    let mut counts: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
    let mut inked = 0usize;
    let (bg, ink) = (rgb(background), rgb(ink));
    for px in buf[..info.buffer_size()].chunks_exact(channels) {
        let c = [px[0], px[1], px[2]];
        *counts.entry(c).or_default() += 1;
        // Thin glyphs are antialiased toward the background: on a light
        // one the strongest pixels of green text sit about 30 per channel
        // from the scheme's green, while every other color drawn is more
        // than 80 away.
        if near(c, ink, 40) {
            inked += 1;
        }
    }
    let (mode, _) = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .ok_or_else(|| format!("{}: empty image", path.display()))?;
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    if !near(*mode, bg, 3) {
        return Err(format!(
            "{}: the main color is {}, not the scheme's background {}",
            path.display(),
            hex(*mode),
            hex(bg)
        ));
    }
    if counts.len() < 16 {
        return Err(format!(
            "{}: only {} colors; nothing drawn",
            path.display(),
            counts.len()
        ));
    }
    if inked == 0 {
        return Err(format!(
            "{}: no pixel near {}, the scheme's color the scene prints",
            path.display(),
            hex(ink)
        ));
    }
    Ok(format!(
        "{}x{}, background {}, {} colors, {inked} pixels of {}",
        info.width,
        info.height,
        hex(*mode),
        counts.len(),
        hex(ink)
    ))
}

/// The on-screen, normal-layer window owned by `pid`, if any.
/// The app's window: its window number on macOS.
#[cfg(target_os = "macos")]
fn window_of(pid: u32) -> Option<String> {
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
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    id.parse::<u32>().ok().map(|_| id)
}

/// The app's window on Linux: its rectangle in the compositor's layout,
/// as `grim -g` takes it (`x,y wxh`). Hyprland only: it lists windows
/// with their process, for Wayland and XWayland clients alike.
#[cfg(not(target_os = "macos"))]
fn window_of(pid: u32) -> Option<String> {
    let out = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    let clients: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let w = clients.as_array()?.iter().find(|c| {
        c["pid"].as_u64() == Some(u64::from(pid))
            && c["mapped"].as_bool() == Some(true)
            && c["hidden"].as_bool() != Some(true)
    })?;
    let (at, size) = (w["at"].as_array()?, w["size"].as_array()?);
    Some(format!(
        "{},{} {}x{}",
        at.first()?.as_i64()?,
        at.get(1)?.as_i64()?,
        size.first()?.as_i64()?,
        size.get(1)?.as_i64()?
    ))
}

/// Save the window `id` (from [`window_of`]) as a PNG at `path`.
#[cfg(target_os = "macos")]
fn grab(id: &str, path: &Path) -> Result<(), String> {
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

/// Save the window's rectangle as a PNG at `path`, at the output's scale.
/// Windows tile on Hyprland, so the rectangle holds only Dreamer's window.
#[cfg(not(target_os = "macos"))]
fn grab(rect: &str, path: &Path) -> Result<(), String> {
    let status = Command::new("grim")
        .args(["-g", rect])
        .arg(path)
        .output()
        .map_err(|e| format!("grim: {e}"))?;
    if !path.is_file() {
        return Err(format!(
            "grim could not capture window {rect}: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
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
    // The window may have moved while it settled (a tiling compositor
    // places it after its first frame).
    let id = window_of(pid).unwrap_or(id);
    grab(&id, path)
}

/// Every output is off (DPMS), so nothing is drawn: Linux under Hyprland.
/// macOS reports a locked screen instead (`package::screen_locked`).
pub(crate) fn display_asleep() -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    let Some(out) = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()
    else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.as_array().map(|m| {
                !m.is_empty() && m.iter().all(|o| o["dpmsStatus"].as_bool() == Some(false))
            })
        })
        .unwrap_or(false)
}
