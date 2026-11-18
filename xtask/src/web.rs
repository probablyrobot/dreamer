//! `cargo xtask web bundle [--release]`: the desktop with web panes, as a
//! development bundle (`<target>/web-bundle/Dreamer.app`): the app built
//! with its `web` feature, the CEF framework, and the five helper apps CEF
//! starts its renderer, GPU, and utility processes from. macOS only.
//!
//! The pinned CEF distribution, cmake, and Ninja come from `cargo xtask
//! deps web`. The release package does not include web panes yet.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::WebDeps;
use crate::package::Profile;
use crate::util::{cargo_target, repo_root};

/// The framework's directory name, in the distribution and the bundle.
const FRAMEWORK: &str = "Chromium Embedded Framework.framework";

/// The helper apps CEF looks for beside the framework, named after the
/// main executable (`dreamer`): `dreamer Helper.app`, `dreamer Helper
/// (GPU).app`, and so on, each with the helper executable.
const HELPERS: [(&str, &str); 5] = [
    ("", "helper"),
    (" (GPU)", "helper.gpu"),
    (" (Renderer)", "helper.renderer"),
    (" (Plugin)", "helper.plugin"),
    (" (Alerts)", "helper.alerts"),
];

const HELPER_EXE: &str = "dreamer-web-helper";

pub fn run(args: &[String]) -> Result<(), String> {
    let mut profile = Profile::Debug;
    match args {
        [cmd, rest @ ..] if cmd == "bundle" => {
            for a in rest {
                match a.as_str() {
                    "--release" => profile = Profile::Release,
                    other => return Err(format!("unknown web bundle argument `{other}`")),
                }
            }
        }
        _ => return Err("usage: cargo xtask web bundle [--release]".into()),
    }
    let web = provisioned()?
        .ok_or("the web engine's dependencies are not provisioned: run `cargo xtask deps web`")?;
    let root = repo_root();
    let app = bundle(&root, profile, &web)?;
    println!("web: {}", app.display());
    Ok(())
}

/// The web engine's dependencies, if `cargo xtask deps web` provisioned
/// them; `None` on a host without web panes.
pub fn provisioned() -> Result<Option<WebDeps>, String> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    crate::deps::web_deps(&crate::manifest::Manifest::load()?)
}

/// Build and verify the development bundle in `root`'s target directory.
pub fn bundle(root: &Path, profile: Profile, web: &WebDeps) -> Result<PathBuf, String> {
    let out = cargo_target(root).join("web-bundle");
    let app = crate::package::build_bundle_with(root, profile, &out, Some(web))?;
    crate::package::verify_bundle(&app)?;
    verify_engine(&app)?;
    Ok(app)
}

fn run_checked(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sign(path: &Path) -> Result<(), String> {
    run_checked(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(path),
        "codesign",
    )
}

/// Put the framework and the helper apps into `app` (before the app
/// itself is signed), each signed ad hoc.
pub fn add_engine(app: &Path, bin_dir: &Path, web: &WebDeps, version: &str) -> Result<(), String> {
    let frameworks = app.join("Contents").join("Frameworks");
    std::fs::create_dir_all(&frameworks).map_err(|e| e.to_string())?;
    let framework = frameworks.join(FRAMEWORK);
    if framework.exists() {
        std::fs::remove_dir_all(&framework).map_err(|e| e.to_string())?;
    }
    // ditto keeps the framework's links and permissions.
    run_checked(
        Command::new("ditto").arg(web.framework()).arg(&framework),
        "ditto",
    )?;
    sign(&framework)?;
    let helper = bin_dir.join(HELPER_EXE);
    for (suffix, id) in HELPERS {
        let name = format!("dreamer Helper{suffix}");
        let contents = frameworks.join(format!("{name}.app")).join("Contents");
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
        std::fs::copy(&helper, macos.join(&name)).map_err(|e| format!("copy {HELPER_EXE}: {e}"))?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>{name}</string>
  <key>CFBundleIdentifier</key><string>dev.dreamer.Dreamer.{id}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>{name}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><string>1</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
"#
        );
        std::fs::write(contents.join("Info.plist"), plist).map_err(|e| e.to_string())?;
        sign(&contents.join("MacOS").join(&name))?;
        sign(&frameworks.join(format!("{name}.app")))?;
    }
    Ok(())
}

/// The framework and every helper app are in place, and a helper loads the
/// framework from where it sits. Opens no window.
pub fn verify_engine(app: &Path) -> Result<(), String> {
    let frameworks = app.join("Contents/Frameworks");
    let binary = frameworks
        .join(FRAMEWORK)
        .join("Chromium Embedded Framework");
    if !binary.is_file() {
        return Err(format!("{} is missing", binary.display()));
    }
    for (suffix, _) in HELPERS {
        let name = format!("dreamer Helper{suffix}");
        let exe = frameworks.join(format!("{name}.app/Contents/MacOS/{name}"));
        if !exe.is_file() {
            return Err(format!("{} is missing", exe.display()));
        }
    }
    // Without a process type CEF's entry point returns at once (-1): this
    // checks only that the helper finds and loads the framework.
    let exe = frameworks.join("dreamer Helper.app/Contents/MacOS/dreamer Helper");
    let out = Command::new(&exe)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(255) || stderr.contains("dreamer-web-helper:") {
        return Err(format!(
            "the helper did not load the framework ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    let size = Command::new("du")
        .args(["-sh"])
        .arg(app)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    println!(
        "web: CEF framework and {} helper apps in place, a helper loads the framework; bundle {size}",
        HELPERS.len()
    );
    Ok(())
}

/// What the web smoke saw, or why it could not judge.
pub enum Smoke {
    Passed(Vec<String>),
    /// The window or the page was not being drawn.
    NotRun(String),
}

/// The page's two colors (`PAGE` in the app's web smoke).
const TOP: u32 = 0x2a_6f_db;
const BOTTOM: u32 = 0xd9_82_2b;

/// Run the bundle's `dreamer --smoke-web` against an isolated tmux server
/// with private daemon, desktop, and configuration directories, then check
/// its captures: the page's top color fills the pane's rectangle, and after
/// the scroll its bottom color does.
pub fn smoke(app: &Path, out: &Path) -> Result<Smoke, String> {
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let pid = std::process::id();
    let socket = format!("dreamer-gate-web-{pid}");
    let root = PathBuf::from(format!("/tmp/drm-gate-web-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(root.join("config")).map_err(|e| e.to_string())?;
    let started = Command::new(&tmux)
        .args(["-L", &socket, "-f", "/dev/null", "new-session", "-d"])
        .args(["-s", "main", "-x", "120", "-y", "36"])
        .arg("env PS1='$ ' /bin/sh")
        .status()
        .map_err(|e| format!("{tmux}: {e}"))?;
    if !started.success() {
        return Err("could not start an isolated tmux server".into());
    }
    let result = Command::new(app.join("Contents/MacOS/dreamer"))
        .arg("--smoke-web")
        .arg(out)
        .args(["-L", &socket, "--tmux", &tmux])
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_DESKTOP_STATE_DIR", root.join("desktop"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .output();
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
    let result = result.map_err(|e| format!("dreamer: {e}"))?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&result.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    match result.status.code() {
        Some(0) => {}
        Some(5) => return Ok(Smoke::NotRun(lines.join("\n"))),
        _ => {
            return Err(format!(
                "web smoke failed ({}): {}\n{}",
                result.status,
                lines.join("\n"),
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
    }
    let rect =
        std::fs::read_to_string(out.join("rect.txt")).map_err(|e| format!("rect.txt: {e}"))?;
    let v: Vec<f64> = rect
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let [x, y, w, h, window_w, _] = v[..] else {
        return Err(format!("rect.txt: `{}`", rect.trim()));
    };
    for (name, color) in [("before.png", TOP), ("after.png", BOTTOM)] {
        let path = out.join(name);
        let (share, main) = crate::scenes::share_in(&path, color, window_w, (x, y, w, h))?;
        lines.push(format!(
            "{}: #{color:06x} covers {:.1}% of the pane (most common #{main:06x})",
            path.display(),
            share * 100.0
        ));
        if share < 0.9 {
            return Err(format!(
                "{}: the page's #{color:06x} covers only {:.1}% of the web pane's rectangle (most common #{main:06x})\n{}",
                path.display(),
                share * 100.0,
                lines.join("\n")
            ));
        }
    }
    Ok(Smoke::Passed(lines))
}
