//! Linux archives built in, and checked on, fresh containers (spec 17.5:
//! "test launching the unpacked archive on a clean compatible machine").
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

use std::process::Command;

use crate::util::repo_root;

/// Runtime packages the desktop needs on Ubuntu 24.04: linked (xcb,
/// xkbcommon) and loaded at run time (Wayland, Vulkan, fontconfig). Keep
/// in step with `docs/packaging.md`.
pub const DESKTOP_RUNTIME: &str = "libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 mesa-vulkan-drivers libfontconfig1";

fn triple(arch: &str) -> Result<&'static str, String> {
    match arch {
        "arm64" => Ok("aarch64-unknown-linux-gnu"),
        "amd64" => Ok("x86_64-unknown-linux-gnu"),
        other => Err(format!("unknown --arch `{other}` (arm64, amd64)")),
    }
}

fn docker(args: &[&str], script: &str, what: &str) -> Result<(), String> {
    let status = Command::new("docker")
        .args(args)
        .args(["ubuntu:24.04", "bash", "-c", script])
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
          libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan-dev >/dev/null; \
        command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none; \
        . \"$HOME/.cargo/env\"; \
        cargo xtask deps prepare --locked; \
        cargo xtask package --platform linux; \
        cargo xtask package --platform server";
    let platform = format!("linux/{arch}");
    let src = format!("{}:/src", root.display());
    let cargo = format!("dreamer-{arch}-cargo:/root/.cargo");
    let rustup = format!("dreamer-{arch}-rustup:/root/.rustup");
    let cache = format!("dreamer-{arch}-cache:/root/.cache");
    docker(
        &[
            "run",
            "--rm",
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
        ],
        script,
        "building the Linux archives",
    )
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
    println!("fresh: both archives ran on fresh {arch} containers");
    // For the build report: which archives, by checksum, passed.
    let record = format!(
        "{{\n  \"arch\": \"{arch}\",\n  \"image\": \"ubuntu:24.04\",\n  \"unix_time\": {},\n  \
         \"checked\": [\n    {{ \"artifact\": \"{}\", \"sha256\": \"{}\", \"check\": \"bare image; dreamerd --self-check, dream and dreamer-termapp --version\" }},\n    \
         {{ \"artifact\": \"{}\", \"sha256\": \"{}\", \"check\": \"install.sh for a new user with the documented runtime packages; dreamer --smoke presented a frame on X11 (Xvfb, Mesa software Vulkan)\" }}\n  ]\n}}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        server,
        crate::util::sha256_file(&out.join("server").join(&server))?,
        desktop.trim_start_matches("linux/"),
        crate::util::sha256_file(&out.join(&desktop))?,
    );
    std::fs::write(out.join(format!("fresh-{arch}.json")), record).map_err(|e| e.to_string())
}

/// The architecture docker runs natively here.
pub fn native_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}
