//! pi as a managed dependency: Dreamer installs it under its own data
//! directory, never the user's global packages, and switches between
//! releases with a pointer so the previous one is kept for rollback.
//!
//! pi publishes a standalone executable for each platform with every
//! GitHub release (`pi-<os>-<arch>.tar.gz`, beside `SHA256SUMS`), so no
//! Node runtime is needed. The newest release is the one pi's own installer
//! reads from `pi.dev`.
//!
//! ```text
//! <data dir>/agents/pi/
//!   1.5.1/pi           an unpacked release; its executable
//!   1.5.2/pi
//!   current -> 1.5.2   the active release
//!   previous -> 1.5.1  the one before it, kept for rollback
//! ```
//!
//! Login state comes only from pi's own `pi auth check`; nothing here reads
//! pi's credentials.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// The newest release, as pi's own installer finds it.
pub const LATEST_URL: &str = "https://pi.dev/api/installer/releases/latest";
/// Release assets: `<base>/v<version>/<asset>`.
pub const DOWNLOAD_URL: &str = "https://github.com/earendil-works/pi/releases/download";

/// Environment variables that would let pi bill an API account instead of
/// the user's own login; they are not passed to the login check.
pub const SCRUBBED: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
];

/// The release asset for this platform, or `None` where pi publishes none.
pub fn asset() -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    Some(format!("pi-{os}-{arch}.tar.gz"))
}

/// A release number: `X.Y.Z`, optionally with a `-pre.release` suffix.
/// Nothing else is used as a directory name.
pub fn valid_version(v: &str) -> bool {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && (v.len() == core.len()
            || (!pre.is_empty() && pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')))
}

/// Dreamer's own pi agent directory (`PI_CODING_AGENT_DIR`), used unless
/// the settings name another.
pub fn profile_dir() -> PathBuf {
    platform::data_dir().join("pi-profile")
}

/// The managed install.
#[derive(Debug, Clone)]
pub struct Install {
    pub root: PathBuf,
    latest_url: String,
    download_url: String,
}

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub version: String,
    /// The release that was active before, now kept for rollback.
    pub previous: Option<String>,
    /// It was the active release already; nothing changed.
    pub unchanged: bool,
}

impl Default for Install {
    fn default() -> Self {
        Install::new(platform::data_dir().join("agents").join("pi"))
    }
}

impl Install {
    /// The install at `root`. `DREAMER_PI_LATEST_URL` and
    /// `DREAMER_PI_DOWNLOAD_URL` replace the upstream addresses (a mirror).
    pub fn new(root: PathBuf) -> Install {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Install {
            root,
            latest_url: var("DREAMER_PI_LATEST_URL", LATEST_URL),
            download_url: var("DREAMER_PI_DOWNLOAD_URL", DOWNLOAD_URL),
        }
    }

    /// The active release's executable, through the `current` pointer, so
    /// the path stays the same across updates.
    pub fn command(&self) -> PathBuf {
        self.root.join("current").join("pi")
    }

    fn pointer(&self, name: &str) -> Option<String> {
        let target = std::fs::read_link(self.root.join(name)).ok()?;
        let v = target.file_name()?.to_str()?.to_owned();
        (valid_version(&v) && self.root.join(&v).join("pi").is_file()).then_some(v)
    }

    pub fn current(&self) -> Option<String> {
        self.pointer("current")
    }

    pub fn previous(&self) -> Option<String> {
        self.pointer("previous")
    }

    /// Every unpacked release, oldest first by version.
    pub fn installed(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| valid_version(n) && self.root.join(n).join("pi").is_file())
            .collect();
        out.sort_by_key(|v| order_key(v));
        out
    }

    /// The newest release number.
    pub fn latest(&self) -> Result<String, String> {
        let body = fetch_text(&self.latest_url)?;
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("{}: not the release metadata ({e})", self.latest_url))?;
        let version = v["version"]
            .as_str()
            .ok_or_else(|| format!("{}: no version in the reply", self.latest_url))?;
        if !valid_version(version) {
            return Err(format!(
                "{}: {version:?} is not a release number",
                self.latest_url
            ));
        }
        Ok(version.to_owned())
    }

    /// Install `version` (the newest when `None`) if it is not there yet,
    /// make it the active release, and keep the one before it. A download
    /// that fails its checksum, or a release that does not run, changes
    /// nothing.
    pub fn update(
        &self,
        version: Option<&str>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Updated, String> {
        let asset = asset().ok_or_else(|| {
            format!(
                "pi publishes no release for {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        let version = match version {
            Some(v) if valid_version(v) => v.to_owned(),
            Some(v) => return Err(format!("{v:?} is not a release number such as 1.5.2")),
            None => {
                progress("finding the newest pi release");
                self.latest()?
            }
        };
        std::fs::create_dir_all(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        let _lock = Lock::take(&self.root.join(".lock"))?;
        let before = self.current();
        if before.as_deref() == Some(version.as_str())
            && run_version(&self.root.join(&version).join("pi")).is_ok()
        {
            return Ok(Updated {
                version,
                previous: self.previous(),
                unchanged: true,
            });
        }
        let dir = self.root.join(&version);
        if run_version(&dir.join("pi")).ok().as_deref() != Some(version.as_str()) {
            self.fetch(&version, &asset, progress)?;
        }
        if let Some(b) = &before {
            self.point("previous", b)?;
        }
        self.point("current", &version)?;
        self.prune();
        Ok(Updated {
            version,
            previous: before,
            unchanged: false,
        })
    }

    /// Make the kept previous release the active one again (and the active
    /// one the kept one, so a second rollback undoes the first).
    pub fn rollback(&self) -> Result<String, String> {
        let _lock = Lock::take(&self.root.join(".lock"))?;
        let previous = self
            .previous()
            .ok_or("there is no previous pi release to roll back to")?;
        let current = self.current();
        self.point("current", &previous)?;
        if let Some(c) = current {
            self.point("previous", &c)?;
        }
        Ok(previous)
    }

    /// Download, check, and unpack one release into `<root>/<version>`.
    fn fetch(
        &self,
        version: &str,
        asset: &str,
        progress: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        let stage = self
            .root
            .join(format!(".stage-{version}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(stage.join("unpacked"))
            .map_err(|e| format!("{}: {e}", stage.display()))?;
        let result = (|| {
            let base = format!("{}/v{version}", self.download_url.trim_end_matches('/'));
            let sums = fetch_text(&format!("{base}/SHA256SUMS"))?;
            let want = sums
                .lines()
                .filter_map(|l| l.split_once(char::is_whitespace))
                .find(|(_, name)| name.trim().trim_start_matches('*') == asset)
                .map(|(sum, _)| sum.to_ascii_lowercase())
                .ok_or_else(|| format!("pi {version}: SHA256SUMS lists no {asset}"))?;
            let archive = stage.join(asset);
            progress(&format!("downloading pi {version} ({asset})"));
            download(&format!("{base}/{asset}"), &archive)?;
            let got = sha256_file(&archive)?;
            if got != want {
                return Err(format!(
                    "pi {version}: {asset} does not match its checksum (expected {want}, got {got}); nothing changed"
                ));
            }
            let unpacked = stage.join("unpacked");
            let out = run(
                Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&unpacked)
                    .args(["--strip-components", "1"]),
                Duration::from_secs(300),
            )?;
            if !out.status.success() {
                return Err(format!(
                    "unpacking {asset}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let reported = run_version(&unpacked.join("pi"))
                .map_err(|e| format!("pi {version} does not run here: {e}; nothing changed"))?;
            if reported != version {
                return Err(format!(
                    "the {version} release says it is {reported:?}; nothing changed"
                ));
            }
            let dir = self.root.join(version);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::rename(&unpacked, &dir).map_err(|e| format!("{}: {e}", dir.display()))
        })();
        let _ = std::fs::remove_dir_all(&stage);
        result
    }

    /// Point `name` at `version`, replacing the old pointer in one step.
    fn point(&self, name: &str, version: &str) -> Result<(), String> {
        let tmp = self.root.join(format!(".{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let to = self.root.join(name);
        std::fs::rename(&tmp, &to).map_err(|e| format!("{}: {e}", to.display()))
    }

    /// Remove releases that are neither active nor kept, and stale
    /// downloads.
    fn prune(&self) {
        let keep = [self.current(), self.previous()];
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let release = valid_version(&name) && !keep.iter().flatten().any(|k| *k == name);
            let stale = name.starts_with(".stage-");
            if (release || stale) && e.path().is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// Sort releases by number, not by text.
fn order_key(v: &str) -> (Vec<u64>, bool, String) {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    (
        core.split('.').map(|p| p.parse().unwrap_or(0)).collect(),
        // A release sorts after its pre-releases.
        pre.is_empty(),
        pre.to_owned(),
    )
}

/// One updater at a time.
#[derive(Debug)]
struct Lock(PathBuf);

impl Lock {
    fn take(path: &Path) -> Result<Lock, String> {
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    use std::io::Write as _;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Lock(path.to_path_buf()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|t| t.trim().parse::<u32>().ok());
                    match holder {
                        Some(pid) if platform::process_alive(pid) => {
                            return Err(format!(
                                "another update of pi is running (pid {pid}); try again when it ends"
                            ));
                        }
                        // Left by an update that died: take it over.
                        _ => {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Err(format!("{}: could not take the lock", path.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run `cmd` with no input, collecting its output, for at most `limit`.
fn run(cmd: &mut Command, limit: Duration) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(r) => r.map_err(|e| format!("{program}: {e}")),
        Err(_) => {
            let _ = platform::terminate(pid);
            Err(format!(
                "{program} did not finish within {}s",
                limit.as_secs()
            ))
        }
    }
}

fn curl(url: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "--connect-timeout", "20"])
        .arg(url);
    c
}

fn fetch_text(url: &str) -> Result<String, String> {
    let out = run(&mut curl(url), Duration::from_secs(60))
        .map_err(|e| format!("fetching {url}: {e} (is curl installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "fetching {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("{url}: not text"))
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let out = run(curl(url).arg("-o").arg(to), Duration::from_secs(900))
        .map_err(|e| format!("downloading {url}: {e} (is curl installed?)"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "downloading {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A pi command with the environment Dreamer runs it in: quiet about its
/// own updates (Dreamer manages those) and with no automatic network use.
pub(crate) fn pi_command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.env("PI_SKIP_VERSION_CHECK", "1").env("PI_OFFLINE", "1");
    c
}

/// What `pi --version` prints, trimmed.
pub fn run_version(program: &Path) -> Result<String, String> {
    if !program.is_file() {
        return Err(format!("{} does not exist", program.display()));
    }
    let out = run(
        pi_command(program).arg("--version"),
        Duration::from_secs(30),
    )?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        Err(format!(
            "`{} --version` failed: {}",
            program.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// pi's answer to whether a provider's login is ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub provider: String,
    /// `ready`, `not_ready`, or `invalid` (pi's words); `unknown` when pi
    /// did not answer.
    pub status: String,
    /// pi's reason, when it gives one.
    pub reason: Option<String>,
}

/// Ask pi (`pi auth check --provider P --no-refresh --json`) whether it is
/// logged in to `provider`, in agent directory `agent_dir`. API key
/// variables are left out, so only pi's own login counts. pi never prints
/// a credential for this command.
pub fn login(program: &Path, agent_dir: &Path, provider: &str) -> Login {
    auth_check(program, &[], agent_dir, provider, false)
}

/// `pi auth check --provider P --json` in agent directory `agent_dir`.
/// `prefix` are the arguments a configured command puts before pi's own
/// (`agents.pi.command`). With `refresh`, pi may renew an expired login
/// itself, as it would before a turn; without it, `--no-refresh`. It runs
/// with `PI_OFFLINE=1` and without API key variables.
pub fn auth_check(
    program: &Path,
    prefix: &[String],
    agent_dir: &Path,
    provider: &str,
    refresh: bool,
) -> Login {
    let mut c = pi_command(program);
    c.args(prefix)
        .args(["auth", "check", "--provider", provider])
        .args((!refresh).then_some("--no-refresh"))
        .arg("--json")
        .env("PI_CODING_AGENT_DIR", agent_dir);
    for v in SCRUBBED {
        c.env_remove(v);
    }
    let unknown = |why: String| Login {
        provider: provider.to_owned(),
        status: "unknown".into(),
        reason: Some(why),
    };
    let out = match run(&mut c, Duration::from_secs(20)) {
        Ok(o) => o,
        Err(e) => return unknown(e),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<serde_json::Value>(text.trim()) {
        Ok(v) if v["status"].is_string() => Login {
            provider: provider.to_owned(),
            status: v["status"].as_str().unwrap_or_default().to_owned(),
            reason: v["reason"].as_str().map(str::to_owned),
        },
        _ => unknown(format!(
            "pi gave no status: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// pi's subscription providers, the only ones Dreamer starts a model turn
/// on: OpenAI and OpenAI Codex (a ChatGPT subscription signs in to either)
/// and Anthropic (Claude Pro or Max).
pub const SUBSCRIPTION_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "anthropic"];

/// The provider Dreamer pins pi to: the one a `provider/id` model names,
/// else `defaultProvider` in the agent directory's `settings.json`, else
/// the one in Dreamer's own profile. Anything but a subscription provider
/// is refused, so pi never chooses a provider of its own.
pub fn pinned_provider(model: Option<&str>, agent_dir: &Path) -> Result<String, String> {
    let check = |p: &str, from: &str| {
        if SUBSCRIPTION_PROVIDERS.contains(&p) {
            Ok(p.to_owned())
        } else {
            Err(format!(
                "{from} names pi provider `{p}`; Dreamer runs pi only on a subscription login ({})",
                SUBSCRIPTION_PROVIDERS.join(", ")
            ))
        }
    };
    if let Some((p, _)) = model.and_then(|m| m.split_once('/')) {
        return check(p, &format!("the model `{}`", model.unwrap_or_default()));
    }
    let settings = agent_dir.join("settings.json");
    if let Ok(text) = std::fs::read_to_string(&settings) {
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("{}: not JSON ({e})", settings.display()))?;
        if let Some(p) = v["defaultProvider"].as_str() {
            return check(p, &settings.display().to_string());
        }
    }
    let shipped = crate::pi_profile::BASE
        .iter()
        .find(|(p, _)| *p == "settings.json")
        .and_then(|(_, t)| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v["defaultProvider"].as_str().map(str::to_owned))
        .unwrap_or_else(|| SUBSCRIPTION_PROVIDERS[0].to_owned());
    check(&shipped, "Dreamer's pi profile")
}

/// A word for a shell command line, quoted when it needs to be.
pub fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The command that logs pi in to `provider`, for a person to run.
pub fn login_command(
    program: &Path,
    prefix: &[String],
    agent_dir: &Path,
    provider: &str,
) -> String {
    let mut words = vec![
        format!(
            "PI_CODING_AGENT_DIR={}",
            shell_word(&agent_dir.display().to_string())
        ),
        shell_word(&program.display().to_string()),
    ];
    words.extend(prefix.iter().map(|a| shell_word(a)));
    words.extend(["auth".into(), "login".into(), provider.to_owned()]);
    words.join(" ")
}

/// The gate before every model turn Dreamer starts in pi: pi itself must
/// report a ready login for `provider` (`pi auth check`, which may renew an
/// expired login). The error says how to log in. Dreamer never reads pi's
/// credentials to decide this.
pub fn require_login(
    program: &Path,
    prefix: &[String],
    agent_dir: &Path,
    provider: &str,
) -> Result<(), String> {
    let l = auth_check(program, prefix, agent_dir, provider, true);
    if l.status == "ready" {
        return Ok(());
    }
    let why = match &l.reason {
        Some(r) if !r.is_empty() => format!("{}: {r}", l.status),
        _ => l.status.clone(),
    };
    Err(format!(
        "pi is not logged in to `{provider}` ({why}), so Dreamer starts no model turn \
         (without a login pi would fall back to a provider of its own). Log in with \
         `{}`, or run `/login {provider}` inside `dream agents pi`",
        login_command(program, prefix, agent_dir, provider)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_is_pinned_and_only_to_a_subscription() {
        let dir = std::env::temp_dir().join(format!("drm-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // Nothing in the agent directory: Dreamer's profile decides.
        let shipped = pinned_provider(None, &dir).expect("shipped");
        assert!(SUBSCRIPTION_PROVIDERS.contains(&shipped.as_str()));
        std::fs::write(
            dir.join("settings.json"),
            r#"{"defaultProvider":"anthropic"}"#,
        )
        .expect("settings");
        assert_eq!(pinned_provider(None, &dir).as_deref(), Ok("anthropic"));
        assert_eq!(
            pinned_provider(Some("openai-codex/gpt-x"), &dir).as_deref(),
            Ok("openai-codex")
        );
        // A bare model id keeps the settings' provider.
        assert_eq!(
            pinned_provider(Some("gpt-x"), &dir).as_deref(),
            Ok("anthropic")
        );
        assert!(pinned_provider(Some("radius/balanced"), &dir).is_err());
        std::fs::write(
            dir.join("settings.json"),
            r#"{"defaultProvider":"openrouter"}"#,
        )
        .expect("settings");
        assert!(pinned_provider(None, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_numbers_are_strict_and_sort_by_number() {
        for ok in ["1.5.2", "0.0.1", "10.20.30-beta.1"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in [
            "1.5", "v1.5.2", "1.5.2-", "1.5.2/..", "../1.5.2", "current", "",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
        let mut v = vec!["1.10.0", "1.9.3", "1.10.0-rc.1", "0.9.0"];
        v.sort_by_key(|v| order_key(v));
        assert_eq!(v, ["0.9.0", "1.9.3", "1.10.0-rc.1", "1.10.0"]);
    }
}
