//! Where Dreamer keeps per-user runtime and state files, and the checks
//! that keep them private (spec 11.3: owner-restricted runtime directories
//! and sockets).
//!
//! | Directory | Default | Override |
//! | --- | --- | --- |
//! | runtime (socket, lock) | `$XDG_RUNTIME_DIR/dreamer`, else `$TMPDIR/dreamer-<uid>`, else `/tmp/dreamer-<uid>` | `DREAMER_RUNTIME_DIR` |
//! | state (log, action ledger) | `$XDG_STATE_HOME/dreamer`, else `~/.local/state/dreamer` | `DREAMER_STATE_DIR` |

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn runtime_dir() -> PathBuf {
    if let Some(d) = env_dir("DREAMER_RUNTIME_DIR") {
        return d;
    }
    if let Some(d) = env_dir("XDG_RUNTIME_DIR") {
        return d.join("dreamer");
    }
    let base = env_dir("TMPDIR").unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("dreamer-{}", uid()))
}

pub fn state_dir() -> PathBuf {
    if let Some(d) = env_dir("DREAMER_STATE_DIR") {
        return d;
    }
    if let Some(d) = env_dir("XDG_STATE_HOME") {
        return d.join("dreamer");
    }
    env_dir("HOME")
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".local/state/dreamer")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("dreamerd.sock")
}

/// The JSON-RPC endpoint, beside the native socket.
pub fn rpc_socket_path() -> PathBuf {
    runtime_dir().join("dreamerd.rpc.sock")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("dreamerd.lock")
}

pub fn log_path() -> PathBuf {
    state_dir().join("dreamerd.log")
}

pub fn ledger_path() -> PathBuf {
    state_dir().join("actions.jsonl")
}

/// Create `dir` (mode 0700) or check that the existing one belongs to this
/// user and is closed to everyone else. Never loosens or takes over a
/// directory it does not own.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if meta.uid() != uid() {
        return Err(format!(
            "{} belongs to uid {}, not this user ({}); refusing to use it",
            dir.display(),
            meta.uid(),
            uid()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to other users (mode {mode:o}); it must be 0700",
            dir.display()
        ));
    }
    Ok(())
}

/// Restrict a file (the socket, the log) to its owner.
pub fn owner_only(path: &Path) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Detach from the launching terminal: a new session has no controlling
/// terminal, so closing the terminal or SSH session sends it no hangup.
pub fn detach_session() -> Result<(), String> {
    rustix::process::setsid()
        .map(drop)
        .map_err(|e| format!("setsid: {e}"))
}

/// Whether a process with this pid exists.
pub fn process_alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid as i32)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Ask a process to stop (SIGTERM).
pub fn terminate(pid: u32) -> Result<(), String> {
    let p = rustix::process::Pid::from_raw(pid as i32).ok_or("invalid pid")?;
    rustix::process::kill_process(p, rustix::process::Signal::TERM)
        .map_err(|e| format!("kill {pid}: {e}"))
}

/// Signal a whole process group: SIGTERM, or SIGKILL with `force`.
pub fn terminate_group(pgid: u32, force: bool) -> Result<(), String> {
    let p = rustix::process::Pid::from_raw(pgid as i32).ok_or("invalid process group")?;
    let signal = if force {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    rustix::process::kill_process_group(p, signal).map_err(|e| format!("kill -{pgid}: {e}"))
}

/// The pid recorded in the daemon's startup lock, if any.
pub fn lock_holder() -> Option<u32> {
    std::fs::read_to_string(lock_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The session a process belongs to (diagnostics and tests).
pub fn session_of(pid: u32) -> Option<u32> {
    let p = rustix::process::Pid::from_raw(pid as i32)?;
    rustix::process::getsid(Some(p))
        .ok()
        .map(|s| s.as_raw_nonzero().get() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dirs_are_created_closed_and_open_ones_refused() {
        let base = std::env::temp_dir().join(format!("dreamer-platform-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("rt");
        private_dir(&dir).expect("create");
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        private_dir(&dir).expect("reuse");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = private_dir(&dir).expect_err("open dir refused");
        assert!(err.contains("must be 0700"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn this_process_is_alive_and_has_a_session() {
        assert!(process_alive(std::process::id()));
        assert!(session_of(std::process::id()).is_some());
    }
}
