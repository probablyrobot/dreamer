//! Linux archives built in, and checked on, fresh containers, to
//! test the unpacked archive on a clean, compatible machine.
//!
//! - `cargo xtask package --platform linux --container [--arch A]` builds
//!   the desktop tarball and the server archive inside an `ubuntu:24.04`
//!   container of that architecture (`arm64` by default: native on an
//!   Apple Silicon Mac), so ARM64 archives exist without an ARM64 runner.
//! - `cargo xtask package --fresh [--arch A]` checks the archives for that
//!   architecture on fresh `ubuntu:24.04` containers:
//!   - the server archive on a bare image with nothing added, every
//!     executable run;
//!   - the desktop tarball installed by `install.sh` for a new user, with
//!     only the runtime packages `docs/packaging.md` lists, then
//!     `dreamer --smoke` on X11 (Xvfb, with Mesa's software Vulkan), which
//!     must present a frame.
//! - `cargo xtask package --fresh --platform macos` adds Dreamer Web the
//!   way a user does, away from the tree ([`mac_check`]). There is no
//!   fresh Mac to run it on: it uses a new directory and an empty data
//!   directory on this one.

use std::process::Command;

use crate::util::repo_root;

/// Runtime packages the desktop needs on Ubuntu 24.04: linked (xcb,
/// xkbcommon) and loaded at run time (Wayland, Vulkan, fontconfig). Keep
/// in step with `docs/packaging.md`.
pub const DESKTOP_RUNTIME: &str = "libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 mesa-vulkan-drivers libfontconfig1";

/// The RPM families the `.rpm` is checked on: Fedora, and RHEL 10's
/// rebuild AlmaLinux.
const FEDORA: &str = "fedora:44";
const EL: &str = "almalinux:10";

fn triple(arch: &str) -> Result<&'static str, String> {
    match arch {
        "arm64" => Ok("aarch64-unknown-linux-gnu"),
        "amd64" => Ok("x86_64-unknown-linux-gnu"),
        other => Err(format!("unknown --arch `{other}` (arm64, amd64)")),
    }
}

fn docker(args: &[&str], script: &str, what: &str) -> Result<(), String> {
    docker_on("ubuntu:24.04", args, script, what)
}

fn docker_on(image: &str, args: &[&str], script: &str, what: &str) -> Result<(), String> {
    let status = Command::new("docker")
        .args(args)
        .args([image, "bash", "-c", script])
        .status()
        .map_err(|e| format!("docker run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed in the container"))
    }
}

/// Build the desktop tarball and the server archive in a container.
pub fn build_in_container(arch: &str) -> Result<(), String> {
    triple(arch)?;
    let root = repo_root();
    let script = "set -e; export DEBIAN_FRONTEND=noninteractive; \
        apt-get update -qq && apt-get install -y -qq --no-install-recommends \
          build-essential pkg-config curl ca-certificates xz-utils git libclang-dev \
          libevent-dev libncurses-dev bison python3 \
          libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev \
          libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan-dev rpm >/dev/null; \
        command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
        . \"$HOME/.cargo/env\"; \
        cargo xtask deps prepare --locked; \
        cargo xtask package --platform linux; \
        cargo xtask package --platform deb; \
        cargo xtask package --platform rpm; \
        cargo xtask package --platform server";
    let platform = format!("linux/{arch}");
    let src = format!("{}:/src", root.display());
    let cargo = format!("dreamer-{arch}-cargo:/root/.cargo");
    let rustup = format!("dreamer-{arch}-rustup:/root/.rustup");
    let cache = format!("dreamer-{arch}-cache:/root/.cache");
    let mut args: Vec<&str> = vec!["run", "--rm"];
    args.extend([
        "--platform",
        &platform,
        "-v",
        &src,
        "-v",
        &cargo,
        "-v",
        &rustup,
        "-v",
        &cache,
        "-e",
        "CARGO_TARGET_DIR=/root/.cache/dreamer-package-target",
        "-w",
        "/src",
    ]);
    docker(&args, script, "building the Linux archives")
}

/// Check this architecture's archives on fresh containers.
pub fn fresh_check(arch: &str) -> Result<(), String> {
    let triple = triple(arch)?;
    let root = repo_root();
    let out = root.join("target/package");
    let desktop = format!("linux/dreamer-{triple}.tar.gz");
    let server = std::fs::read_dir(out.join("server"))
        .map_err(|e| format!("{}: {e}", out.join("server").display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("dreamer-server-") && n.ends_with(&format!("-{triple}.tar.gz")))
        .ok_or_else(|| format!("no server archive for {triple} in target/package/server"))?;
    for f in [desktop.clone(), format!("server/{server}")] {
        if !out.join(&f).is_file() {
            return Err(format!(
                "{} is missing; build it first",
                out.join(f).display()
            ));
        }
    }
    let platform = format!("linux/{arch}");
    let pkg = format!("{}:/pkg:ro", out.display());
    let install = format!(
        "{}:/install.sh:ro",
        root.join("packaging/install.sh").display()
    );
    let mounts = [
        "run",
        "--rm",
        "--platform",
        &platform,
        "-v",
        &pkg,
        "-v",
        &install,
    ];

    println!("fresh: {server} on a bare ubuntu:24.04");
    docker(
        &mounts,
        &format!(
            "set -e; mkdir /d && tar -xzf /pkg/server/{server} -C /d; b=$(ls -d /d/*/bin); \
             $b/dreamerd --self-check; $b/dream --version; $b/dreamer-termapp --version"
        ),
        "the server archive",
    )?;

    println!("fresh: dreamer-{triple}.tar.gz installed for a new user, then a frame on X11");
    docker(
        &mounts,
        &format!(
            "set -e; export DEBIAN_FRONTEND=noninteractive; \
             apt-get update -qq && apt-get install -y -qq --no-install-recommends \
               {DESKTOP_RUNTIME} xvfb xauth >/dev/null; \
             useradd -m u; su u -c 'sh /install.sh --from /pkg/{desktop}' >/dev/null; \
             b=/home/u/.local/bin; \
             su u -c \"$b/dream --version; $b/dreamer-termapp --version; $b/dreamerd --self-check\"; \
             su u -c \"mkdir -p -m 700 /tmp/xrt-u; XDG_RUNTIME_DIR=/tmp/xrt-u timeout 120 \
               xvfb-run -a -s '-screen 0 1280x800x24' $b/dreamer --smoke\""
        ),
        "the desktop tarball",
    )?;
    // The system packages, where they were built, installed by each
    // family's own package manager on its fresh image.
    let version = crate::package::version(&root)?;
    let deb = format!("linux/dreamer-{version}-{triple}.deb");
    let rpm = format!("linux/dreamer-{version}-{triple}.rpm");
    let mut packages: Vec<(String, &str, String)> = Vec::new();
    // As an unprivileged user, after the package: the CLIs, then a frame.
    let run_as_user = "useradd -m u; \
        su u -c 'dream --version; dreamer-termapp --version; dreamerd --self-check'";
    if out.join(&deb).is_file() {
        println!("fresh: {deb} installed with apt on ubuntu:24.04, then a frame on X11");
        docker_on(
            "ubuntu:24.04",
            &mounts,
            &format!(
                "set -e; export DEBIAN_FRONTEND=noninteractive; apt-get update -qq; \
                 apt-get install -y -qq /pkg/{deb} xvfb xauth >/dev/null; {run_as_user}; \
                 su u -c \"mkdir -p -m 700 /tmp/xrt-u; XDG_RUNTIME_DIR=/tmp/xrt-u timeout 120 \
                   xvfb-run -a -s '-screen 0 1280x800x24' dreamer --smoke\""
            ),
            "the .deb",
        )?;
        packages.push((deb.clone(), "ubuntu:24.04", "apt install with its dependencies; the CLIs and dreamer --smoke presented a frame on X11 (Xvfb, Mesa software Vulkan)".into()));
    }
    if out.join(&rpm).is_file() {
        println!("fresh: {rpm} installed with dnf on {FEDORA}, then a frame on X11");
        docker_on(
            FEDORA,
            &mounts,
            &format!(
                "set -e; dnf install -y -q /pkg/{rpm} xorg-x11-server-Xvfb mesa-vulkan-drivers \
                   shadow-utils util-linux >/dev/null; {run_as_user}; \
                 su u -c \"mkdir -p -m 700 /tmp/xrt-u; Xvfb :99 -screen 0 1280x800x24 & \
                   sleep 1; DISPLAY=:99 XDG_RUNTIME_DIR=/tmp/xrt-u timeout 120 dreamer --smoke\""
            ),
            "the .rpm",
        )?;
        packages.push((rpm.clone(), FEDORA, "dnf install with its dependencies; the CLIs and dreamer --smoke presented a frame on X11 (Xvfb, Mesa software Vulkan)".into()));
        // RHEL 10 ships no X server, Xvfb included: the install and the
        // command-line tools only.
        println!("fresh: {rpm} installed with dnf on {EL} (no Xvfb there: no frame)");
        docker_on(
            EL,
            &mounts,
            &format!(
                "set -e; dnf install -y -q /pkg/{rpm} shadow-utils util-linux >/dev/null; \
                 {run_as_user}; test -x /usr/bin/dreamer"
            ),
            "the .rpm",
        )?;
        packages.push((rpm.clone(), EL, "dnf install with its dependencies; the CLIs ran (RHEL 10 has no Xvfb, so no frame was drawn)".into()));
    }
    println!("fresh: both archives ran on fresh {arch} containers");
    // For the build report: which archives, by checksum, passed.
    let mut checked = vec![
        serde_json::json!({
            "artifact": server,
            "sha256": crate::util::sha256_file(&out.join("server").join(&server))?,
            "check": "bare image; dreamerd --self-check, dream and dreamer-termapp --version",
        }),
        serde_json::json!({
            "artifact": desktop.trim_start_matches("linux/"),
            "sha256": crate::util::sha256_file(&out.join(&desktop))?,
            "check": "install.sh for a new user with the documented runtime packages; dreamer --smoke presented a frame on X11 (Xvfb, Mesa software Vulkan)",
        }),
    ];
    for (file, image, check) in &packages {
        checked.push(serde_json::json!({
            "artifact": file.trim_start_matches("linux/"),
            "sha256": crate::util::sha256_file(&out.join(file))?,
            "check": format!("{check} ({image})"),
        }));
    }
    let record = serde_json::json!({
        "arch": arch,
        "image": "ubuntu:24.04",
        "unix_time": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        "checked": checked,
    });
    std::fs::write(
        out.join(format!("fresh-{arch}.json")),
        serde_json::to_string_pretty(&record).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())
}

/// macOS: the release's Dreamer Web zip added the way a user adds it,
/// away from the tree. The base zip is unpacked into a new temporary
/// directory and, with an empty data directory, its `dream web status`
/// says Dreamer Web is missing and how to add it; `dream web install`
/// installs it from a copy of its zip (checksum, manifest, the framework's
/// and helper apps' signatures, a helper loading the framework from where
/// it sits); `dream web status` then says it is ready; and `dream web
/// remove` takes it out. Writes `target/package/fresh-macos.json` for the
/// build report. Opens no window. The lines it printed.
pub fn mac_check(root: &std::path::Path) -> Result<Vec<String>, String> {
    if !cfg!(target_os = "macos") {
        return Err("the macOS component check runs on a Mac".into());
    }
    let out = root.join("target/package");
    let version = crate::package::version(root)?;
    let base = out.join(format!("dreamer-{version}-aarch64-apple-darwin.zip"));
    let name = web_component::archive_name(&version, "aarch64-apple-darwin");
    let component = out.join(&name);
    for f in [&base, &component] {
        if !f.is_file() {
            return Err(format!(
                "{} is missing; build it first (cargo xtask package --platform macos)",
                f.display()
            ));
        }
    }
    let away = std::env::temp_dir().join(format!("dreamer-fresh-macos-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&away);
    std::fs::create_dir_all(&away).map_err(|e| e.to_string())?;
    let result = (|| {
        let unzipped = Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&base)
            .arg(&away)
            .status()
            .map_err(|e| format!("ditto: {e}"))?;
        if !unzipped.success() {
            return Err(format!("unpacking {} failed", base.display()));
        }
        let app = away.join("Dreamer.app");
        for f in [name.clone(), format!("{name}.sha256")] {
            std::fs::copy(out.join(&f), away.join(&f)).map_err(|e| format!("{f}: {e}"))?;
        }
        let data = away.join("data");
        let lines = crate::web::check_install(&app, &away.join(&name), &data)?;
        crate::web::dream(
            &app,
            &["web", "remove"],
            None,
            &[("DREAMER_DATA_DIR", data.into_os_string())],
        )?;
        Ok(lines)
    })();
    let _ = std::fs::remove_dir_all(&away);
    let lines = result?;
    for l in &lines {
        println!("fresh: {l}");
    }
    let record = serde_json::json!({
        "arch": "arm64",
        "image": "this Mac: a new directory and an empty data directory",
        "unix_time": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        "checked": [{
            "artifact": name,
            "sha256": crate::util::sha256_file(&component)?,
            "check": "installed by the unpacked app's `dream web install` into an empty data directory: checksum, manifest, codesign --verify of the framework and helper apps, a helper loaded the framework; `dream web status` ready; `dream web remove`",
        }],
    });
    std::fs::write(
        out.join("fresh-macos.json"),
        serde_json::to_string_pretty(&record).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())?;
    println!("fresh: {name} added on this Mac, away from the tree");
    Ok(lines)
}

/// The architecture docker runs natively here.
pub fn native_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}
