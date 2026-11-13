//! Installing a server on a host (spec 11.2, 17.6): probe the host over
//! SSH, choose the archive for its OS and architecture, show the plan, and
//! carry out exactly that plan.
//!
//! Everything on the host runs as fixed POSIX scripts under `sh`, with the
//! variable parts as quoted arguments (`client::ssh::script_command`).
//! Nothing changes outside `~/.local/share/dreamer`: no root, no package
//! manager, no shell profiles. The archive's SHA-256 is checked here before
//! sending and on the host before anything is unpacked, and the switch to
//! the new version is one atomic rename, so a failure leaves the previous
//! version in place.

use std::path::{Path, PathBuf};
use std::time::Duration;

use api::{RpcError, code};
use client::ssh::{Mode, Remote};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Where the server lives on the host, relative to `$HOME`.
const BASE: &str = ".local/share/dreamer";

const PROBE: &str = r#"uname -s; uname -m; printf '%s\n' "$HOME"; v="$HOME/.local/share/dreamer/current/VERSION"; if [ -f "$v" ]; then head -n 1 "$v"; else echo none; fi"#;

/// `$1` version, `$2` SHA-256; the archive on standard input.
const INSTALL: &str = r#"set -eu; v="$1"; sum="$2"; base="$HOME/.local/share/dreamer"; mkdir -p "$base/versions"; tmp="$base/.upload.$$"; stage="$base/versions/.stage.$$"; trap 'rm -rf "$tmp" "$stage" "$base/.current.$$"' EXIT; cat > "$tmp"; if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp" | cut -d " " -f 1); else got=$(shasum -a 256 "$tmp" | cut -d " " -f 1); fi; if [ "$got" != "$sum" ]; then echo "checksum mismatch: expected $sum, got $got; nothing was changed" >&2; exit 3; fi; mkdir -p "$stage"; tar -xzf "$tmp" -C "$stage"; set -- "$stage"/*; if [ ! -x "$1/bin/dream" ]; then echo "the archive has no bin/dream; nothing was changed" >&2; exit 4; fi; "$1/bin/dream" --version >/dev/null; rm -rf "$base/versions/$v"; mv "$1" "$base/versions/$v"; prev=$(readlink "$base/current" 2>/dev/null || true); ln -s "versions/$v" "$base/.current.$$"; mv -T "$base/.current.$$" "$base/current" 2>/dev/null || mv -fh "$base/.current.$$" "$base/current"; echo "previous ${prev:-none}"; "$base/current/bin/dream" --version"#;

fn ssh_program() -> String {
    std::env::var("DREAMER_SSH").unwrap_or_else(|_| "ssh".into())
}

fn err(code: i32, m: impl Into<String>) -> RpcError {
    RpcError::new(code, m)
}

/// Run `script` on the host; its standard output, or why it failed (with
/// SSH's and the script's own words).
async fn run(
    remote: &Remote,
    script: &str,
    args: &[&str],
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<String, String> {
    let argv = client::ssh::script_command(
        &ssh_program(),
        &remote.destination,
        script,
        args,
        Mode::Batch,
    )
    .map_err(|e| e.to_string())?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        });
    }
    let out = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| format!("the host did not finish within {} s", limit.as_secs()))?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(format!(
            "{} ({})",
            if said.is_empty() { "failed" } else { &said },
            out.status
        ))
    }
}

/// The Rust target for `uname -s` and `uname -m`.
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-gnu"),
        ("Darwin", "arm64" | "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// The archives directory: given, else `DREAMER_SERVER_ARTIFACTS`, else
/// `share/dreamer/server` beside this daemon's `bin`.
fn artifacts_dir(given: Option<&str>) -> PathBuf {
    if let Some(d) = given {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("DREAMER_SERVER_ARTIFACTS") {
        return PathBuf::from(d);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join("share/dreamer/server")))
        .unwrap_or_else(|| PathBuf::from("share/dreamer/server"))
}

/// SHA-256 of a local file with the system's tool.
async fn sha256(path: &Path) -> Result<String, String> {
    for (tool, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        let Ok(out) = Command::new(tool).args(args).arg(path).output().await else {
            continue;
        };
        if out.status.success()
            && let Some(sum) = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
        {
            return Ok(sum.to_owned());
        }
    }
    Err("neither sha256sum nor shasum is available".into())
}

fn plan_id(p: &api::InstallPlan) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        &p.destination,
        &p.target,
        &p.version,
        &p.sha256,
        &p.installed,
        &p.archive,
    )
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Probe the host and say what installing would do.
pub async fn plan(
    name: &str,
    remote: &Remote,
    artifacts: Option<&str>,
) -> Result<api::InstallPlan, RpcError> {
    let probe = run(remote, PROBE, &[], None, Duration::from_secs(60))
        .await
        .map_err(|e| err(code::UNAVAILABLE, format!("probing `{name}`: {e}")))?;
    let mut lines = probe.lines().map(str::trim);
    let (os, arch, home, installed) = (
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or_default().to_owned(),
        lines.next().unwrap_or("none").to_owned(),
    );
    let target = target_for(&os, &arch).ok_or_else(|| {
        err(
            code::PRECONDITION,
            format!("no server is built for {os} {arch} (`{name}`)"),
        )
    })?;
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let dir = artifacts_dir(artifacts);
    let file = format!("dreamer-server-{version}-{target}.tar.gz");
    let archive = dir.join(&file);
    let size = std::fs::metadata(&archive)
        .map_err(|e| {
            err(
                code::NOT_FOUND,
                format!(
                    "{}: {e}; build it with `cargo xtask package --platform server --target {target}`",
                    archive.display()
                ),
            )
        })?
        .len();
    let listed = std::fs::read_to_string(dir.join(format!("{file}.sha256")))
        .map_err(|e| err(code::NOT_FOUND, format!("{file}.sha256: {e}")))?;
    let listed = listed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let actual = sha256(&archive).await.map_err(|e| err(code::INTERNAL, e))?;
    if actual != listed {
        return Err(err(
            code::PRECONDITION,
            format!("{file} does not match its .sha256 (corrupted or replaced)"),
        ));
    }
    let remote_dir = format!("{BASE}/versions/{version}");
    let mut p = api::InstallPlan {
        name: name.to_owned(),
        destination: remote.destination.clone(),
        remote_os: os,
        remote_arch: arch,
        target: target.to_owned(),
        home,
        installed: (installed != "none").then_some(installed),
        version: version.clone(),
        archive: archive.display().to_string(),
        sha256: actual.clone(),
        size,
        remote_dir: remote_dir.clone(),
        steps: vec![
            format!(
                "Send {file} ({size} bytes) to {} over SSH",
                remote.destination
            ),
            format!("Check its SHA-256 on the host: {actual}"),
            format!("Unpack it into ~/{remote_dir}"),
            format!(
                "Point ~/{BASE}/current at it with one atomic rename; earlier versions stay in ~/{BASE}/versions"
            ),
            format!(
                "Run ~/{BASE}/current/bin/dream --version there. Nothing outside ~/{BASE} changes: no root, no package manager, no shell profiles"
            ),
        ],
        plan_id: String::new(),
    };
    p.plan_id = plan_id(&p);
    Ok(p)
}

/// Carry out the plan with id `plan_id`, if probing again gives the same.
pub async fn install(
    name: &str,
    remote: &Remote,
    artifacts: Option<&str>,
    plan_id: &str,
) -> Result<api::InstallResult, RpcError> {
    let p = plan(name, remote, artifacts).await?;
    if p.plan_id != plan_id {
        return Err(err(
            code::CONFLICT,
            "the plan changed since it was reviewed; review it again",
        ));
    }
    let data = std::fs::read(&p.archive).map_err(|e| err(code::INTERNAL, e.to_string()))?;
    let out = run(
        remote,
        INSTALL,
        &[&p.version, &p.sha256],
        Some(&data),
        Duration::from_secs(300),
    )
    .await
    .map_err(|e| err(code::UNAVAILABLE, format!("installing on `{name}`: {e}")))?;
    let mut previous = None;
    let mut check = String::new();
    for line in out.lines() {
        match line.strip_prefix("previous ") {
            Some("none") => {}
            Some(v) => previous = Some(v.trim().to_owned()),
            None if !line.trim().is_empty() => check = line.trim().to_owned(),
            None => {}
        }
    }
    Ok(api::InstallResult {
        version: p.version,
        remote_dream: format!("~/{}", client::ssh::INSTALLED_DREAM),
        previous,
        check,
    })
}

#[cfg(test)]
mod tests {
    use super::target_for;

    #[test]
    fn hosts_get_the_archive_for_their_os_and_architecture() {
        assert_eq!(
            target_for("Linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("Linux", "aarch64"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("Linux", "arm64"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(target_for("Darwin", "arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("Darwin", "x86_64"), None);
        assert_eq!(target_for("FreeBSD", "amd64"), None);
    }
}
