//! Packages. See `docs/packaging.md`.
//!
//! - `cargo xtask package --platform macos [--debug]` (or `current` on a
//!   Mac) writes `target/package/Dreamer.app` with `dreamer`, `dreamerd`,
//!   `dream`, and `dreamer-termapp`, plus `dreamer-<version>-aarch64-apple-darwin.zip` and its
//!   `.sha256`. Bundles are ad hoc signed; with `--sign IDENTITY` they
//!   are Developer ID signed under the hardened runtime, and with
//!   `--notarize PROFILE` notarized and stapled (spec 3.2, 17.5). The app is
//!   built with web panes, but Chromium is not in it: beside it go
//!   `target/package/dreamer-web/` and
//!   `dreamer-web-<version>-aarch64-apple-darwin.zip` (with `.sha256` and
//!   `.json`), the Dreamer Web component ([`crate::web`]), which users add
//!   with `dream web install`. This needs `cargo xtask deps web`'s pins,
//!   fetched (checksum-verified) when missing.
//! - `cargo xtask package --fresh --platform macos` installs that component
//!   from its zip with the unpacked app's `dream`, into an empty data
//!   directory away from the tree ([`crate::fresh::mac_check`]).
//! - `cargo xtask package --platform linux` (on Linux) writes
//!   `target/package/linux/dreamer-<target>.tar.gz` and its `.sha256`: a
//!   self-contained release that `packaging/install.sh` installs into the
//!   user's home directory.
//! - `cargo xtask package --platform server [--target TRIPLE]` writes
//!   `target/package/server/dreamer-server-<version>-<target>.tar.gz` and
//!   its `.sha256`: the headless server and CLI (`dreamerd`, `dream`,
//!   `dreamer-termapp`) with nothing graphical, for installing on a host
//!   over SSH. It is unpacked away from the tree and checked after packing.
//! - `cargo xtask package --platform arch [--source-only] [--nodeps]` writes
//!   `target/package/arch/` with a `git archive` source tarball and a
//!   PKGBUILD, then runs `makepkg` when it is available.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::repo_root;

const BUNDLE_ID: &str = "dev.dreamer.Dreamer";
const MIN_MACOS: &str = "15.0";
/// Executables in `Contents/MacOS` and the Linux `bin/`; the first is the
/// bundle's main one. `dreamer-termapp` runs pane apps and filter panes;
/// the app and `dream` look for it beside themselves.
const EXECUTABLES: [&str; 4] = ["dreamer", "dreamerd", "dream", "dreamer-termapp"];

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
    let mut target: Option<String> = None;
    let mut container = false;
    let mut fresh = false;
    let mut arch = crate::fresh::native_arch().to_owned();
    let mut signing = Signing::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--platform" => platform = iter.next().ok_or("--platform needs a value")?.clone(),
            "--debug" => profile = Profile::Debug,
            "--source-only" => source_only = true,
            "--nodeps" => nodeps = true,
            "--target" => target = Some(iter.next().ok_or("--target needs a value")?.clone()),
            "--container" => container = true,
            "--fresh" => fresh = true,
            "--arch" => arch = iter.next().ok_or("--arch needs a value")?.clone(),
            "--sign" => {
                signing.identity = Some(iter.next().ok_or("--sign needs an identity")?.clone());
            }
            "--notarize" => {
                signing.notary = Some(
                    iter.next()
                        .ok_or("--notarize needs a notarytool keychain profile")?
                        .clone(),
                );
            }
            other => return Err(format!("unknown package argument `{other}`")),
        }
    }
    if signing.notary.is_some() && signing.identity.is_none() {
        return Err(
            "--notarize needs --sign: only a Developer ID signed build can be notarized".into(),
        );
    }
    if signing.identity.is_some()
        && (fresh || container || !matches!(platform.as_str(), "macos" | "current"))
    {
        return Err("--sign and --notarize apply to the macOS packages".into());
    }
    if fresh {
        if platform == "macos" {
            return crate::fresh::mac_check(&repo_root()).map(drop);
        }
        return crate::fresh::fresh_check(&arch);
    }
    if container {
        if platform != "linux" {
            return Err("--container builds the Linux archives: use --platform linux".into());
        }
        return crate::fresh::build_in_container(&arch);
    }
    match platform.as_str() {
        "arch" => return arch_package(&repo_root(), source_only, nodeps),
        "linux" => return linux_tarball(&repo_root()).map(drop),
        "deb" => return deb_package(&repo_root()).map(drop),
        "rpm" => return rpm_package(&repo_root()).map(drop),
        "server" => return server_archive(&repo_root(), target).map(drop),
        "macos" => {}
        "current" if cfg!(target_os = "macos") => {}
        "current" => {
            return Err("on Linux, choose a package: --platform linux, deb, rpm, or arch".into());
        }
        other => {
            return Err(format!(
                "unknown platform `{other}` (macos, arch, linux, deb, rpm, server)"
            ));
        }
    }
    if !cfg!(target_os = "macos") {
        return Err("the macOS bundle is built on a Mac".into());
    }
    let root = repo_root();
    let out = root.join("target").join("package");
    let web = crate::deps::ensure_web(&crate::manifest::Manifest::load()?)?;
    let made = macos_packages(&root, profile, &out, &web, &signing)?;
    println!("package: {}", made.app.display());
    println!("package: {} ({})", made.zip.display(), size(&made.zip));
    println!("package: {}", made.component.display());
    println!(
        "package: {} ({})",
        made.component_zip.display(),
        size(&made.component_zip)
    );
    Ok(())
}

/// What [`macos_packages`] made.
#[derive(Debug)]
pub struct MacPackages {
    /// The base app, without Chromium.
    pub app: PathBuf,
    /// Its zip.
    pub zip: PathBuf,
    /// The Dreamer Web component's directory.
    pub component: PathBuf,
    /// Its zip.
    pub component_zip: PathBuf,
}

/// How the macOS packages are signed. The default is ad hoc, which needs
/// no credentials (the gate's builds); a release passes a Developer ID
/// identity and, to notarize, a `notarytool` keychain profile.
#[derive(Debug, Clone, Default)]
pub struct Signing {
    /// A Developer ID Application identity, as `codesign --sign` takes it
    /// (its name or SHA-1).
    pub identity: Option<String>,
    /// A keychain profile made with `xcrun notarytool store-credentials`.
    pub notary: Option<String>,
}

/// The macOS release artifacts in `out`: the base app (built with web
/// panes, Chromium not in it) and its zip, and the Dreamer Web component
/// and its zip, each signed as `signing` says and verified.
pub fn macos_packages(
    root: &Path,
    profile: Profile,
    out: &Path,
    web: &crate::deps::WebDeps,
    signing: &Signing,
) -> Result<MacPackages, String> {
    let app = build_bundle_with(root, profile, out, Some(web))?;
    if let Some(id) = &signing.identity {
        developer_id_sign_app(&app, id)?;
    }
    verify_bundle(&app)?;
    if let Some(profile) = &signing.notary {
        notarize(&app, out, profile)?;
        staple(&app)?;
        assess(&app)?;
    }
    let zip = zip_bundle(&app, out)?;
    let version = version(root)?;
    let component = crate::web::build_component(out, &bin_dir(root, profile), web, &version)?;
    if let Some(id) = &signing.identity {
        crate::web::developer_id_sign_component(&component, id, out)?;
    }
    crate::web::verify_component(&component)?;
    if let Some(profile) = &signing.notary {
        // A framework cannot hold a ticket; the helper apps can, and
        // Gatekeeper finds the framework's online.
        notarize(&component, out, profile)?;
        for app in crate::web::helper_apps(&component) {
            staple(&app)?;
            assess(&app)?;
        }
    }
    let component_zip = crate::web::zip_component(&component, out, &version)?;
    Ok(MacPackages {
        app,
        zip,
        component,
        component_zip,
    })
}

/// A file's size in MB, for messages.
fn size(path: &Path) -> String {
    let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Where cargo puts `profile`'s executables for `root`.
pub fn bin_dir(root: &Path, profile: Profile) -> PathBuf {
    crate::util::cargo_target(root).join(match profile {
        Profile::Release => "release",
        Profile::Debug => "debug",
    })
}

/// The commit `root` is at, or empty.
pub fn commit(root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The Linux icons as a hicolor theme tree under `to`, named after the
/// desktop file.
fn copy_icons(from: &Path, to: &Path) -> Result<(), String> {
    let mut files = vec![(
        from.join("dreamer.svg"),
        to.join("scalable/apps/dev.dreamer.Dreamer.svg"),
    )];
    for entry in std::fs::read_dir(from.join("linux"))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let path = entry.path();
        if let Some(size) = path.file_stem().and_then(|s| s.to_str()) {
            files.push((
                path.clone(),
                to.join(format!("{size}x{size}/apps/dev.dreamer.Dreamer.png")),
            ));
        }
    }
    for (src, dst) in files {
        std::fs::create_dir_all(dst.parent().expect("icon dir")).map_err(|e| e.to_string())?;
        std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    crate::util::sha256_file(path)
}

/// A self-contained release tarball for any Linux distribution, installed
/// by `packaging/install.sh` (see docs/packaging.md for the layout).
/// Returns the staged tree it was made from, the version, and the target.
fn linux_tarball(root: &Path) -> Result<(PathBuf, String, &'static str), String> {
    if !cfg!(target_os = "linux") {
        return Err(
            "the Linux tarball is built on Linux (CI builds it on every pull request)".into(),
        );
    }
    let version = version(root)?;
    let triple = crate::deps::host_triple()?;
    let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo
        .args([
            "build",
            "--release",
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
        .env(
            "RUSTFLAGS",
            format!(
                "{} --remap-path-prefix={}=/usr/src/dreamer",
                std::env::var("RUSTFLAGS").unwrap_or_default(),
                root.display()
            ),
        )
        .current_dir(root);
    if !cargo
        .status()
        .map_err(|e| format!("cargo build: {e}"))?
        .success()
    {
        return Err("release build failed".into());
    }
    let out = root.join("target/package/linux");
    let name = format!("dreamer-{version}-{triple}");
    let stage = out.join(&name);
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    let copy = |from: &Path, to: &Path| -> Result<(), String> {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(from, to)
            .map(drop)
            .map_err(|e| format!("{}: {e}", from.display()))
    };
    for exe in EXECUTABLES {
        copy(
            &crate::util::cargo_target(root).join("release").join(exe),
            &stage.join("bin").join(exe),
        )?;
    }
    let fonts = crate::deps::ensure_fonts(&crate::manifest::Manifest::load()?)?;
    for entry in std::fs::read_dir(&fonts)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        copy(
            &entry.path(),
            &stage.join("share/dreamer/fonts").join(entry.file_name()),
        )?;
    }
    copy(
        &root.join("packaging/linux/dev.dreamer.Dreamer.desktop"),
        &stage.join("share/applications/dev.dreamer.Dreamer.desktop"),
    )?;
    copy_icons(
        &root.join("packaging/icons"),
        &stage.join("share/icons/hicolor"),
    )?;
    copy(
        &root.join("packaging/linux/dreamerd.service"),
        &stage.join("share/systemd/user/dreamerd.service"),
    )?;
    for entry in std::fs::read_dir(root.join("docs"))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        if entry.path().extension().is_some_and(|x| x == "md") {
            copy(
                &entry.path(),
                &stage.join("share/doc/dreamer").join(entry.file_name()),
            )?;
        }
    }
    for f in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"] {
        copy(&root.join(f), &stage.join(f))?;
    }
    copy(
        &root.join("packaging/install.sh"),
        &stage.join("install.sh"),
    )?;
    std::fs::write(stage.join("VERSION"), format!("{version}\n")).map_err(|e| e.to_string())?;
    // Release assets carry no version in the name, so
    // releases/latest/download/dreamer-<target>.tar.gz always resolves.
    let tarball = out.join(format!("dreamer-{triple}.tar.gz"));
    run_checked(
        Command::new("tar")
            .arg("-C")
            .arg(&out)
            .arg("-czf")
            .arg(&tarball)
            .arg(&name),
        "tar",
    )?;
    let sum = crate::util::sha256_file(&tarball)?;
    let file = tarball
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::fs::write(
        out.join(format!("{file}.sha256")),
        format!("{sum}  {file}\n"),
    )
    .map_err(|e| e.to_string())?;
    println!("package: {}", tarball.display());
    println!(
        "package: install with `sh packaging/install.sh --from {}`",
        tarball.display()
    );
    Ok((stage, version, triple))
}

/// The files a system package installs, under `dest/usr`, from the
/// tarball's staged tree: the executables, Dreamer's own copy of its font
/// (found beside the executables, as in the tarball), the launcher entry
/// and icons, the systemd user unit, the documents, and the licenses.
fn usr_tree(stage: &Path, dest: &Path) -> Result<(), String> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| e.to_string())?;
    }
    let usr = dest.join("usr");
    let copy_tree = |from: &Path, to: &Path| -> Result<(), String> {
        std::fs::create_dir_all(to.parent().unwrap_or(to)).map_err(|e| e.to_string())?;
        run_checked(Command::new("cp").arg("-R").arg(from).arg(to), "cp").map(drop)
    };
    copy_tree(&stage.join("bin"), &usr.join("bin"))?;
    for dir in ["dreamer", "applications", "icons", "doc"] {
        copy_tree(&stage.join("share").join(dir), &usr.join("share").join(dir))?;
    }
    copy_tree(
        &stage.join("share/systemd/user"),
        &usr.join("lib/systemd/user"),
    )?;
    let licenses = usr.join("share/licenses/dreamer");
    std::fs::create_dir_all(&licenses).map_err(|e| e.to_string())?;
    for f in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"] {
        std::fs::copy(stage.join(f), licenses.join(f)).map_err(|e| format!("{f}: {e}"))?;
    }
    Ok(())
}

const PACKAGE_SUMMARY: &str = "Native terminal on tmux with live pane mirrors";
const PACKAGE_DESCRIPTION: &str = "Dreamer draws every tmux pane as its own surface. Its server keeps \
working when the window closes, and panes can host scripted apps and agents that watch and \
query other terminals.";

/// Write `file`'s SHA-256 beside it, as `<file>.sha256`.
fn write_sha256(file: &Path) -> Result<(), String> {
    let sum = crate::util::sha256_file(file)?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::fs::write(
        file.with_file_name(format!("{name}.sha256")),
        format!("{sum}  {name}\n"),
    )
    .map_err(|e| e.to_string())
}

/// `.deb` for Ubuntu 24.04 and later and Debian 13, from the tarball's
/// build (glibc 2.39): `target/package/linux/dreamer-<version>-<target>.deb`.
fn deb_package(root: &Path) -> Result<PathBuf, String> {
    let (stage, version, triple) = linux_tarball(root)?;
    let arch = match triple {
        "x86_64-unknown-linux-gnu" => "amd64",
        "aarch64-unknown-linux-gnu" => "arm64",
        other => return Err(format!("no Debian architecture for {other}")),
    };
    let out = root.join("target/package/linux");
    let tree = out.join("deb-root");
    usr_tree(&stage, &tree)?;
    // Debian keeps the license with the documents, as `copyright`.
    std::fs::copy(
        stage.join("LICENSE"),
        tree.join("usr/share/doc/dreamer/copyright"),
    )
    .map_err(|e| e.to_string())?;
    let kib = run_checked(Command::new("du").args(["-sk"]).arg(tree.join("usr")), "du")?
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .to_owned();
    std::fs::create_dir_all(tree.join("DEBIAN")).map_err(|e| e.to_string())?;
    std::fs::write(
        tree.join("DEBIAN/control"),
        format!(
            "Package: dreamer\nVersion: {version}\nArchitecture: {arch}\n\
             Maintainer: Dreamer developers <noreply@github.com>\nInstalled-Size: {kib}\n\
             Depends: tmux, libc6 (>= 2.39), libgcc-s1, libstdc++6, libxcb1, libxkbcommon0, \
             libxkbcommon-x11-0, libwayland-client0, libvulkan1, libfontconfig1\n\
             Recommends: mesa-vulkan-drivers\nSection: x11\nPriority: optional\n\
             Homepage: https://github.com/probablyrobot/dreamer\n\
             Description: {PACKAGE_SUMMARY}\n {PACKAGE_DESCRIPTION}\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    let deb = out.join(format!("dreamer-{version}-{triple}.deb"));
    run_checked(
        Command::new("dpkg-deb")
            .args(["--root-owner-group", "-Zxz", "--build"])
            .arg(&tree)
            .arg(&deb),
        "dpkg-deb",
    )?;
    std::fs::remove_dir_all(&tree).map_err(|e| e.to_string())?;
    write_sha256(&deb)?;
    println!("package: {}", deb.display());
    Ok(deb)
}

/// `.rpm` for Fedora 40 and later and RHEL 10 (Alma, Rocky), from the
/// tarball's build (glibc 2.39):
/// `target/package/linux/dreamer-<version>-<target>.rpm`. Libraries the
/// executables link are found by rpm; those loaded at run time (Wayland,
/// Vulkan, fontconfig) and tmux are named.
fn rpm_package(root: &Path) -> Result<PathBuf, String> {
    let (stage, version, triple) = linux_tarball(root)?;
    let arch = match triple {
        "x86_64-unknown-linux-gnu" => "x86_64",
        "aarch64-unknown-linux-gnu" => "aarch64",
        other => return Err(format!("no RPM architecture for {other}")),
    };
    let out = root.join("target/package/linux");
    let top = out.join("rpmbuild");
    if top.exists() {
        std::fs::remove_dir_all(&top).map_err(|e| e.to_string())?;
    }
    let payload = top.join("payload");
    usr_tree(&stage, &payload)?;
    let spec = top.join("dreamer.spec");
    std::fs::write(
        &spec,
        format!(
            "Name: dreamer\nVersion: {version}\nRelease: 1\nSummary: {PACKAGE_SUMMARY}\n\
             License: Apache-2.0\nURL: https://github.com/probablyrobot/dreamer\n\
             Requires: tmux, libwayland-client, vulkan-loader, fontconfig, libxkbcommon-x11\n\
             Recommends: mesa-vulkan-drivers\n\
             # Built and stripped by cargo already.\n\
             %global debug_package %{{nil}}\n%global __os_install_post %{{nil}}\n\
             %define _build_id_links none\n%define _binary_payload w19.zstdio\n\n\
             %description\n{PACKAGE_DESCRIPTION}\n\n\
             %install\ncp -a {payload}/. %{{buildroot}}/\n\n\
             %files\n/usr/bin/dreamer\n/usr/bin/dreamerd\n/usr/bin/dream\n/usr/bin/dreamer-termapp\n\
             /usr/share/dreamer\n/usr/share/applications/dev.dreamer.Dreamer.desktop\n\
             /usr/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.*\n\
             /usr/lib/systemd/user/dreamerd.service\n%doc /usr/share/doc/dreamer\n\
             %license /usr/share/licenses/dreamer\n",
            payload = payload.display()
        ),
    )
    .map_err(|e| e.to_string())?;
    run_checked(
        Command::new("rpmbuild")
            .arg("--define")
            .arg(format!("_topdir {}", top.display()))
            .args(["--target", arch, "-bb"])
            .arg(&spec),
        "rpmbuild",
    )?;
    let built = top
        .join("RPMS")
        .join(arch)
        .join(format!("dreamer-{version}-1.{arch}.rpm"));
    let rpm = out.join(format!("dreamer-{version}-{triple}.rpm"));
    std::fs::rename(&built, &rpm).map_err(|e| format!("{}: {e}", built.display()))?;
    std::fs::remove_dir_all(&top).map_err(|e| e.to_string())?;
    write_sha256(&rpm)?;
    println!("package: {}", rpm.display());
    Ok(rpm)
}

/// Executables of the headless server archive.
pub const SERVER_EXECUTABLES: [&str; 3] = ["dreamerd", "dream", "dreamer-termapp"];

/// The headless server archive for `target` (default: this host), checked
/// after packing. Returns the archive's path.
pub fn server_archive(root: &Path, target: Option<String>) -> Result<PathBuf, String> {
    let version = version(root)?;
    let host = crate::deps::host_triple()?;
    let triple = target.unwrap_or_else(|| host.to_owned());
    let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo
        .args(["build", "--release", "--locked", "--target", &triple])
        .args([
            "-p",
            "dreamer-server",
            "-p",
            "dreamer-cli",
            "-p",
            "dreamer-termapp",
        ])
        .env(
            "RUSTFLAGS",
            format!(
                "{} --remap-path-prefix={}=/usr/src/dreamer",
                std::env::var("RUSTFLAGS").unwrap_or_default(),
                root.display()
            ),
        )
        .current_dir(root);
    if !cargo
        .status()
        .map_err(|e| format!("cargo build: {e}"))?
        .success()
    {
        return Err(format!("release build for {triple} failed"));
    }
    let built = crate::util::cargo_target(root)
        .join(&triple)
        .join("release");
    let out = root.join("target/package/server");
    let name = format!("dreamer-server-{version}-{triple}");
    let stage = out.join(&name);
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    let copy = |from: &Path, to: &Path| -> Result<(), String> {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(from, to)
            .map(drop)
            .map_err(|e| format!("{}: {e}", from.display()))
    };
    for exe in SERVER_EXECUTABLES {
        copy(&built.join(exe), &stage.join("bin").join(exe))?;
    }
    if triple.contains("linux") {
        copy(
            &root.join("packaging/linux/dreamerd.service"),
            &stage.join("share/systemd/user/dreamerd.service"),
        )?;
    }
    for doc in ["remote.md", "daemon.md", "cli.md"] {
        copy(
            &root.join("docs").join(doc),
            &stage.join("share/doc/dreamer").join(doc),
        )?;
    }
    for f in ["LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"] {
        copy(&root.join(f), &stage.join(f))?;
    }
    let commit = run_checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"]),
        "git",
    )
    .unwrap_or_default();
    std::fs::write(
        stage.join("VERSION"),
        format!(
            "version {version}\ncommit {}\ntarget {triple}\n",
            commit.trim()
        ),
    )
    .map_err(|e| e.to_string())?;
    let tarball = out.join(format!("{name}.tar.gz"));
    run_checked(
        Command::new("tar")
            .arg("-C")
            .arg(&out)
            .arg("-czf")
            .arg(&tarball)
            .arg(&name),
        "tar",
    )?;
    let sum = crate::util::sha256_file(&tarball)?;
    let file = format!("{name}.tar.gz");
    std::fs::write(
        out.join(format!("{file}.sha256")),
        format!("{sum}  {file}\n"),
    )
    .map_err(|e| e.to_string())?;
    check_server_archive(&tarball, &name, &triple, host)?;
    println!("package: {}", tarball.display());
    println!("package: sha256 {sum}");
    Ok(tarball)
}

/// Unpack the archive away from the tree; run each executable when this
/// host (or the target's configured runner) can; and check that nothing
/// graphical is linked.
fn check_server_archive(
    tarball: &Path,
    name: &str,
    triple: &str,
    host: &str,
) -> Result<(), String> {
    let away = std::env::temp_dir().join(format!("dreamer-server-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&away);
    std::fs::create_dir_all(&away).map_err(|e| e.to_string())?;
    let result = (|| {
        run_checked(
            Command::new("tar")
                .arg("-C")
                .arg(&away)
                .arg("-xzf")
                .arg(tarball),
            "tar",
        )?;
        let bin = away.join(name).join("bin");
        let runner_var = format!(
            "CARGO_TARGET_{}_RUNNER",
            triple.to_uppercase().replace('-', "_")
        );
        let runner = std::env::var(&runner_var).ok();
        for exe in SERVER_EXECUTABLES {
            let path = bin.join(exe);
            let mut cmd = if triple == host {
                Command::new(&path)
            } else if let Some(r) = &runner {
                let mut words = r.split_whitespace();
                let mut c = Command::new(words.next().unwrap_or_default());
                c.args(words).arg(&path);
                c
            } else {
                println!("package: {exe}: not run (a {triple} binary on {host}, no {runner_var})");
                continue;
            };
            let v = run_checked(cmd.arg("--version"), exe)?;
            println!("package: {}", v.trim());
            let linked = linked_libraries(&path, triple)?;
            if let Some(bad) = linked.iter().find(|l| graphical(l)) {
                return Err(format!(
                    "{exe} links {bad}, which a headless server must not"
                ));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&away);
    result
}

/// Shared libraries an executable needs, from its headers.
fn linked_libraries(path: &Path, triple: &str) -> Result<Vec<String>, String> {
    if triple.contains("apple") {
        let out = run_checked(Command::new("otool").arg("-L").arg(path), "otool")?;
        Ok(out
            .lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect())
    } else {
        let out = run_checked(Command::new("readelf").arg("-d").arg(path), "readelf")?;
        Ok(out
            .lines()
            .filter(|l| l.contains("(NEEDED)"))
            .filter_map(|l| l.split('[').nth(1)?.split(']').next().map(str::to_owned))
            .collect())
    }
}

/// A library only a graphical program would need.
fn graphical(lib: &str) -> bool {
    const MARKS: [&str; 16] = [
        "X11",
        "libX",
        "xcb",
        "wayland",
        "vulkan",
        "GL",
        "EGL",
        "fontconfig",
        "freetype",
        "harfbuzz",
        "AppKit",
        "Metal",
        "CoreText",
        "CoreGraphics",
        "QuartzCore",
        "libxkb",
    ];
    MARKS.iter().any(|m| lib.contains(m))
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

pub fn version(root: &Path) -> Result<String, String> {
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
    build_bundle_with(root, profile, out, None)
}

/// [`build_bundle`], with `web`: the desktop built with web panes, and the
/// helper executable built for the Dreamer Web component. Chromium itself
/// is not put in the bundle: it is the component ([`crate::web`]).
pub fn build_bundle_with(
    root: &Path,
    profile: Profile,
    out: &Path,
    web: Option<&crate::deps::WebDeps>,
) -> Result<PathBuf, String> {
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
            "--package",
            "dreamer-termapp",
        ])
        .current_dir(root);
    if profile == Profile::Release {
        cargo.arg("--release");
    }
    if let Some(web) = web {
        cargo
            .args(["--package", "web-engine", "--features", "dreamer-app/web"])
            .envs(web.env());
    }
    let status = cargo.status().map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build of dreamer, dreamerd, dream, and dreamer-termapp failed".into());
    }
    let bin_dir = bin_dir(root, profile);

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
    std::fs::copy(
        root.join("packaging/icons/macos/Dreamer.icns"),
        resources.join("Dreamer.icns"),
    )
    .map_err(|e| format!("copy icon: {e}"))?;
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
  <key>CFBundleIconFile</key><string>Dreamer</string>
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
    // stable code identity. A release re-signs it with Developer ID
    // ([`Signing`]).
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

/// Sign `path` with Developer ID `identity` under the hardened runtime,
/// with a secure timestamp, and `entitlements` (a plist) when given.
pub fn developer_id_sign(
    path: &Path,
    identity: &str,
    entitlements: Option<&Path>,
) -> Result<(), String> {
    let mut cmd = Command::new("codesign");
    cmd.args([
        "--force",
        "--options",
        "runtime",
        "--timestamp",
        "--sign",
        identity,
    ]);
    if let Some(e) = entitlements {
        cmd.arg("--entitlements").arg(e);
    }
    run_checked(cmd.arg(path), "codesign").map(drop)
}

/// Re-sign the ad hoc bundle with Developer ID, helpers before the bundle
/// that seals them, and check that the identity is a Developer ID
/// Application one: no other kind can be notarized for distribution.
fn developer_id_sign_app(app: &Path, identity: &str) -> Result<(), String> {
    let macos = app.join("Contents").join("MacOS");
    for exe in &EXECUTABLES[1..] {
        developer_id_sign(&macos.join(exe), identity, None)?;
    }
    developer_id_sign(app, identity, None)?;
    let (kind, team, _) = signing_state(app);
    if !kind.starts_with("Developer ID Application:") {
        return Err(format!(
            "`{identity}` signed the bundle as `{kind}`; a release needs a Developer ID Application identity (`security find-identity -v -p codesigning` lists yours)"
        ));
    }
    println!(
        "package: {} signed by {kind} (team {team}), hardened runtime",
        app.display()
    );
    Ok(())
}

/// Submit `path` (an app or a directory of them), zipped, to Apple's
/// notary service with keychain `profile`, and wait for the verdict. On
/// anything but Accepted, the service's log says why.
pub fn notarize(path: &Path, out: &Path, profile: &str) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let upload = out.join(format!("{name}.notarize.zip"));
    let _ = std::fs::remove_file(&upload);
    run_checked(
        Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(path)
            .arg(&upload),
        "ditto",
    )?;
    println!("package: notarizing {name} (this usually takes a few minutes)");
    let submitted = Command::new("xcrun")
        .args(["notarytool", "submit"])
        .arg(&upload)
        .args([
            "--keychain-profile",
            profile,
            "--wait",
            "--output-format",
            "json",
        ])
        .output()
        .map_err(|e| format!("xcrun notarytool: {e}"));
    let _ = std::fs::remove_file(&upload);
    let submitted = submitted?;
    let reply: serde_json::Value =
        serde_json::from_slice(&submitted.stdout).unwrap_or(serde_json::Value::Null);
    let status = reply["status"].as_str().unwrap_or_default();
    if status == "Accepted" {
        println!(
            "package: {name} notarized (submission {})",
            reply["id"].as_str().unwrap_or("?")
        );
        return Ok(());
    }
    let mut why = format!(
        "notarization of {name}: {}",
        if status.is_empty() {
            String::from_utf8_lossy(&submitted.stderr).trim().to_owned()
        } else {
            status.to_owned()
        }
    );
    if let Some(id) = reply["id"].as_str()
        && let Ok(log) = Command::new("xcrun")
            .args(["notarytool", "log", id, "--keychain-profile", profile])
            .output()
    {
        why.push_str(&format!(
            "\n{}",
            String::from_utf8_lossy(&log.stdout).trim()
        ));
    }
    Err(why)
}

/// Attach the notarization ticket to `app`, so it opens offline.
fn staple(app: &Path) -> Result<(), String> {
    run_checked(
        Command::new("xcrun").args(["stapler", "staple"]).arg(app),
        "xcrun stapler staple",
    )
    .map(drop)
}

/// Gatekeeper accepts `app` as a notarized Developer ID app.
fn assess(app: &Path) -> Result<(), String> {
    let out = Command::new("spctl")
        .args(["--assess", "--type", "execute", "-vv"])
        .arg(app)
        .output()
        .map_err(|e| format!("spctl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() && text.contains("source=Notarized Developer ID") {
        Ok(())
    } else {
        Err(format!(
            "Gatekeeper does not accept {}: {}",
            app.display(),
            text.trim()
        ))
    }
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
    // Chromium is the Dreamer Web component, never part of the app.
    let frameworks = app.join("Contents/Frameworks");
    if frameworks.join(web_component::FRAMEWORK).exists()
        || web_component::HELPERS
            .iter()
            .any(|(suffix, _)| frameworks.join(web_component::helper_app(suffix)).exists())
    {
        return Err(format!(
            "{}: Chromium is in the bundle; it belongs in the {} component",
            app.display(),
            web_component::NAME
        ));
    }
    if !app.join("Contents/Resources/Dreamer.icns").is_file() {
        return Err("the bundle has no Dreamer.icns".into());
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
    let termapp = run_checked(
        Command::new(macos.join("dreamer-termapp")).arg("--version"),
        "dreamer-termapp",
    )?;
    let (kind, _, _) = signing_state(app);
    println!(
        "package: dreamer, dreamerd, dream, dreamer-termapp: arm64, signed {kind}, minimum macOS {MIN_MACOS}; {}; {}; {}",
        daemon.trim(),
        cli.trim(),
        termapp.trim()
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
    record_bundle(app, &zip, hash)?;
    Ok(zip)
}

/// How the bundle is signed, from `codesign`: the kind (`adhoc`, or the
/// first signing authority), the team, and whether a notarization ticket
/// is stapled.
pub fn signing_state(app: &Path) -> (String, String, bool) {
    let out = Command::new("codesign")
        .args(["-d", "--verbose=2"])
        .arg(app)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    let field = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k).map(str::trim))
            .map(str::to_owned)
    };
    let kind = match field("Signature=") {
        Some(s) if s == "adhoc" => "adhoc".to_owned(),
        _ => field("Authority=").unwrap_or_else(|| "unsigned".to_owned()),
    };
    let team = field("TeamIdentifier=").unwrap_or_else(|| "not set".to_owned());
    let stapled = Command::new("xcrun")
        .args(["stapler", "validate", "-q"])
        .arg(app)
        .status()
        .is_ok_and(|s| s.success());
    (kind, team, stapled)
}

/// `<zip>.json`: what the zip is, the commit it was built from, and its
/// signing state, for the release's build report (spec 17.6).
fn record_bundle(app: &Path, zip: &Path, sha256: &str) -> Result<(), String> {
    use crate::gate::json_escape as esc;
    let root = repo_root();
    let commit = commit(&root);
    let bytes = std::fs::metadata(zip).map_or(0, |m| m.len());
    let (kind, team, stapled) = signing_state(app);
    let exes: Vec<String> = EXECUTABLES.iter().map(|e| format!("\"{e}\"")).collect();
    let json = format!(
        "{{\n  \"artifact\": \"{}\",\n  \"bytes\": {bytes},\n  \"sha256\": \"{}\",\n  \
         \"target\": \"aarch64-apple-darwin\",\n  \"version\": \"{}\",\n  \"commit\": \"{}\",\n  \
         \"minimum_macos\": \"{MIN_MACOS}\",\n  \"executables\": [{}],\n  \
         \"signing\": {{ \"kind\": \"{}\", \"team\": \"{}\", \"notarized\": {stapled} }}\n}}\n",
        esc(&zip
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()),
        esc(sha256),
        esc(&version(&root)?),
        esc(&commit),
        exes.join(", "),
        esc(&kind),
        esc(&team),
    );
    std::fs::write(zip.with_extension("zip.json"), json).map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::graphical;

    #[test]
    fn graphical_libraries_are_recognized_and_system_ones_are_not() {
        for lib in [
            "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit",
            "/System/Library/Frameworks/Metal.framework/Versions/A/Metal",
            "libxkbcommon.so.0",
            "libwayland-client.so.0",
            "libfontconfig.so.1",
            "libX11-xcb.so.1",
        ] {
            assert!(graphical(lib), "{lib}");
        }
        for lib in [
            "/usr/lib/libSystem.B.dylib",
            "/usr/lib/libiconv.2.dylib",
            "libc.so.6",
            "libm.so.6",
            "libgcc_s.so.1",
            "ld-linux-aarch64.so.1",
        ] {
            assert!(!graphical(lib), "{lib}");
        }
    }
}
