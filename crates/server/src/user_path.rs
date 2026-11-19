//! The `PATH` the daemon runs with.
//!
//! Started from Finder or the Dock, the daemon inherits launchd's `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), which hides Homebrew's tmux and the
//! CLIs providers and jobs run by name. When the inherited `PATH` has
//! nothing beyond those system directories, the user's login shell is asked
//! for its `PATH` once; if it does not answer in time, the usual places
//! that exist are appended. Only `PATH` is read from the shell.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// launchd's default `PATH` for applications.
const SYSTEM_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
/// Appended, when they exist, if the login shell gives no answer.
const USUAL_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
/// The same, under the home directory.
const HOME_DIRS: [&str; 2] = [".local/bin", ".cargo/bin"];

/// How long the login shell may take.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

const BEGIN: &str = "__DREAMER_PATH_BEGIN__";
const END: &str = "__DREAMER_PATH_END__";

/// Whether `path` has nothing beyond launchd's system directories.
fn lacks_user_dirs(path: Option<&OsStr>) -> bool {
    path.is_none_or(|p| {
        std::env::split_paths(p)
            .all(|d| d.as_os_str().is_empty() || SYSTEM_DIRS.iter().any(|s| d == Path::new(s)))
    })
}

/// The `PATH` to run with instead of `inherited`, or `None` to keep it.
///
/// A result never lacks the user's directories unless it equals what was
/// inherited, so a process restarted with it does not change it again.
pub fn effective(
    inherited: Option<&OsStr>,
    shell: Option<&OsStr>,
    home: Option<&Path>,
    timeout: Duration,
) -> Option<OsString> {
    if !lacks_user_dirs(inherited) {
        return None;
    }
    if let Some(p) = shell
        .filter(|s| !s.is_empty())
        .and_then(|s| from_login_shell(s, timeout))
        .filter(|p| !lacks_user_dirs(Some(p)))
    {
        return Some(p);
    }
    let mut dirs: Vec<PathBuf> = inherited
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| !d.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if dirs.is_empty() {
        dirs = SYSTEM_DIRS.iter().map(PathBuf::from).collect();
    }
    let usual = USUAL_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(home.into_iter().flat_map(|h| HOME_DIRS.map(|d| h.join(d))));
    for d in usual {
        if d.is_dir() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    let joined = std::env::join_paths(dirs).ok()?;
    (Some(joined.as_os_str()) != inherited).then_some(joined)
}

/// `PATH` as the user's login shell sets it, or `None` if the shell fails
/// or takes longer than `timeout`. Its standard input is empty, its errors
/// are discarded, and output its profile prints around the answer is
/// ignored.
fn from_login_shell(shell: &OsStr, timeout: Duration) -> Option<OsString> {
    let script = format!("printf '%s%s%s' '{BEGIN}' \"$PATH\" '{END}'");
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut cmd)?;
    let mut stdout = child.stdout.take()?;
    // Read on a thread: something the profile started in the background
    // may hold the pipe open after the shell exits, so the answer is taken
    // as soon as its end marker arrives rather than at end of file.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            out.extend_from_slice(&buf[..n]);
            if let Some(p) = between_markers(&out) {
                let _ = tx.send(p);
                return;
            }
        }
    });
    let answer = rx.recv_timeout(timeout).ok();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
    }
    let _ = child.wait();
    answer.filter(|p| !p.is_empty())
}

fn between_markers(out: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt as _;
    let end = find(out, END.as_bytes())?;
    let begin = rfind(&out[..end], BEGIN.as_bytes())? + BEGIN.len();
    Some(OsStr::from_bytes(&out[begin..end]).to_owned())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// Start `cmd`, retrying while the program is busy being written
/// (`ETXTBSY`, seen with stand-ins written by another thread just before).
fn spawn(cmd: &mut Command) -> Option<std::process::Child> {
    const ETXTBSY: i32 = 26;
    for _ in 0..5 {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            r => return r.ok(),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dreamer-path-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn a_path_with_the_users_dirs_is_kept_without_asking_the_shell() {
        let shell = OsStr::new("/nonexistent/shell");
        for p in ["/opt/homebrew/bin:/usr/bin:/bin", "/home/u/bin"] {
            assert_eq!(
                effective(Some(OsStr::new(p)), Some(shell), None, SHELL_TIMEOUT),
                None
            );
        }
    }

    #[test]
    fn launchds_path_takes_the_login_shells_path() {
        let d = temp();
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let shell = d.join("shell");
        // Profile noise on standard output and errors are ignored, and the
        // shell is asked for a login shell's PATH.
        script(
            &shell,
            &format!(
                "echo 'Welcome' ; echo oops >&2\n\
                 [ \"$1\" = -l ] && [ \"$2\" = -c ] || exit 1\n\
                 PATH='{}:/usr/bin:/bin' exec /bin/sh -c \"$3\"",
                bin.display()
            ),
        );
        let p = effective(
            Some(OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")),
            Some(shell.as_os_str()),
            Some(&d),
            SHELL_TIMEOUT,
        )
        .expect("a new PATH");
        assert_eq!(
            p,
            OsString::from(format!("{}:/usr/bin:/bin", bin.display()))
        );
        // Restarted with it, the daemon keeps it.
        assert_eq!(
            effective(Some(&p), Some(shell.as_os_str()), Some(&d), SHELL_TIMEOUT),
            None
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shell_that_hangs_falls_back_to_the_usual_dirs_in_time() {
        let d = temp();
        let local = d.join(".local/bin");
        std::fs::create_dir_all(&local).expect("dir");
        let shell = d.join("shell");
        script(&shell, "exec sleep 30");
        let started = Instant::now();
        let timeout = Duration::from_millis(500);
        let p = effective(
            Some(OsStr::new("/usr/bin:/bin")),
            Some(shell.as_os_str()),
            Some(&d),
            timeout,
        )
        .expect("a new PATH");
        assert!(started.elapsed() < timeout + Duration::from_secs(2));
        let dirs: Vec<PathBuf> = std::env::split_paths(&p).collect();
        assert_eq!(
            dirs[..2],
            [PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        assert!(dirs.contains(&local), "{p:?}");
        assert!(!dirs.contains(&d.join(".cargo/bin")), "{p:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failing_shell_falls_back_and_nothing_to_add_keeps_the_path() {
        let d = temp();
        let shell = d.join("shell");
        script(&shell, "exit 1");
        let got = effective(
            Some(OsStr::new("/usr/bin:/bin")),
            Some(shell.as_os_str()),
            Some(&d),
            SHELL_TIMEOUT,
        );
        // Only the system-wide usual dirs can be added; the home has none.
        let extra: Vec<&str> = USUAL_DIRS
            .into_iter()
            .filter(|p| Path::new(p).is_dir())
            .collect();
        if extra.is_empty() {
            assert_eq!(got, None);
        } else {
            let want = format!("/usr/bin:/bin:{}", extra.join(":"));
            assert_eq!(got, Some(OsString::from(want)));
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
