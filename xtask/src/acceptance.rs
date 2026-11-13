//! `cargo xtask remote-acceptance`: the section 18.2 remote cases through
//! real OpenSSH, with nothing of the user's SSH setup involved.
//!
//! An unprivileged `sshd` listens on a free loopback port with a generated
//! host key, accepts only a generated client key, and gives its sessions a
//! private HOME and private daemon directories (`SetEnv`). The coordinator's
//! `ssh` is a wrapper with its own config file and `known_hosts`. The test
//! (`crates/server/tests/remote_acceptance.rs`) checks the session's HOME
//! before installing anything.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::util::repo_root;

fn find(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
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

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn run(_args: &[String]) -> Result<(), String> {
    let root = repo_root();
    let sshd = find(&["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/local/sbin/sshd"])
        .ok_or("no sshd found (install openssh-server); remote acceptance not run")?;
    // This build's server archive, as a host would get it.
    let archive = crate::package::server_archive(&root, None)?;
    let artifacts = archive.parent().ok_or("archive dir")?.to_path_buf();
    let build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--locked",
            "-p",
            "dreamer-server",
            "-p",
            "dreamer-cli",
        ])
        .current_dir(&root)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !build.success() {
        return Err("build failed".into());
    }

    let t = PathBuf::from(format!("/tmp/drm-accept-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    // Daemons make their own owner-only directories.
    for d in ["home", "far"] {
        std::fs::create_dir_all(t.join(d)).map_err(|e| e.to_string())?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let result = (|| {
        keygen(&t.join("host_ed25519"))?;
        keygen(&t.join("id_ed25519"))?;
        std::fs::copy(t.join("id_ed25519.pub"), t.join("authorized_keys"))
            .map_err(|e| e.to_string())?;
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| e.to_string())?
            .port();
        let path_env = std::env::var("PATH").unwrap_or_default();
        std::fs::write(
            t.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {t}/host_ed25519\n\
                 PidFile {t}/sshd.pid\nAuthorizedKeysFile {t}/authorized_keys\n\
                 StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\nAllowAgentForwarding no\nAllowTcpForwarding no\n\
                 X11Forwarding no\nPermitTTY no\nUsePAM no\n\
                 SetEnv HOME={t}/home DREAMER_RUNTIME_DIR={t}/far/rt DREAMER_STATE_DIR={t}/far/st \
                 DREAMER_FORBID_DEFAULT_TMUX=1 \"PATH={path_env}\"\n",
                t = t.display()
            ),
        )
        .map_err(|e| e.to_string())?;
        let user = std::env::var("USER").map_err(|_| "USER is not set")?;
        let host_key =
            std::fs::read_to_string(t.join("host_ed25519.pub")).map_err(|e| e.to_string())?;
        std::fs::write(
            t.join("known_hosts"),
            format!("[127.0.0.1]:{port} {}", host_key.trim()),
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(
            t.join("ssh_config"),
            format!(
                "Host accept\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  \
                 IdentityFile {t}/id_ed25519\n  IdentitiesOnly yes\n  \
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
        let _sshd = Sshd(
            Command::new(&sshd)
                .args(["-D", "-f"])
                .arg(t.join("sshd_config"))
                .arg("-E")
                .arg(t.join("sshd.log"))
                .stdin(Stdio::null())
                .spawn()
                .map_err(|e| format!("{}: {e}", sshd.display()))?,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(t.join("sshd.log")).unwrap_or_default();
                return Err(format!("sshd did not start:\n{log}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Real ssh, before the test: a plain command in the private home.
        let probe = Command::new(&wrapper)
            .args(["-T", "-o", "BatchMode=yes", "accept", "printf %s \"$HOME\""])
            .output()
            .map_err(|e| e.to_string())?;
        let seen_home = String::from_utf8_lossy(&probe.stdout).into_owned();
        if !probe.status.success() || seen_home != t.join("home").display().to_string() {
            let log = std::fs::read_to_string(t.join("sshd.log")).unwrap_or_default();
            return Err(format!(
                "ssh to the private sshd failed or its HOME is not private ({seen_home:?}): {}\n{log}",
                String::from_utf8_lossy(&probe.stderr)
            ));
        }
        println!("acceptance: OpenSSH {}", openssh_version());
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "test",
                "--locked",
                "-p",
                "dreamer-server",
                "--test",
                "remote_acceptance",
                "--",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DREAMER_ACCEPT_DIR", &t)
            .env("DREAMER_ACCEPT_ARTIFACTS", &artifacts)
            .current_dir(&root)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            let log = std::fs::read_to_string(t.join("sshd.log")).unwrap_or_default();
            Err(format!("remote acceptance failed; sshd log:\n{log}"))
        }
    })();
    // Whatever is still running from the private directory (the host's
    // daemon, a bridge, an ssh client) goes with it. The pattern names
    // only this run's directory.
    for _ in 0..2 {
        let _ = Command::new("pkill")
            .arg("-f")
            .arg(format!("{}/", t.display()))
            .status();
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = std::fs::remove_dir_all(&t);
    result
}

fn openssh_version() -> String {
    Command::new("ssh")
        .arg("-V")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned())
        .unwrap_or_default()
}
