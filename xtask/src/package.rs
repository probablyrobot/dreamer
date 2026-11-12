//! Packages. See `docs/packaging.md`.
//!
//! - `cargo xtask package --platform macos [--debug]` (or `current` on a
//!   Mac) writes `target/package/Dreamer.app` with `dreamer`, `dreamerd`,
//!   and `dream`, plus `dreamer-<version>-aarch64-apple-darwin.zip` and its
//!   `.sha256`. Bundles are ad hoc signed; Developer ID signing and
//!   notarization are a separate release step (spec 3.2, 17.5).
//! - `cargo xtask package --platform arch [--source-only] [--nodeps]` writes
//!   `target/package/arch/` with a `git archive` source tarball and a
//!   PKGBUILD, then runs `makepkg` when it is available.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::repo_root;

const BUNDLE_ID: &str = "dev.dreamer.Dreamer";
const MIN_MACOS: &str = "15.0";
/// Executables in `Contents/MacOS`; the first is the bundle's main one.
const EXECUTABLES: [&str; 3] = ["dreamer", "dreamerd", "dream"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut profile = Profile::Release;
    let mut platform = "current".to_owned();
    let mut source_only = false;
    let mut nodeps = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--platform" => platform = iter.next().ok_or("--platform needs a value")?.clone(),
            "--debug" => profile = Profile::Debug,
            "--source-only" => source_only = true,
            "--nodeps" => nodeps = true,
            other => return Err(format!("unknown package argument `{other}`")),
        }
    }
    match platform.as_str() {
        "arch" => return arch_package(&repo_root(), source_only, nodeps),
        "macos" => {}
        "current" if cfg!(target_os = "macos") => {}
        "current" => return Err("on Linux, choose a package: --platform arch".into()),
        other => return Err(format!("unknown platform `{other}` (macos, arch)")),
    }
    if !cfg!(target_os = "macos") {
        return Err("the macOS bundle is built on a Mac".into());
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

fn sha256_of(path: &Path) -> Result<String, String> {
    crate::util::sha256_file(path)
}

/// Source tarball plus PKGBUILD in `target/package/arch`, then `makepkg`.
fn arch_package(root: &Path, source_only: bool, nodeps: bool) -> Result<(), String> {
    let version = version(root)?;
    let out = root.join("target").join("package").join("arch");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let dirty = run_checked(
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root),
        "git status",
    )?;
    if !dirty.trim().is_empty() {
        println!("package: the work tree has uncommitted changes; packaging HEAD only");
    }
    let tarball = out.join(format!("dreamer-{version}.tar.gz"));
    run_checked(
        Command::new("git")
            .args(["archive", "--format=tar.gz"])
            .arg(format!("--prefix=dreamer-{version}/"))
            .arg("-o")
            .arg(&tarball)
            .arg("HEAD")
            .current_dir(root),
        "git archive",
    )?;
    let sum = sha256_of(&tarball)?;
    let template = std::fs::read_to_string(root.join("packaging/arch/PKGBUILD.in"))
        .map_err(|e| format!("PKGBUILD.in: {e}"))?;
    let pkgbuild = template
        .replace("@VERSION@", &version)
        .replace("@SHA256@", &sum);
    std::fs::write(out.join("PKGBUILD"), pkgbuild).map_err(|e| e.to_string())?;
    println!("package: {}", tarball.display());
    println!("package: {}", out.join("PKGBUILD").display());
    let has_makepkg = Command::new("makepkg").arg("--version").output().is_ok();
    if source_only || !has_makepkg {
        if !source_only {
            println!(
                "package: makepkg not found; run `makepkg` in {} on Arch",
                out.display()
            );
        }
        return Ok(());
    }
    let mut makepkg = Command::new("makepkg");
    makepkg.args(["--force", "--noconfirm"]);
    if nodeps {
        // Build tools not installed through pacman (for example rustup from
        // its own installer) are fine; skip makepkg's package check.
        makepkg.arg("--nodeps");
    }
    let status = makepkg
        .current_dir(&out)
        .status()
        .map_err(|e| format!("makepkg: {e}"))?;
    if !status.success() {
        return Err("makepkg failed".into());
    }
    for entry in std::fs::read_dir(&out)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".pkg.tar.zst") {
            println!("package: {}", entry.path().display());
        }
    }
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
        .args([
            "build",
            "--locked",
            "--package",
            "dreamer-app",
            "--package",
            "dreamer-server",
            "--package",
            "dreamer-cli",
        ])
        .current_dir(root);
    if profile == Profile::Release {
        cargo.arg("--release");
    }
    let status = cargo.status().map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build of dreamer, dreamerd, and dream failed".into());
    }
    let bin_dir = root.join("target").join(if profile == Profile::Release {
        "release"
    } else {
        "debug"
    });

    let app = out.join("Dreamer.app");
    if app.exists() {
        std::fs::remove_dir_all(&app).map_err(|e| e.to_string())?;
    }
    let macos = app.join("Contents").join("MacOS");
    let resources = app.join("Contents").join("Resources");
    std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&resources).map_err(|e| e.to_string())?;
    // The app starts dreamerd from its own directory; dream is the CLI.
    for exe in EXECUTABLES {
        std::fs::copy(bin_dir.join(exe), macos.join(exe))
            .map_err(|e| format!("copy {exe}: {e}"))?;
    }
    // The default terminal font, so the app looks the same on a Mac without
    // it installed. SIL OFL 1.1; its license travels with it.
    let fonts = crate::deps::ensure_fonts(&crate::manifest::Manifest::load()?)?;
    let font_dir = resources.join("Fonts");
    std::fs::create_dir_all(&font_dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&fonts)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        std::fs::copy(entry.path(), font_dir.join(entry.file_name()))
            .map_err(|e| format!("copy font: {e}"))?;
    }
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
    // Helpers are signed before the bundle that seals them.
    for exe in &EXECUTABLES[1..] {
        run_checked(
            Command::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(macos.join(exe)),
            "codesign",
        )?;
    }
    run_checked(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&app),
        "codesign",
    )?;
    Ok(app)
}

/// Architecture, deployment target, and signature of every executable, and
/// the helpers run from inside the bundle.
pub fn verify_bundle(app: &Path) -> Result<(), String> {
    run_checked(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep", "--verbose=1"])
            .arg(app),
        "codesign --verify",
    )?;
    for name in EXECUTABLES {
        let exe = app.join("Contents").join("MacOS").join(name);
        let archs = run_checked(Command::new("lipo").arg("-archs").arg(&exe), "lipo")?;
        if archs.trim() != "arm64" {
            return Err(format!("{name}: architectures `{}`", archs.trim()));
        }
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
                "{name}: deployment target {found}, expected {MIN_MACOS} (MACOSX_DEPLOYMENT_TARGET)"
            ));
        }
    }
    let fonts = app.join("Contents/Resources/Fonts");
    let ttf = std::fs::read_dir(&fonts)
        .map_err(|e| format!("{}: {e}", fonts.display()))?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "ttf"))
        .count();
    if ttf < 4 || !fonts.join("OFL.txt").is_file() {
        return Err(format!(
            "{}: expected the four font styles and OFL.txt",
            fonts.display()
        ));
    }
    let macos = app.join("Contents").join("MacOS");
    let daemon = run_checked(
        Command::new(macos.join("dreamerd")).arg("--self-check"),
        "dreamerd",
    )?;
    let cli = run_checked(Command::new(macos.join("dream")).arg("--version"), "dream")?;
    println!(
        "package: dreamer, dreamerd, dream: arm64, ad hoc signed, minimum macOS {MIN_MACOS}; {}; {}",
        daemon.trim(),
        cli.trim()
    );
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
    // The packaged app must paint with its default font, never a fallback.
    let font_ok = stdout.contains("smoke: font JetBrainsMono Nerd Font Mono ");
    if !out.status.success() || !stdout.contains("smoke: frame presented") || !font_ok {
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
