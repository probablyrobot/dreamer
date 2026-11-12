//! macOS application bundle: build, ad hoc sign, verify, launch, and zip.
//!
//! `cargo xtask package --platform current [--debug]` writes
//! `target/package/Dreamer.app`, `dreamer-<version>-<target>.zip`, and its
//! `.sha256`. Development bundles are ad hoc signed; Developer ID signing and
//! notarization are a separate release step (spec 3.2, 17.5).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::repo_root;

const BUNDLE_ID: &str = "dev.dreamer.Dreamer";
const MIN_MACOS: &str = "15.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut profile = Profile::Release;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--platform" => {
                let p = iter.next().ok_or("--platform needs a value")?;
                if p != "current" {
                    return Err("only --platform current is supported".into());
                }
            }
            "--debug" => profile = Profile::Debug,
            other => return Err(format!("unknown package argument `{other}`")),
        }
    }
    if !cfg!(target_os = "macos") {
        return Err("Linux packaging arrives with M8; this builds the macOS bundle".into());
    }
    let root = repo_root();
    let out = root.join("target").join("package");
    let app = build_bundle(&root, profile, &out)?;
    verify_bundle(&app)?;
    let zip = zip_bundle(&app, &out)?;
    println!("package: {}", app.display());
    println!("package: {}", zip.display());
    Ok(())
}

fn version(root: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    text.lines()
        .skip_while(|l| l.trim() != "[workspace.package]")
        .find_map(|l| {
            let v = l.trim().strip_prefix("version")?.trim().strip_prefix('=')?;
            Some(v.trim().trim_matches('"').to_owned())
        })
        .ok_or_else(|| "workspace version not found".into())
}

fn run_checked(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build `dreamer` in `root` and assemble `out/Dreamer.app`.
pub fn build_bundle(root: &Path, profile: Profile, out: &Path) -> Result<PathBuf, String> {
    let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo
        .args(["build", "--locked", "--package", "dreamer-app"])
        .current_dir(root);
    if profile == Profile::Release {
        cargo.arg("--release");
    }
    let status = cargo.status().map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build -p dreamer-app failed".into());
    }
    let binary = root
        .join("target")
        .join(if profile == Profile::Release {
            "release"
        } else {
            "debug"
        })
        .join("dreamer");

    let app = out.join("Dreamer.app");
    if app.exists() {
        std::fs::remove_dir_all(&app).map_err(|e| e.to_string())?;
    }
    let macos = app.join("Contents").join("MacOS");
    let resources = app.join("Contents").join("Resources");
    std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&resources).map_err(|e| e.to_string())?;
    std::fs::copy(&binary, macos.join("dreamer")).map_err(|e| format!("copy binary: {e}"))?;
    for notice in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"] {
        std::fs::copy(root.join(notice), resources.join(notice))
            .map_err(|e| format!("copy {notice}: {e}"))?;
    }
    let version = version(root)?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>dreamer</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Dreamer</string>
  <key>CFBundleDisplayName</key><string>Dreamer</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>{MIN_MACOS}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
"#
    );
    std::fs::write(app.join("Contents").join("Info.plist"), plist).map_err(|e| e.to_string())?;
    // Ad hoc signature: enough to run locally and to give the bundle a
    // stable code identity. Public builds are Developer ID signed later.
    run_checked(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&app),
        "codesign",
    )?;
    Ok(app)
}

/// Architecture and signature checks.
pub fn verify_bundle(app: &Path) -> Result<(), String> {
    let exe = app.join("Contents").join("MacOS").join("dreamer");
    let archs = run_checked(Command::new("lipo").arg("-archs").arg(&exe), "lipo")?;
    if archs.trim() != "arm64" {
        return Err(format!(
            "bundle executable architectures: `{}`",
            archs.trim()
        ));
    }
    run_checked(
        Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=1"])
            .arg(app),
        "codesign --verify",
    )?;
    let minos = run_checked(
        Command::new("vtool").args(["-show-build"]).arg(&exe),
        "vtool",
    )
    .unwrap_or_default();
    let found = minos
        .lines()
        .find_map(|l| l.trim().strip_prefix("minos").map(str::trim))
        .unwrap_or("unknown")
        .to_owned();
    if found != MIN_MACOS {
        return Err(format!(
            "executable deployment target is {found}, expected {MIN_MACOS} (MACOSX_DEPLOYMENT_TARGET)"
        ));
    }
    println!("package: arm64, ad hoc signed, minimum macOS {found}");
    Ok(())
}

/// Run the bundle's executable with `--smoke` and require a presented frame.
pub fn launch_smoke(app: &Path) -> Result<String, String> {
    let exe = app.join("Contents").join("MacOS").join("dreamer");
    let out = Command::new(&exe)
        .arg("--smoke")
        .output()
        .map_err(|e| format!("launch {}: {e}", exe.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() || !stdout.contains("smoke: frame presented") {
        return Err(format!(
            "launch smoke failed ({}): {}{}",
            out.status,
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(stdout)
}

/// True when this process can reach the logged-in user's window server:
/// launchd reports an Aqua session, or (for tools started from a background
/// context such as an agent) the console belongs to this user and the
/// process is not running over SSH.
/// Whether the console session's screen is locked. While it is, macOS
/// stops refreshing windows after their first frame, so checks that need a
/// repaint prove nothing. `None` when the state cannot be read.
pub fn screen_locked() -> Option<bool> {
    const PROBE: &str = r#"
import ctypes, ctypes.util
cg = ctypes.cdll.LoadLibrary(ctypes.util.find_library("CoreGraphics"))
cf = ctypes.cdll.LoadLibrary(ctypes.util.find_library("CoreFoundation"))
cg.CGSessionCopyCurrentDictionary.restype = ctypes.c_void_p
d = cg.CGSessionCopyCurrentDictionary()
if not d:
    print("unknown")
else:
    cf.CFCopyDescription.restype = ctypes.c_void_p
    cf.CFCopyDescription.argtypes = [ctypes.c_void_p]
    cf.CFStringGetCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_long, ctypes.c_uint32]
    buf = ctypes.create_string_buffer(16384)
    cf.CFStringGetCString(cf.CFCopyDescription(d), buf, 16384, 0x08000100)
    print("locked" if "CGSSessionScreenIsLocked = 1" in buf.value.decode() else "unlocked")
"#;
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("/usr/bin/python3")
        .args(["-I", "-c", PROBE])
        .output()
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "locked" => Some(true),
        "unlocked" => Some(false),
        _ => None,
    }
}

pub fn gui_session_available() -> bool {
    let aqua = Command::new("launchctl")
        .arg("managername")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "Aqua");
    if aqua {
        return true;
    }
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return false;
    }
    let console_user = Command::new("stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let me = std::env::var("USER").unwrap_or_default();
    !me.is_empty() && console_user == me
}

pub fn zip_bundle(app: &Path, out: &Path) -> Result<PathBuf, String> {
    let root = repo_root();
    let zip = out.join(format!(
        "dreamer-{}-aarch64-apple-darwin.zip",
        version(&root)?
    ));
    let _ = std::fs::remove_file(&zip);
    run_checked(
        Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(app)
            .arg(&zip),
        "ditto",
    )?;
    let sum = run_checked(
        Command::new("shasum").args(["-a", "256"]).arg(&zip),
        "shasum",
    )?;
    let file_name = zip
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let hash = sum.split_whitespace().next().unwrap_or_default();
    std::fs::write(
        zip.with_extension("zip.sha256"),
        format!("{hash}  {file_name}\n"),
    )
    .map_err(|e| e.to_string())?;
    Ok(zip)
}

/// Unpack a bundle zip outside the source tree and launch it from there.
pub fn smoke_from_zip(zip: &Path) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("dreamer-unpacked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    run_checked(
        Command::new("ditto").args(["-x", "-k"]).arg(zip).arg(&dir),
        "ditto -x",
    )?;
    let app = dir.join("Dreamer.app");
    verify_bundle(&app)?;
    let result = launch_smoke(&app);
    let _ = std::fs::remove_dir_all(&dir);
    result
}
