//! Installing a server on a host: probe the host over
//! SSH, choose the archive for its OS and architecture, show the plan, and
//! carry out exactly that plan.
//!
//! The archive is taken from a directory of archives when there is one
//! with it (given, `DREAMER_SERVER_ARTIFACTS`, beside the daemon, or the
//! cache of earlier downloads), else downloaded from this version's
//! release ([`URL_ENV`] names another place) with the `curl` executable,
//! as Dreamer Web is: the daemon opens no network sockets of its own. A
//! download is checked against the release's `.sha256` before it is kept
//! or sent.
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

/// The variable that says where to download server archives from instead
/// of this version's release: an address (`https://…`, or `file://…`) of
/// a directory holding them and their `.sha256` files.
pub const URL_ENV: &str = "DREAMER_SERVER_URL";

/// The archive of the server for `version` on `target`, as a release
/// publishes it (its checksum beside it, with `.sha256` added).
pub fn archive_name(version: &str, target: &str) -> String {
    format!("dreamer-server-{version}-{target}.tar.gz")
}

/// Where the release of `version` publishes `file`.
pub fn release_url(version: &str, file: &str) -> String {
    format!(
        "{}/releases/download/v{version}/{file}",
        env!("CARGO_PKG_REPOSITORY")
    )
}

/// Where to download `file` from: the directory [`URL_ENV`] names, else
/// this version's release.
fn download_url(version: &str, file: &str) -> String {
    match std::env::var(URL_ENV) {
        Ok(base) if !base.trim().is_empty() => {
            format!("{}/{file}", base.trim().trim_end_matches('/'))
        }
        _ => release_url(version, file),
    }
}

/// The archives directory named for this install: given, else
/// `DREAMER_SERVER_ARTIFACTS`. Either is used alone, and nothing is
/// downloaded.
fn named_dir(given: Option<&str>) -> Option<PathBuf> {
    given
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("DREAMER_SERVER_ARTIFACTS").map(PathBuf::from))
        .filter(|d| !d.as_os_str().is_empty())
}

/// `share/dreamer/server` beside this daemon's `bin`.
fn beside_daemon() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join("share/dreamer/server")))
        .unwrap_or_else(|| PathBuf::from("share/dreamer/server"))
}

/// Where downloaded archives are kept: `server-archives/` in Dreamer's
/// data directory.
pub fn cache_dir() -> PathBuf {
    platform::data_dir().join("server-archives")
}

/// Where a plan takes the archive from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// An archive on this machine, checked against its `.sha256`.
    Local(PathBuf),
    /// The archive at this address, to download into [`cache_dir`].
    Download(String),
}

/// The archive `file` with its SHA-256 and size: from the named directory
/// if there is one, else from beside the daemon or the cache if it is
/// there, else to download (its SHA-256 from the `.sha256` published
/// beside it, its size from the server).
async fn locate(
    given: Option<&str>,
    file: &str,
    version: &str,
    target: &str,
) -> Result<(Source, String, u64), RpcError> {
    if let Some(dir) = named_dir(given) {
        return local(&dir.join(file), target).await;
    }
    for dir in [beside_daemon(), cache_dir()] {
        if dir.join(file).is_file() {
            return local(&dir.join(file), target).await;
        }
    }
    let url = download_url(version, file);
    let sum_url = format!("{url}.sha256");
    let unavailable = |why: String| {
        err(
            code::NOT_FOUND,
            format!(
                "{file} is not on this machine and could not be found at {url} ({why}). If it is \
                 not published there or cannot be reached from here, set {URL_ENV} to where it \
                 is, or give a directory that has it (--artifacts DIR or \
                 DREAMER_SERVER_ARTIFACTS); `cargo xtask package --platform server --target \
                 {target}` builds one"
            ),
        )
    };
    let listed = fetch_text(&sum_url).await.map_err(unavailable)?;
    let sum = listed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unavailable(format!("{sum_url} holds no SHA-256")));
    }
    let size = content_length(&url)
        .await
        .ok_or_else(|| unavailable("the server did not say its size".into()))?;
    Ok((Source::Download(url), sum, size))
}

/// An archive on this machine, its size, and its SHA-256, which must be
/// the one its `.sha256` lists.
async fn local(archive: &Path, target: &str) -> Result<(Source, String, u64), RpcError> {
    let file = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let size = std::fs::metadata(archive)
        .map_err(|e| {
            err(
                code::NOT_FOUND,
                format!(
                    "{}: {e}; build it with `cargo xtask package --platform server --target {target}`, \
                     or leave out the directory to download it from the release",
                    archive.display()
                ),
            )
        })?
        .len();
    let listed = std::fs::read_to_string(archive.with_file_name(format!("{file}.sha256")))
        .map_err(|e| err(code::NOT_FOUND, format!("{file}.sha256: {e}")))?;
    let listed = listed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let actual = sha256(archive).await.map_err(|e| err(code::INTERNAL, e))?;
    if !actual.eq_ignore_ascii_case(&listed) {
        return Err(err(
            code::PRECONDITION,
            format!("{file} does not match its .sha256 (corrupted or replaced)"),
        ));
    }
    Ok((Source::Local(archive.to_path_buf()), actual, size))
}

fn curl(url: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "--connect-timeout", "20"])
        .arg(url)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    c
}

/// The text at `url` (a `.sha256` file).
async fn fetch_text(url: &str) -> Result<String, String> {
    let out = tokio::time::timeout(Duration::from_secs(60), curl(url).output())
        .await
        .map_err(|_| "no answer within 60 s".to_owned())?
        .map_err(|e| format!("curl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if said.is_empty() {
            format!("curl: {}", out.status)
        } else {
            said
        })
    }
}

/// The size `url` says it has, if it says.
async fn content_length(url: &str) -> Option<u64> {
    let out = tokio::time::timeout(Duration::from_secs(60), curl(url).arg("-I").output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // After redirects, the last response's length is the file's.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<u64>().ok())
                .flatten()
        })
        .next_back()
}

/// Download `url` into [`cache_dir`] as `file`, saying how far it got
/// every so often, and keep it only if it has `size` bytes and SHA-256
/// `sum`. Returns where it is kept.
async fn download(url: &str, file: &str, sum: &str, size: u64) -> Result<PathBuf, String> {
    const MB: u64 = 1_000_000;
    let dir = cache_dir();
    if let Some(data) = dir.parent() {
        platform::private_dir(data)?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = dir.join(format!(".{file}.{}.part", std::process::id()));
    let result = async {
        crate::log::info(format!(
            "hosts: downloading {file} ({} MB) from {url}",
            size.div_ceil(MB)
        ));
        let mut child = curl(url)
            .arg("-o")
            .arg(&part)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        let started = tokio::time::Instant::now();
        let status = loop {
            match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                Ok(status) => break status.map_err(|e| format!("curl: {e}"))?,
                Err(_) if started.elapsed() > Duration::from_secs(30 * 60) => {
                    return Err("the download did not finish within 30 minutes".to_owned());
                }
                Err(_) => {
                    let got = std::fs::metadata(&part).map_or(0, |m| m.len());
                    crate::log::info(format!(
                        "hosts: downloading {file}: {} of {} MB",
                        got / MB,
                        size.div_ceil(MB)
                    ));
                }
            }
        };
        if !status.success() {
            let mut said = String::new();
            if let Some(mut e) = child.stderr.take() {
                use tokio::io::AsyncReadExt as _;
                let _ = e.read_to_string(&mut said).await;
            }
            let said = said.trim();
            return Err(if said.is_empty() {
                format!("curl: {status}")
            } else {
                said.to_owned()
            });
        }
        let got = std::fs::metadata(&part).map_or(0, |m| m.len());
        if got != size {
            return Err(format!("{got} bytes came, and the plan said {size}"));
        }
        let actual = sha256(&part).await?;
        if !actual.eq_ignore_ascii_case(sum) {
            return Err(format!(
                "its SHA-256 is {actual}, and the release's checksum says {sum}"
            ));
        }
        let kept = dir.join(file);
        std::fs::write(
            dir.join(format!("{file}.sha256")),
            format!("{sum}  {file}\n"),
        )
        .map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::rename(&part, &kept).map_err(|e| format!("{}: {e}", kept.display()))?;
        crate::log::info(format!("hosts: downloaded and checked {file}"));
        Ok(kept)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result.map_err(|e| format!("downloading {file} from {url}: {e}; nothing was installed"))
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
    Ok(plan_from(name, remote, artifacts).await?.0)
}

/// The plan, and where its archive comes from.
async fn plan_from(
    name: &str,
    remote: &Remote,
    artifacts: Option<&str>,
) -> Result<(api::InstallPlan, Source), RpcError> {
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
    let file = archive_name(&version, target);
    let (source, actual, size) = locate(artifacts, &file, &version, target).await?;
    let remote_dir = format!("{BASE}/versions/{version}");
    let mut steps = Vec::new();
    let archive = match &source {
        Source::Local(path) => path.display().to_string(),
        Source::Download(url) => {
            steps.push(format!(
                "Download {file} ({size} bytes) from {url} into {}, and check its SHA-256 against the release's: {actual}",
                cache_dir().display()
            ));
            url.clone()
        }
    };
    steps.extend([
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
    ]);
    let mut p = api::InstallPlan {
        name: name.to_owned(),
        destination: remote.destination.clone(),
        remote_os: os,
        remote_arch: arch,
        target: target.to_owned(),
        home,
        installed: (installed != "none").then_some(installed),
        version: version.clone(),
        archive,
        sha256: actual,
        size,
        remote_dir,
        steps,
        plan_id: String::new(),
    };
    p.plan_id = plan_id(&p);
    Ok((p, source))
}

/// Carry out the plan with id `plan_id`, if probing again gives the same.
pub async fn install(
    name: &str,
    remote: &Remote,
    artifacts: Option<&str>,
    plan_id: &str,
) -> Result<api::InstallResult, RpcError> {
    let (p, source) = plan_from(name, remote, artifacts).await?;
    if p.plan_id != plan_id {
        return Err(err(
            code::CONFLICT,
            "the plan changed since it was reviewed; review it again",
        ));
    }
    let archive = match source {
        Source::Local(path) => path,
        Source::Download(url) => download(
            &url,
            &archive_name(&p.version, &p.target),
            &p.sha256,
            p.size,
        )
        .await
        .map_err(|e| err(code::UNAVAILABLE, e))?,
    };
    let data = std::fs::read(&archive).map_err(|e| err(code::INTERNAL, e.to_string()))?;
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

/// Stop the host's daemon with the installed `dream shutdown`, which stops
/// a daemon from another build by its pid. tmux keeps running.
pub async fn stop_daemon(remote: &Remote) -> Result<String, String> {
    const STOP: &str = r#""$HOME/.local/share/dreamer/current/bin/dream" shutdown"#;
    run(remote, STOP, &[], None, Duration::from_secs(30)).await
}

/// The version in an installed `VERSION` file's first line
/// (`version 0.1.0`), if any.
pub fn installed_version(first_line: Option<&str>) -> Option<String> {
    first_line?
        .split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)
        .map(str::to_owned)
}

/// `a` is older than `b`, comparing dotted numbers (a missing or
/// non-numeric part counts as 0).
pub fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(a) < parts(b)
}

#[cfg(test)]
mod tests {
    use super::{archive_name, installed_version, older, release_url, target_for};

    #[test]
    fn a_release_publishes_each_targets_archive_under_its_version() {
        let file = archive_name("0.12.0", "aarch64-apple-darwin");
        assert_eq!(file, "dreamer-server-0.12.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            release_url("0.12.0", &file),
            "https://github.com/probablyrobot/dreamer/releases/download/v0.12.0/dreamer-server-0.12.0-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn versions_are_read_and_compared_as_numbers() {
        assert_eq!(
            installed_version(Some("version 0.1.0")),
            Some("0.1.0".into())
        );
        assert_eq!(installed_version(Some("0.1.0")), None);
        assert_eq!(installed_version(None), None);
        assert!(older("0.9.0", "0.10.0"));
        assert!(older("0.1.0", "0.1.1"));
        assert!(!older("1.0.0", "0.99.0"));
        assert!(!older("0.1.0", "0.1.0"));
    }

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
