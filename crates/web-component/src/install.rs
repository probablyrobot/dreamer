//! The one installer of Dreamer Web, shared by `dream web install` and the
//! desktop (the button in a web pane, the first-start offer, the palette).
//!
//! An install finds the component (a copy already on the system, an
//! archive downloaded or given, or an unpacked directory), checks it (its
//! manifest names this Dreamer and its CEF release, every file is there,
//! the framework and each helper app pass `codesign --verify --strict`,
//! and a helper loads the framework from where it sits), and only then
//! swaps it in by a rename next to where it goes. On any failure the copy
//! that was there stays as it was.
//!
//! A copy already on the system ([`crate::system_dirs`]) is not copied:
//! `web/` becomes a link to it, after it passed the same checks. Downloads
//! run the `curl` executable (Dreamer itself opens no network sockets),
//! and report their progress as the file grows. Everything here blocks:
//! callers run it off any UI thread.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::{
    FRAMEWORK, HELPERS, Manifest, NAME, State, TOP, URL_ENV, VERSION, archive_name, check_for,
    helper_app, helper_executable, linked_to,
};

/// Where an install takes the component from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A copy already on the system if there is one, else the archive at
    /// this address (`https://…`, or `file://…`), checked against the
    /// `.sha256` beside it.
    Find(String),
    /// The archive at this address, and nothing already on the system.
    Download(String),
    /// An archive on disk, checked against the `.sha256` beside it if
    /// there is one.
    Archive(PathBuf),
    /// An unpacked component directory, copied.
    Dir(PathBuf),
}

impl Source {
    /// What `dream web install ARG` means: an address, a directory, or an
    /// archive.
    pub fn from_arg(arg: &str, target: &str) -> Source {
        if ["https://", "http://", "file://"]
            .iter()
            .any(|p| arg.starts_with(p))
        {
            Source::Download(crate::archive_url(arg, target))
        } else if Path::new(arg).is_dir() {
            Source::Dir(PathBuf::from(arg))
        } else {
            Source::Archive(PathBuf::from(arg))
        }
    }
}

/// Where an install is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Looking for a copy already on the system.
    Looking,
    /// Downloading: bytes so far, of how many if the server said.
    Downloading {
        got: u64,
        total: Option<u64>,
    },
    Unpacking,
    /// Checking the manifest, the signatures, and that a helper loads the
    /// framework.
    Checking,
    /// Swapping it in.
    Installing,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MB: u64 = 1_000_000;
        match self {
            Step::Looking => write!(f, "Looking for {NAME} on this Mac"),
            Step::Downloading {
                got,
                total: Some(total),
            } if *total > 0 => write!(
                f,
                "Downloading {NAME}: {} of {} MB",
                got / MB,
                total.div_ceil(MB)
            ),
            Step::Downloading { got, .. } => write!(f, "Downloading {NAME}: {} MB", got / MB),
            Step::Unpacking => write!(f, "Unpacking {NAME}"),
            Step::Checking => write!(f, "Checking {NAME}"),
            Step::Installing => write!(f, "Installing {NAME}"),
        }
    }
}

/// Where the component that was installed came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// It was there already, this build's: nothing changed.
    AlreadyInstalled,
    /// A copy on the system, now used in place (linked, not copied).
    InPlace(PathBuf),
    Downloaded(String),
    Archive(PathBuf),
    Dir(PathBuf),
}

/// An install that succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub manifest: Manifest,
    /// Where it is installed (`web/` in Dreamer's data directory).
    pub dir: PathBuf,
    pub origin: Origin,
}

/// Installs one component at one place.
#[derive(Debug)]
pub struct Installer {
    /// Where the component goes.
    pub dest: PathBuf,
    /// The manifest the component must have.
    pub want: Manifest,
    /// Copies already on the system to try first ([`Source::Find`]).
    pub system: Vec<PathBuf>,
    /// The checks beyond the files being there (signatures, and the helper
    /// loading the framework): [`verify_signed`] for real installs.
    pub verify: fn(&Path) -> Result<(), String>,
}

impl Installer {
    /// This build's installer: its component, into `web/` of Dreamer's
    /// data directory, trying [`crate::system_dirs`] first.
    pub fn for_this_build() -> Result<Installer, String> {
        let want = Manifest::this_build().ok_or_else(unsupported)?;
        Ok(Installer {
            dest: crate::installed_dir(),
            want,
            system: crate::system_dirs(),
            verify: verify_signed,
        })
    }

    fn check(&self, dir: &Path) -> State {
        check_for(dir, Some(&self.want))
    }

    /// Install from `source`, reporting each step to `progress`. On any
    /// failure what was installed stays as it was.
    pub fn install(
        &self,
        source: &Source,
        progress: &mut dyn FnMut(Step),
    ) -> Result<Installed, String> {
        let parent = self.parent()?;
        let _lock = lock(&parent)?;
        if let Source::Find(_) = source
            && let Some(done) = self.in_place(&parent, progress)?
        {
            return Ok(done);
        }
        // Beside the destination, so the swap is a rename.
        let staging = parent.join(unique(".web-install"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        let result = self.stage_and_swap(source, &staging, &parent, progress);
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// Use a copy already on the system, if there is one and nothing of
    /// this build's is installed; never downloads.
    pub fn use_system_copy(
        &self,
        progress: &mut dyn FnMut(Step),
    ) -> Result<Option<Installed>, String> {
        let parent = self.parent()?;
        let _lock = lock(&parent)?;
        self.in_place(&parent, progress)
    }

    /// The data directory the component goes in, made private (as the
    /// daemon requires it to be) if it was not there.
    fn parent(&self) -> Result<PathBuf, String> {
        let parent = self
            .dest
            .parent()
            .ok_or_else(|| format!("{}: no parent directory", self.dest.display()))?
            .to_path_buf();
        platform::private_dir(&parent)?;
        Ok(parent)
    }

    /// What is installed if it is this build's, else a copy on the system
    /// linked in place of it, if there is one.
    fn in_place(
        &self,
        parent: &Path,
        progress: &mut dyn FnMut(Step),
    ) -> Result<Option<Installed>, String> {
        if let State::Ready(m) = self.check(&self.dest) {
            return Ok(Some(self.installed(m, Origin::AlreadyInstalled)));
        }
        progress(Step::Looking);
        let Some(found) = self.find_on_system() else {
            return Ok(None);
        };
        progress(Step::Installing);
        let m = self.link(&found, parent)?;
        Ok(Some(self.installed(m, Origin::InPlace(found))))
    }

    fn stage_and_swap(
        &self,
        source: &Source,
        staging: &Path,
        parent: &Path,
        progress: &mut dyn FnMut(Step),
    ) -> Result<Installed, String> {
        let unpacked = staging.join(TOP);
        let origin = match source {
            Source::Dir(path) => {
                progress(Step::Unpacking);
                copy_dir(path, &unpacked)?;
                Origin::Dir(path.clone())
            }
            Source::Archive(zip) => {
                verify_sum(zip, None)?;
                progress(Step::Unpacking);
                unzip(zip, staging)?;
                Origin::Archive(zip.clone())
            }
            Source::Find(url) | Source::Download(url) => {
                let zip = staging.join(archive_name(&self.want.version, &self.want.target));
                download(url, &zip, progress)?;
                let sum = fetch_text(&format!("{url}.sha256"))
                    .map_err(|e| download_failed(&format!("{url}.sha256"), &e))?;
                verify_sum(&zip, Some(&sum))?;
                progress(Step::Unpacking);
                unzip(&zip, staging)?;
                let _ = std::fs::remove_file(&zip);
                Origin::Downloaded(url.clone())
            }
        };
        progress(Step::Checking);
        let manifest = self.verify_dir(&unpacked)?;
        progress(Step::Installing);
        swap(&unpacked, &self.dest, parent)?;
        Ok(self.installed(manifest, origin))
    }

    fn installed(&self, manifest: Manifest, origin: Origin) -> Installed {
        Installed {
            manifest,
            dir: self.dest.clone(),
            origin,
        }
    }

    /// `dir` holds this build's component, complete, and passes
    /// [`Installer::verify`].
    fn verify_dir(&self, dir: &Path) -> Result<Manifest, String> {
        let manifest = match self.check(dir) {
            State::Ready(m) => m,
            State::Other(m) => {
                return Err(format!(
                    "this is {NAME} for Dreamer {} (CEF {}), and this is Dreamer {} (CEF {}): \
                     install {}",
                    m.version,
                    m.cef,
                    self.want.version,
                    self.want.cef,
                    archive_name(&self.want.version, &self.want.target)
                ));
            }
            State::Missing => return Err(format!("{}: not found", dir.display())),
            State::Broken(why) => return Err(format!("not a usable {NAME}: {why}")),
        };
        (self.verify)(dir)?;
        Ok(manifest)
    }

    /// The first copy on the system that is exactly this build's component,
    /// is not writable by other users, and passes the checks.
    pub fn find_on_system(&self) -> Option<PathBuf> {
        self.system.iter().find_map(|dir| {
            let real = dir.canonicalize().ok()?;
            if real == self.dest.canonicalize().unwrap_or_default() {
                return None;
            }
            if !matches!(self.check(&real), State::Ready(_)) || writable_by_others(&real) {
                return None;
            }
            self.verify_dir(&real).ok()?;
            Some(real)
        })
    }

    /// Make the destination a link to `found`.
    fn link(&self, found: &Path, parent: &Path) -> Result<Manifest, String> {
        let link = parent.join(unique(".web-link"));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(found, &link).map_err(|e| format!("{}: {e}", link.display()))?;
        let manifest = match self.check(&link) {
            State::Ready(m) => m,
            _ => {
                let _ = std::fs::remove_file(&link);
                return Err(format!("{}: changed while linking it", found.display()));
            }
        };
        if let Err(e) = swap(&link, &self.dest, parent) {
            let _ = std::fs::remove_file(&link);
            return Err(e);
        }
        Ok(manifest)
    }
}

/// Remove what is installed at `dir`: the component, or the link to a copy
/// used in place (the copy itself is left). Returns what was removed.
pub fn remove(dir: &Path) -> Result<Option<Origin>, String> {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Ok(None);
    };
    if meta.file_type().is_symlink() {
        let to = linked_to(dir).unwrap_or_default();
        std::fs::remove_file(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        return Ok(Some(Origin::InPlace(to)));
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(Some(Origin::Dir(dir.to_path_buf())))
}

fn unsupported() -> String {
    format!("{NAME} is built for macOS on Apple Silicon only so far")
}

/// A name in the data directory no other install (in this process or
/// another) uses.
fn unique(prefix: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// One install at a time in a data directory (the CLI's and the desktop's
/// alike): held until the returned file is dropped.
///
/// A lock just let go can still be held for a moment by a process another
/// thread is starting (forked, not yet running its program), so a busy
/// lock is tried again for a little while before it counts as another
/// install's.
fn lock(parent: &Path) -> Result<std::fs::File, String> {
    let path = parent.join(".web-install.lock");
    let f = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    for _ in 0..10 {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("{}: {e}", path.display()));
            }
        }
    }
    Err(format!(
        "another install of {NAME} is running; try again when it is done"
    ))
}

fn writable_by_others(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(dir).map_or(true, |m| m.mode() & 0o022 != 0)
}

fn curl(url: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "--connect-timeout", "20"])
        .arg(url);
    c
}

/// Why a download failed, and what to do about it.
fn download_failed(url: &str, why: &str) -> String {
    format!(
        "{NAME} could not be downloaded from {url} ({why}). If it is not published there or \
         cannot be reached from here, set {URL_ENV} or `web.url` in dreamer.yaml to where the \
         archive is, or install one you have with `dream web install FILE.zip`."
    )
}

/// The size `url` says it has, if it says.
fn content_length(url: &str) -> Option<u64> {
    let out = curl(url).arg("-I").stderr(Stdio::null()).output().ok()?;
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

/// Download `url` to `to`, reporting how much has come every so often.
fn download(url: &str, to: &Path, progress: &mut dyn FnMut(Step)) -> Result<(), String> {
    let total = content_length(url);
    progress(Step::Downloading { got: 0, total });
    let mut child = curl(url)
        .arg("-o")
        .arg(to)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| download_failed(url, &format!("curl: {e}")))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let got = std::fs::metadata(to).map_or(0, |m| m.len());
                progress(Step::Downloading { got, total });
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(download_failed(url, &e.to_string())),
        }
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        let err = err.trim();
        return Err(download_failed(
            url,
            if err.is_empty() { "curl failed" } else { err },
        ));
    }
    let got = std::fs::metadata(to).map_or(0, |m| m.len());
    progress(Step::Downloading {
        got,
        total: Some(got),
    });
    Ok(())
}

fn fetch_text(url: &str) -> Result<String, String> {
    run(&mut curl(url), "curl")
}

/// Run `cmd`; its standard output, or why it failed.
fn run(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("ditto")
    } else {
        let mut c = Command::new("cp");
        c.arg("-a");
        c
    };
    run(cmd.arg(from).arg(to), "copying the component").map(drop)
}

fn unzip(zip: &Path, into: &Path) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("ditto");
        c.args(["-x", "-k"]).arg(zip).arg(into);
        c
    } else {
        let mut c = Command::new("unzip");
        c.arg("-q").arg(zip).arg("-d").arg(into);
        c
    };
    run(&mut cmd, &format!("unpacking {}", zip.display()))?;
    if into.join(TOP).is_dir() {
        Ok(())
    } else {
        Err(format!(
            "{} is not a {NAME} archive (it has no {TOP}/)",
            zip.display()
        ))
    }
}

/// Check `zip` against `sum` (a `.sha256` file's text), or against the
/// `.sha256` file beside it if there is one.
fn verify_sum(zip: &Path, sum: Option<&str>) -> Result<(), String> {
    let beside = PathBuf::from(format!("{}.sha256", zip.display()));
    let text = match sum {
        Some(s) => s.to_owned(),
        None => match std::fs::read_to_string(&beside) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        },
    };
    let want = text.split_whitespace().next().unwrap_or_default();
    let got = sha256_file(zip)?;
    if want.eq_ignore_ascii_case(&got) {
        Ok(())
    } else {
        Err(format!(
            "{}: SHA-256 {got}, but its checksum says {want}",
            zip.display()
        ))
    }
}

/// The SHA-256 of the file at `path`, in hex.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest as _;
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
    Ok(h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}

/// The checks of a real component beyond its files: the framework and
/// each helper app pass `codesign --verify --strict`, and a helper loads
/// the framework from where it sits.
pub fn verify_signed(dir: &Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err(unsupported());
    }
    let mut signed = vec![dir.join(FRAMEWORK)];
    signed.extend(
        HELPERS
            .iter()
            .map(|(suffix, _)| dir.join(helper_app(suffix))),
    );
    for path in signed {
        run(
            Command::new("codesign")
                .args(["--verify", "--strict"])
                .arg(&path),
            &format!("codesign --verify {}", path.display()),
        )?;
    }
    // With no process type, CEF's entry point returns at once (-1): this
    // checks only that the helper finds and loads the framework.
    let helper = helper_executable(dir, "");
    let out = Command::new(&helper)
        .output()
        .map_err(|e| format!("{}: {e}", helper.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(255) || stderr.contains("dreamer-web-helper:") {
        return Err(format!(
            "the helper did not load the framework ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(())
}

/// Put `new` (a directory, or a link to one) at `dir`, moving whatever is
/// there away first, and back if the move fails.
fn swap(new: &Path, dir: &Path, parent: &Path) -> Result<(), String> {
    let old = parent.join(unique(".web-old"));
    let had = std::fs::symlink_metadata(dir).is_ok();
    if had {
        std::fs::rename(dir, &old).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(new, dir) {
        if had {
            let _ = std::fs::rename(&old, dir);
        }
        return Err(format!("{}: {e}", dir.display()));
    }
    if had {
        // A link is removed, not what it points to.
        let _ = remove(&old);
    }
    Ok(())
}

/// What the installed component is, for `dream web status`: where it is,
/// and where it came from when it is a copy used in place.
pub fn describe(dir: &Path, m: &Manifest) -> String {
    let at = match linked_to(dir) {
        Some(to) => format!("{} (used in place, from {})", dir.display(), to.display()),
        None => dir.display().to_string(),
    };
    format!(
        "{NAME} {} (Chromium {}, for Dreamer {VERSION}) in {at}",
        m.version,
        m.chromium()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAMEWORK_BINARY, MANIFEST};

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Temp {
            let d = std::env::temp_dir().join(format!(
                "web-install-{name}-{}-{}",
                std::process::id(),
                unique("t")
            ));
            let _ = std::fs::remove_dir_all(&d);
            platform::private_dir(&d).expect("temp dir");
            Temp(d)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn want() -> Manifest {
        Manifest {
            version: "9.9.9".into(),
            cef: crate::CEF_VERSION.into(),
            target: "test-target".into(),
        }
    }

    /// A fake component: every file the check looks for, with `manifest`.
    fn lay_out(dir: &Path, manifest: &Manifest) {
        std::fs::create_dir_all(dir.join(FRAMEWORK)).expect("framework dir");
        std::fs::write(dir.join(FRAMEWORK_BINARY), b"framework").expect("framework");
        for (suffix, _) in HELPERS {
            let exe = helper_executable(dir, suffix);
            std::fs::create_dir_all(exe.parent().expect("parent")).expect("helper dir");
            std::fs::write(exe, b"helper").expect("helper");
        }
        std::fs::write(dir.join(MANIFEST), manifest.to_json()).expect("manifest");
    }

    fn installer(root: &Path, system: Vec<PathBuf>) -> Installer {
        Installer {
            dest: root.join("data/web"),
            want: want(),
            system,
            verify: |_| Ok(()),
        }
    }

    /// A zip of `dir` (as `dreamer-web/`) and its `.sha256`, beside it.
    #[cfg(target_os = "macos")]
    fn zip_of(dir: &Path, to: &Path) {
        let top = dir.parent().expect("parent").join(TOP);
        if top != dir {
            std::fs::rename(dir, &top).expect("rename");
        }
        let ok = Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&top)
            .arg(to)
            .status()
            .expect("ditto");
        assert!(ok.success());
        let sum = sha256_file(to).expect("sum");
        std::fs::write(
            format!("{}.sha256", to.display()),
            format!("{sum}  x.zip\n"),
        )
        .expect("sum file");
    }

    #[test]
    fn a_directory_installs_and_a_bad_one_keeps_what_was_there() {
        let t = Temp::new("dir");
        let i = installer(&t.0, Vec::new());
        let good = t.0.join("good");
        lay_out(&good, &want());
        let mut p = |_: Step| {};
        let done = i
            .install(&Source::Dir(good.clone()), &mut p)
            .expect("install");
        assert_eq!(done.manifest, want());
        assert_eq!(done.origin, Origin::Dir(good));
        assert_eq!(i.check(&i.dest), State::Ready(want()));

        // Another version's component is refused, and the installed one
        // stays.
        let other = t.0.join("other");
        lay_out(
            &other,
            &Manifest {
                version: "0.0.1".into(),
                ..want()
            },
        );
        let e = i.install(&Source::Dir(other), &mut p).expect_err("other");
        assert!(e.contains("for Dreamer 0.0.1"), "{e}");
        assert_eq!(i.check(&i.dest), State::Ready(want()));

        // So is one that fails the signature checks.
        let unsigned = Installer {
            verify: |_| Err("codesign said no".into()),
            ..installer(&t.0, Vec::new())
        };
        let again = t.0.join("again");
        lay_out(&again, &want());
        let e = unsigned
            .install(&Source::Dir(again), &mut p)
            .expect_err("unsigned");
        assert!(e.contains("codesign said no"), "{e}");
        assert_eq!(i.check(&i.dest), State::Ready(want()));
        // No staging is left behind.
        let left: Vec<_> = std::fs::read_dir(t.0.join("data"))
            .expect("data")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "web" && n != ".web-install.lock")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_copy_on_the_system_is_used_in_place_and_removing_it_leaves_the_copy() {
        let t = Temp::new("system");
        let wrong = t.0.join("wrong");
        lay_out(
            &wrong,
            &Manifest {
                cef: "1.0.0+chromium-1".into(),
                ..want()
            },
        );
        let system = t.0.join("system/web");
        lay_out(&system, &want());
        let i = installer(&t.0, vec![t.0.join("nowhere"), wrong, system.clone()]);
        let mut steps = Vec::new();
        let done = i
            .install(&Source::Find("file:///nowhere.zip".into()), &mut |s| {
                steps.push(s);
            })
            .expect("found");
        let real = system.canonicalize().expect("real");
        assert_eq!(done.origin, Origin::InPlace(real.clone()));
        assert_eq!(steps, [Step::Looking, Step::Installing]);
        assert_eq!(linked_to(&i.dest), Some(real.clone()));
        assert_eq!(i.check(&i.dest), State::Ready(want()));
        assert!(describe(&i.dest, &want()).contains("used in place"));

        // Ready now: asking again changes nothing.
        let again = i
            .install(&Source::Find("file:///nowhere.zip".into()), &mut |_| {})
            .expect("again");
        assert_eq!(again.origin, Origin::AlreadyInstalled);
        assert_eq!(
            i.use_system_copy(&mut |_| {})
                .expect("no change")
                .map(|d| d.origin),
            Some(Origin::AlreadyInstalled)
        );

        assert_eq!(remove(&i.dest), Ok(Some(Origin::InPlace(real))));
        assert!(std::fs::symlink_metadata(&i.dest).is_err());
        assert_eq!(i.check(&system), State::Ready(want()));
        assert_eq!(remove(&i.dest), Ok(None));

        // Without a copy on the system, nothing is downloaded for it.
        let bare = installer(&t.0, vec![t.0.join("nowhere")]);
        assert_eq!(bare.use_system_copy(&mut |_| {}), Ok(None));
        assert!(std::fs::symlink_metadata(&bare.dest).is_err());
    }

    #[test]
    fn a_failed_download_says_where_to_get_it_and_keeps_what_was_there() {
        let t = Temp::new("fail");
        let i = installer(&t.0, Vec::new());
        let old = t.0.join("old");
        lay_out(&old, &want());
        i.install(&Source::Dir(old), &mut |_| {}).expect("install");
        let url = format!("file://{}/missing.zip", t.0.display());
        let e = i
            .install(&Source::Download(url.clone()), &mut |_| {})
            .expect_err("missing");
        assert!(e.contains(&url) && e.contains(URL_ENV), "{e}");
        assert_eq!(i.check(&i.dest), State::Ready(want()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_archive_downloads_with_progress_and_is_checked_against_its_sum() {
        let t = Temp::new("zip");
        let i = installer(&t.0, Vec::new());
        let src = t.0.join("src");
        lay_out(&src, &want());
        let zip = t.0.join("served/dreamer-web.zip");
        std::fs::create_dir_all(zip.parent().expect("parent")).expect("served");
        zip_of(&src, &zip);
        let size = std::fs::metadata(&zip).expect("zip").len();

        let url = format!("file://{}", zip.display());
        let mut steps = Vec::new();
        let done = i
            .install(&Source::Download(url.clone()), &mut |s| steps.push(s))
            .expect("download");
        assert_eq!(done.origin, Origin::Downloaded(url.clone()));
        assert_eq!(i.check(&i.dest), State::Ready(want()));
        assert!(steps.contains(&Step::Downloading {
            got: size,
            total: Some(size)
        }));
        assert_eq!(steps.last(), Some(&Step::Installing));
        assert_eq!(
            Step::Downloading {
                got: 42_000_000,
                total: Some(150_000_000)
            }
            .to_string(),
            "Downloading Dreamer Web: 42 of 150 MB"
        );

        // A zip that does not match its sum is refused; so is one given as
        // a file.
        std::fs::write(format!("{}.sha256", zip.display()), "00ff  x.zip\n").expect("sum");
        let e = i
            .install(&Source::Download(url), &mut |_| {})
            .expect_err("sum");
        assert!(e.contains("SHA-256"), "{e}");
        let e = i
            .install(&Source::Archive(zip.clone()), &mut |_| {})
            .expect_err("sum");
        assert!(e.contains("SHA-256"), "{e}");
        std::fs::remove_file(format!("{}.sha256", zip.display())).expect("rm");
        let done = i
            .install(&Source::Archive(zip.clone()), &mut |_| {})
            .expect("zip");
        assert_eq!(done.origin, Origin::Archive(zip));
    }

    #[test]
    fn one_install_at_a_time() {
        let t = Temp::new("lock");
        let held = lock(&t.0).expect("lock");
        assert!(lock(&t.0).expect_err("held").contains("another install"));
        drop(held);
        lock(&t.0).expect("free again");
    }

    #[test]
    fn an_argument_is_an_address_a_directory_or_an_archive() {
        let t = "aarch64-apple-darwin";
        assert_eq!(
            Source::from_arg("https://h/d/", t),
            Source::Download(format!("https://h/d/{}", archive_name(VERSION, t)))
        );
        let dir = std::env::temp_dir();
        assert_eq!(
            Source::from_arg(&dir.display().to_string(), t),
            Source::Dir(dir.clone())
        );
        assert_eq!(
            Source::from_arg("x.zip", t),
            Source::Archive(PathBuf::from("x.zip"))
        );
    }
}
