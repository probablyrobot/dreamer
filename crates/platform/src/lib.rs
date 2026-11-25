//! Where Dreamer keeps per-user runtime and state files, and the checks
//! that keep them private (owner-restricted runtime directories
//! and sockets).
//!
//! | Directory | Default | Override |
//! | --- | --- | --- |
//! | runtime (socket, lock) | `$XDG_RUNTIME_DIR/dreamer`, else `$TMPDIR/dreamer-<uid>`, else `/tmp/dreamer-<uid>` | `DREAMER_RUNTIME_DIR` |
//! | state (log, action ledger) | `$XDG_STATE_HOME/dreamer`, else `~/.local/state/dreamer` | `DREAMER_STATE_DIR` |
//! | data (managed agent installs, Dreamer Web) | `$XDG_DATA_HOME/dreamer`, else `~/.local/share/dreamer` | `DREAMER_DATA_DIR` |

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

/// Data Dreamer installs and keeps for the user, such as managed agent
/// harnesses and the Dreamer Web component. The same place on macOS and Linux.
pub fn data_dir() -> PathBuf {
    if let Some(d) = env_dir("DREAMER_DATA_DIR") {
        return d;
    }
    if let Some(d) = env_dir("XDG_DATA_HOME") {
        return d.join("dreamer");
    }
    env_dir("HOME")
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".local/share/dreamer")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("dreamerd.sock")
}

/// The JSON-RPC endpoint, beside the native socket.
pub fn rpc_socket_path() -> PathBuf {
    runtime_dir().join("dreamerd.rpc.sock")
}

/// The longest Unix socket path the system accepts, in bytes (the size of
/// `sun_path`, less its terminating NUL).
pub const SOCKET_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// Refuse a socket path the system could not bind, before trying, with a
/// reason a person can act on.
pub fn check_socket_path(p: &Path) -> Result<(), String> {
    let n = p.as_os_str().len();
    if n > SOCKET_PATH_MAX {
        return Err(format!(
            "the socket path {} is {n} bytes, longer than the {SOCKET_PATH_MAX} this system allows; \
             use a shorter DREAMER_RUNTIME_DIR",
            p.display()
        ));
    }
    Ok(())
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

/// Hand memory that the allocator holds but no longer uses back to the
/// system: glibc's `malloc_trim`, or macOS's malloc pressure relief. A
/// daemon that has just dropped many terminal mirrors calls this so its
/// resident size shrinks with them. Elsewhere it does nothing.
#[allow(unsafe_code)]
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: malloc_trim takes a plain size and only touches the
        // allocator's own free lists, under its own lock.
        unsafe {
            malloc_trim(0);
        }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means every zone and a goal of 0 means as
        // much as possible; the call only releases free pages.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

/// Bytes the allocator has handed out and not had back, without the freed
/// memory it keeps for reuse: what a process really holds, where resident
/// size also counts retained free pages. `None` where it cannot tell.
#[allow(unsafe_code)]
pub fn heap_in_use() -> Option<u64> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        #[repr(C)]
        struct Mallinfo2 {
            arena: usize,
            ordblks: usize,
            smblks: usize,
            hblks: usize,
            hblkhd: usize,
            usmblks: usize,
            fsmblks: usize,
            uordblks: usize,
            fordblks: usize,
            keepcost: usize,
        }
        unsafe extern "C" {
            fn mallinfo2() -> Mallinfo2;
        }
        // SAFETY: mallinfo2 (glibc 2.33+) takes nothing and returns a
        // plain struct by value.
        let m = unsafe { mallinfo2() };
        Some((m.uordblks + m.hblkhd) as u64)
    }
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        #[derive(Default)]
        struct MallocStatistics {
            blocks_in_use: u32,
            size_in_use: usize,
            max_size_in_use: usize,
            size_allocated: usize,
        }
        unsafe extern "C" {
            fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStatistics);
        }
        let mut s = MallocStatistics::default();
        // SAFETY: a null zone sums every zone into the struct we own.
        unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
        Some(s.size_in_use as u64)
    }
    #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
    {
        None
    }
}

/// A process's physical footprint on macOS, in bytes: the memory the
/// kernel charges it for (dirty and compressed pages, as Activity Monitor
/// shows), which leaves out free pages the allocator still has mapped but
/// that resident size counts. `None` elsewhere, or when the process is
/// gone or belongs to another user.
#[allow(unsafe_code)]
pub fn phys_footprint(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        /// `struct rusage_info_v2` from <sys/resource.h>, field for field.
        #[repr(C)]
        #[derive(Default)]
        struct RusageInfoV2 {
            uuid: [u8; 16],
            user_time: u64,
            system_time: u64,
            pkg_idle_wkups: u64,
            interrupt_wkups: u64,
            pageins: u64,
            wired_size: u64,
            resident_size: u64,
            phys_footprint: u64,
            /// From `ri_proc_start_abstime` to `ri_diskio_byteswritten`.
            rest: [u64; 10],
        }
        const RUSAGE_INFO_V2: std::ffi::c_int = 2;
        unsafe extern "C" {
            fn proc_pid_rusage(
                pid: std::ffi::c_int,
                flavor: std::ffi::c_int,
                buffer: *mut RusageInfoV2,
            ) -> std::ffi::c_int;
        }
        let pid = std::ffi::c_int::try_from(pid).ok()?;
        let mut info = RusageInfoV2::default();
        // SAFETY: for RUSAGE_INFO_V2 the kernel writes at most
        // `sizeof(struct rusage_info_v2)` bytes, which is the size of the
        // struct we own and pass.
        let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, &mut info) };
        (rc == 0).then_some(info.phys_footprint)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocator statistics are read through hand-written C structs; a
    /// wrong layout would read the wrong field. Other tests share the heap
    /// and allocate or free a little meanwhile, so the block is large and
    /// the bounds leave room for them.
    #[test]
    fn heap_in_use_follows_a_large_allocation() {
        const BLOCK: u64 = 128 << 20;
        const SLACK: u64 = 32 << 20;
        let Some(before) = heap_in_use() else {
            return;
        };
        let block = vec![1u8; BLOCK as usize];
        let during = heap_in_use().unwrap_or(0);
        assert!(
            during + SLACK >= before + BLOCK,
            "{before} then {during} with 128 MiB more held"
        );
        drop(block);
        release_free_memory();
        let after = heap_in_use().unwrap_or(u64::MAX);
        assert!(
            after + BLOCK <= during + SLACK,
            "{during} then {after} after freeing 128 MiB"
        );
    }

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

    /// The rusage struct is hand-written; a wrong layout would read another
    /// field. A block written page by page must show in the footprint. The
    /// heap test may hold its own 128 MiB meanwhile, hence the upper bound.
    #[test]
    fn phys_footprint_follows_a_written_block() {
        const BLOCK: u64 = 128 << 20;
        const SLACK: u64 = 32 << 20;
        let me = std::process::id();
        if !cfg!(target_os = "macos") {
            assert_eq!(phys_footprint(me), None);
            return;
        }
        let before = phys_footprint(me).expect("footprint of this process");
        let block = vec![1u8; BLOCK as usize];
        let during = phys_footprint(me).unwrap_or(0);
        assert!(
            during + SLACK >= before + BLOCK && during <= before + 2 * BLOCK + SLACK,
            "{before} then {during} with 128 MiB more written"
        );
        drop(block);
    }

    #[test]
    fn this_process_is_alive_and_has_a_session() {
        assert!(process_alive(std::process::id()));
        assert!(session_of(std::process::id()).is_some());
    }
}
