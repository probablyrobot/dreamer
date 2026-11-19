//! Private git dependencies (Kosmonaut, `docs/dependency-baseline.md`) in
//! Linux containers, which have no GitHub credentials.
//!
//! Cargo fetches every git source `Cargo.lock` names, optional ones too, so
//! a container build needs the private repositories even when no feature
//! uses them. The host fetches them (with the owner's own git login, as
//! every other host build does), and [`docker_args`] hands each container a
//! bare mirror holding exactly the locked commit, mounted read-only, with
//! git told to read the repository's URL from it. No token or credential
//! enters a container, and the container's cargo still checks the commit
//! `Cargo.lock` pins.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Git repositories that need credentials. Anything else in `Cargo.lock`
/// is public and fetched by the container itself.
const PRIVATE: &[&str] = &["https://github.com/DeytaHQ/kosmonaut"];

/// One private source `Cargo.lock` pins: its URL, the ref the manifest
/// names (`refs/tags/v0.24.0`), and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    url: String,
    reference: String,
    commit: String,
}

/// The private git sources in `lock` (the text of `Cargo.lock`), each once.
fn pins(lock: &str) -> Vec<Pin> {
    let mut out: Vec<Pin> = Vec::new();
    for line in lock.lines() {
        let Some(source) = line
            .trim()
            .strip_prefix("source = \"git+")
            .and_then(|s| s.strip_suffix('"'))
        else {
            continue;
        };
        let Some((head, commit)) = source.split_once('#') else {
            continue;
        };
        let (url, query) = head.split_once('?').unwrap_or((head, ""));
        if !PRIVATE.contains(&url) {
            continue;
        }
        let reference = match query.split_once('=') {
            Some(("tag", t)) => format!("refs/tags/{t}"),
            Some(("branch", b)) => format!("refs/heads/{b}"),
            _ => format!("refs/pins/{commit}"),
        };
        let pin = Pin {
            url: url.to_owned(),
            reference,
            commit: commit.to_owned(),
        };
        if !out.contains(&pin) {
            out.push(pin);
        }
    }
    out
}

fn cargo_home() -> Result<PathBuf, String> {
    if let Some(h) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cargo"))
        .ok_or_else(|| "neither CARGO_HOME nor HOME is set".into())
}

fn git(args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The repository name a URL ends in: `kosmonaut`.
fn name(url: &str) -> &str {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git")
}

/// `docker run` arguments that give the container the private git sources
/// `root`'s `Cargo.lock` pins: a read-only mount of a bare mirror per
/// repository (made under `root/target/private-git/` from the host's cargo
/// cache, after `cargo fetch --locked` on the host), git's `insteadOf`
/// pointing the URL at it, and `CARGO_NET_GIT_FETCH_WITH_CLI` so cargo
/// fetches through git. Empty when nothing private is pinned.
pub fn docker_args(root: &Path) -> Result<Vec<String>, String> {
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|e| format!("{}: {e}", root.join("Cargo.lock").display()))?;
    let pins = pins(&lock);
    if pins.is_empty() {
        return Ok(Vec::new());
    }
    // The host's own build fetches the same sources; this makes sure they
    // are in its cache, and touches the network only when they are not.
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["fetch", "--locked"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo fetch: {e}"))?;
    if !status.success() {
        return Err(
            "cargo fetch --locked failed on the host; the private git dependencies need \
             your git login (see docs/dependency-baseline.md)"
                .into(),
        );
    }
    let db = cargo_home()?.join("git").join("db");
    let mirrors = root.join("target").join("private-git");
    std::fs::create_dir_all(&mirrors).map_err(|e| format!("{}: {e}", mirrors.display()))?;
    let mut args = vec![
        "-e".to_owned(),
        "CARGO_NET_GIT_FETCH_WITH_CLI=true".to_owned(),
    ];
    let mut config: Vec<(String, String)> = vec![
        // The mount belongs to the host's user, not the container's root.
        ("safe.directory".into(), "*".into()),
    ];
    for pin in &pins {
        let repo = name(&pin.url);
        // Cargo keeps a bare repository per source, named after it.
        let source = std::fs::read_dir(&db)
            .map_err(|e| format!("{}: {e}", db.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{repo}-")))
            })
            .find(|p| {
                let dir = p.display().to_string();
                let spec = format!("{}^{{commit}}", pin.commit);
                git(&["-C", &dir, "cat-file", "-e", &spec]).is_ok()
            })
            .ok_or_else(|| {
                format!(
                    "{} at {} is not in cargo's cache under {}",
                    pin.url,
                    pin.commit,
                    db.display()
                )
            })?;
        let mirror = mirrors.join(format!("{repo}.git"));
        let m = mirror.display().to_string();
        if !mirror.join("HEAD").is_file() {
            git(&["init", "--bare", "--quiet", &m])?;
            git(&["-C", &m, "config", "uploadpack.allowAnySHA1InWant", "true"])?;
        }
        let spec = format!("+{}:{}", pin.commit, pin.reference);
        let pinned = format!("+{0}:refs/pins/{0}", pin.commit);
        git(&[
            "-C",
            &source.display().to_string(),
            "push",
            "--quiet",
            &m,
            &spec,
            &pinned,
        ])?;
        let inside = format!("/private-git/{repo}.git");
        args.push("-v".into());
        args.push(format!("{m}:{inside}:ro"));
        config.push((format!("url.file://{inside}.insteadOf"), pin.url.clone()));
    }
    args.push("-e".into());
    args.push(format!("GIT_CONFIG_COUNT={}", config.len()));
    for (i, (k, v)) in config.iter().enumerate() {
        args.push("-e".into());
        args.push(format!("GIT_CONFIG_KEY_{i}={k}"));
        args.push("-e".into());
        args.push(format!("GIT_CONFIG_VALUE_{i}={v}"));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_git_sources_are_pinned() {
        let lock = r#"
[[package]]
name = "gpui"
source = "git+https://github.com/zed-industries/zed?rev=6ceb6fe#6ceb6fee8738274589b59139df27efc191ec2231"

[[package]]
name = "kosmonaut"
source = "git+https://github.com/DeytaHQ/kosmonaut?tag=v0.24.0#dbd5af275a7efa3f2ff3ca45afc45b40a214430e"

[[package]]
name = "ram-common"
source = "git+https://github.com/DeytaHQ/kosmonaut?tag=v0.24.0#dbd5af275a7efa3f2ff3ca45afc45b40a214430e"

[[package]]
name = "serde"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        assert_eq!(
            pins(lock),
            vec![Pin {
                url: "https://github.com/DeytaHQ/kosmonaut".into(),
                reference: "refs/tags/v0.24.0".into(),
                commit: "dbd5af275a7efa3f2ff3ca45afc45b40a214430e".into(),
            }]
        );
        assert_eq!(name("https://github.com/DeytaHQ/kosmonaut"), "kosmonaut");
        assert_eq!(name("https://github.com/a/b.git/"), "b");
    }
}
