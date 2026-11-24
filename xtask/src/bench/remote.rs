//! `cargo xtask bench remote --container [--netem SPEC] [OPTIONS]`: the
//! remote workload with this machine as the desktop side and the host's
//! daemon in the gate's ARM64 Linux container, reached through real
//! OpenSSH.
//!
//! - **The host.** The container builds `dreamerd` and `dream` from this
//!   checkout and the pinned tmux, starts a tmux server on a private
//!   socket, and runs `sshd` (as root, inside the container only) on its
//!   port 2222, published on this machine's 127.0.0.1 alone. The sshd has
//!   a generated host key, accepts only a generated client key, and gives
//!   its sessions private daemon directories (`SetEnv`).
//! - **This machine.** `ssh` runs through a wrapper with its own config
//!   file, `known_hosts`, and key, and no agent; nothing of the user's SSH
//!   setup is read or changed. The release `dreamerd` and `dreamer-bench`
//!   built here run the workload against it.
//! - **`--netem SPEC`** adds `tc qdisc ... netem SPEC` (for example
//!   `delay 20ms 5ms`) to the container's `eth0`. It delays what the
//!   container sends, so each round trip gains the delay once.
//!
//! The container is removed afterwards, and the private directory here
//! with it. Only on an ARM64 host, as the gate's container stage.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The host as the private SSH config names it.
const DESTINATION: &str = "dreamer-bench-host";
/// The host's tmux socket name.
const TMUX_SOCKET: &str = "dreamer-bench";
/// Where the host's files live in the container.
const FAR: &str = "/root/drm-bench";

/// Options the container route sets itself.
const OWN: &[&str] = &[
    "--ssh",
    "--destination",
    "--dream",
    "--host-tmux",
    "--host-tmux-socket",
    "--link",
    "--json",
];

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A `tc netem` specification: words of letters, digits, `.`, `%`, and
/// `-` only, since it goes into the container's shell script as words.
fn netem_words(spec: &str) -> Result<String, String> {
    let ok = !spec.trim().is_empty()
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " .%-".contains(c));
    if ok {
        Ok(spec.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        Err(format!(
            "--netem {spec:?}: expected tc netem words such as `delay 20ms 5ms`"
        ))
    }
}

/// The published port from `docker port`'s `127.0.0.1:PORT`.
fn published_port(out: &str) -> Option<u16> {
    out.lines()
        .find(|l| l.starts_with("127.0.0.1:"))
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.trim().parse().ok())
}

fn keygen(path: &Path) -> Result<(), String> {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .map_err(|e| format!("ssh-keygen: {e}"))?
        .success();
    if ok {
        Ok(())
    } else {
        Err("ssh-keygen failed".into())
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!("docker {} failed", args.first().unwrap_or(&"")))
    }
}

pub fn container(root: &Path, args: &[String]) -> Result<(), String> {
    let mut netem: Option<String> = None;
    let mut debug = false;
    let mut pass: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--container" => {}
            "--debug" => debug = true,
            "--netem" => netem = Some(netem_words(it.next().ok_or("--netem needs a spec")?)?),
            "--linux" => return Err("--container and --linux do not go together".into()),
            o if OWN.contains(&o) => {
                return Err(format!("{o} is set by --container"));
            }
            _ => pass.push(a.clone()),
        }
    }
    if std::env::consts::ARCH != "aarch64" {
        return Err("--container runs only on an ARM64 host, as the gate's container".into());
    }
    let docker_up = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty());
    if !docker_up {
        return Err("no container runtime is running".into());
    }
    let profile = if debug { "debug" } else { "release" };

    // This machine's side.
    let mut build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    build.args(["build", "--locked"]);
    if !debug {
        build.arg("--release");
    }
    let built = build
        .args(["-p", "dreamer-server", "-p", "bench"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !built.success() {
        return Err("building dreamerd and dreamer-bench failed".into());
    }

    let t = PathBuf::from(format!("/tmp/drm-bremote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let result = run(root, &t, profile, netem.as_deref(), &pass);
    // The ssh clients this run started name the private directory.
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(format!("{}/", t.display()))
        .status();
    let _ = std::fs::remove_dir_all(&t);
    result
}

fn run(
    root: &Path,
    t: &Path,
    profile: &str,
    netem: Option<&str>,
    pass: &[String],
) -> Result<(), String> {
    keygen(&t.join("host_ed25519"))?;
    keygen(&t.join("id_ed25519"))?;
    std::fs::copy(t.join("id_ed25519.pub"), t.join("authorized_keys"))
        .map_err(|e| e.to_string())?;
    std::fs::write(
        t.join("sshd_config"),
        format!(
            "Port 2222\nListenAddress 0.0.0.0\nHostKey {FAR}/host_ed25519\n\
             PidFile {FAR}/sshd.pid\nAuthorizedKeysFile {FAR}/authorized_keys\n\
             StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\nPermitRootLogin prohibit-password\n\
             AllowAgentForwarding no\nAllowTcpForwarding no\nX11Forwarding no\n\
             PermitTTY no\nUsePAM no\n\
             SetEnv DREAMER_RUNTIME_DIR={FAR}/far/rt DREAMER_STATE_DIR={FAR}/far/st \
             DREAMER_CONFIG_DIR={FAR}/far/config DREAMER_FORBID_DEFAULT_TMUX=1\n"
        ),
    )
    .map_err(|e| e.to_string())?;

    // The host's side: the gate's image, cargo caches, and the checkout,
    // as `bench --linux` mounts them.
    let image = crate::gate::ensure_arm64_image()?;
    let mut run = Command::new("docker");
    run.args([
        "run",
        "-d",
        "--rm",
        "--platform",
        "linux/arm64",
        "--cap-add",
        "NET_ADMIN",
        "-p",
        "127.0.0.1::2222",
    ])
    .arg("-v")
    .arg(format!("{}:/src", root.display()))
    .args([
        "-v",
        "dreamer-arm64-cargo:/root/.cargo",
        "-v",
        "dreamer-arm64-rustup:/root/.rustup",
        "-v",
        "dreamer-arm64-cache:/root/.cache",
        "-e",
        "CARGO_TARGET_DIR=/root/.cache/dreamer-bench-target",
        "-w",
        "/src",
        &image,
        "sleep",
        "infinity",
    ]);
    let out = run.output().map_err(|e| format!("docker run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let container = Container(String::from_utf8_lossy(&out.stdout).trim().to_owned());
    let id = container.0.as_str();
    docker(&["exec", id, "mkdir", "-p", FAR])?;
    for f in ["host_ed25519", "authorized_keys", "sshd_config"] {
        docker(&[
            "cp",
            &t.join(f).display().to_string(),
            &format!("{id}:{FAR}/{f}"),
        ])?;
    }
    let release = if profile == "release" {
        "--release"
    } else {
        ""
    };
    let netem_script = match netem {
        Some(spec) => format!(
            "apt-get update -qq >&2 && apt-get install -y -qq --no-install-recommends iproute2 >/dev/null; \
             tc qdisc add dev eth0 root netem {spec}; tc qdisc show dev eth0 >&2; "
        ),
        None => String::new(),
    };
    // Progress on standard error; the two paths on standard output.
    let script = format!(
        "set -e; ulimit -n 65536; export USER=root DEBIAN_FRONTEND=noninteractive; \
         command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none >&2; \
         . \"$HOME/.cargo/env\"; \
         cargo xtask deps prepare --locked >&2; \
         export DREAMER_TMUX=$(cargo xtask deps tmux | tail -n 1); \
         cargo build --locked {release} -p dreamer-server -p dreamer-cli >&2; \
         chown root: {FAR}/*; chmod 600 {FAR}/*; mkdir -p /run/sshd; \
         /usr/sbin/sshd -f {FAR}/sshd_config -E {FAR}/sshd.log; \
         \"$DREAMER_TMUX\" -L {TMUX_SOCKET} -f /dev/null new-session -d -s main -x 120 -y 40; \
         {netem_script}\
         echo \"$CARGO_TARGET_DIR/{profile}/dream\"; echo \"$DREAMER_TMUX\"; uname -srm"
    );
    println!(
        "bench: preparing the host in container {}",
        &id[..id.len().min(12)]
    );
    let host = docker(&["exec", id, "bash", "-c", &script])?;
    let lines: Vec<&str> = host.lines().collect();
    let [.., dream, tmux, uname] = lines.as_slice() else {
        return Err(format!("the host did not report its paths: {host:?}"));
    };
    let port = published_port(&docker(&["port", id, "2222/tcp"])?)
        .ok_or("docker published no loopback port for sshd")?;

    // This machine's private ssh.
    let host_key =
        std::fs::read_to_string(t.join("host_ed25519.pub")).map_err(|e| e.to_string())?;
    std::fs::write(
        t.join("known_hosts"),
        format!("[127.0.0.1]:{port} {}\n", host_key.trim()),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        t.join("ssh_config"),
        format!(
            "Host {DESTINATION}\n  HostName 127.0.0.1\n  Port {port}\n  User root\n  \
             IdentityFile {t}/id_ed25519\n  IdentitiesOnly yes\n  IdentityAgent none\n  \
             UserKnownHostsFile {t}/known_hosts\n  GlobalKnownHostsFile /dev/null\n  \
             StrictHostKeyChecking yes\n  PasswordAuthentication no\n",
            t = t.display()
        ),
    )
    .map_err(|e| e.to_string())?;
    let wrapper = t.join("ssh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec ssh -F '{}' \"$@\"\n",
            t.join("ssh_config").display()
        ),
    )
    .map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    // sshd is up once a plain command runs.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let probe = Command::new(&wrapper)
            .args(["-T", "-o", "BatchMode=yes", DESTINATION, "true"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if probe.status.success() {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "ssh to the container failed: {}",
                String::from_utf8_lossy(&probe.stderr).trim()
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let openssh = Command::new("ssh")
        .arg("-V")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned())
        .unwrap_or_default();
    let link = format!(
        "{openssh} to an ARM64 Linux container ({uname}) on Docker's published loopback port, netem {}",
        netem.unwrap_or("none")
    );

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let out = root.join(format!("target/bench/remote-{stamp}.json"));
    let bin = crate::util::cargo_target(root).join(profile);
    let status = Command::new(bin.join("dreamer-bench"))
        .arg("remote")
        .arg("--ssh")
        .arg(&wrapper)
        .args([
            "--destination",
            DESTINATION,
            "--dream",
            dream,
            "--host-tmux",
            tmux,
            "--host-tmux-socket",
            TMUX_SOCKET,
            "--link",
            &link,
        ])
        .args(pass)
        .arg("--json")
        .arg(&out)
        .env(
            "DREAMER_BENCH_REVISION",
            super::revision(root, profile).to_string(),
        )
        .status()
        .map_err(|e| format!("dreamer-bench: {e}"))?;
    drop(container);
    if !status.success() {
        return Err("the remote workload failed".into());
    }
    println!("results: {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netem_takes_tc_words_only() {
        assert_eq!(
            netem_words(" delay  20ms 5ms ").as_deref(),
            Ok("delay 20ms 5ms")
        );
        assert_eq!(
            netem_words("delay 10ms loss 0.5%").as_deref(),
            Ok("delay 10ms loss 0.5%")
        );
        assert!(netem_words("").is_err());
        assert!(netem_words("delay 20ms; reboot").is_err());
        assert!(netem_words("delay $(x)").is_err());
    }

    #[test]
    fn the_published_port_is_the_loopback_one() {
        assert_eq!(published_port("127.0.0.1:55021\n"), Some(55021));
        assert_eq!(published_port("0.0.0.0:1\n"), None);
    }
}
