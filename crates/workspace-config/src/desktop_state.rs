//! The desktop's local arrangement (spec 14.3): every open window, with
//! the session it shows, whether its sidebar is open, and where it was.
//! Client-only state, kept apart from preferences (`dreamer.yaml`) and from
//! anything on the host. Written by the app; not meant to be edited.
//!
//! tmux session ids are only meaningful within one run of a tmux server
//! (its incarnation). A saved id is reused only for the same incarnation;
//! otherwise the session is found again by name, never by a stale id.
//!
//! The file lists the windows (version 2). A file from before (version 1)
//! held one window and still loads as that one window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Bump when the file's meaning changes; other versions are ignored.
pub const DESKTOP_STATE_VERSION: u32 = 2;
/// The earlier file: one window, its fields at the top level.
const SINGLE_WINDOW_VERSION: u32 = 1;

/// One window's arrangement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopState {
    /// The endpoint, and the tmux server incarnation the ids belong to.
    pub endpoint: String,
    pub incarnation: u64,
    /// The session shown, by id (only valid for `incarnation`) and by name.
    pub session_id: Option<u32>,
    pub session_name: Option<String>,
    pub sidebar: bool,
    /// Window origin and size in logical pixels.
    pub bounds: Option<[f32; 4]>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DesktopFile {
    version: u32,
    #[serde(default)]
    windows: Vec<DesktopState>,
}

/// `$DREAMER_DESKTOP_STATE_DIR`, else `$XDG_STATE_HOME/dreamer`, else
/// `~/.local/state/dreamer`, on macOS and Linux alike.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DREAMER_DESKTOP_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if let Some(x) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Some(x.join("dreamer"));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/state/dreamer"))
}

pub fn desktop_state_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("desktop.yaml"))
}

/// The saved windows, in the order they were opened, if there is a
/// readable file of this version or of the single-window version.
pub fn load_desktop(path: &Path) -> Option<Vec<DesktopState>> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: DesktopFile = serde_norway::from_str(&text).ok()?;
    match file.version {
        DESKTOP_STATE_VERSION => Some(file.windows),
        SINGLE_WINDOW_VERSION => serde_norway::from_str(&text).ok().map(|w| vec![w]),
        _ => None,
    }
}

static TMP: AtomicU64 = AtomicU64::new(0);

/// Write the windows atomically (a temporary file of this writer's own,
/// then rename): a reader sees the old file or the new one, never a part.
pub fn save_desktop(path: &Path, windows: &[DesktopState]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = DesktopFile {
        version: DESKTOP_STATE_VERSION,
        windows: windows.to_vec(),
    };
    let text = serde_norway::to_string(&file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!(
        "yaml.{}-{}.tmp",
        std::process::id(),
        TMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The one writer of the file for this process: every window puts its own
/// entry here and asks for a save, and each save writes all of them, so
/// windows saving at the same time never drop each other's entries.
#[derive(Debug)]
pub struct DesktopStore {
    path: PathBuf,
    windows: Mutex<Windows>,
    /// Held while writing; the entries are read under it, so the last
    /// write to finish carries the newest of them.
    write: Mutex<()>,
}

#[derive(Debug, Default)]
struct Windows {
    /// Open windows by a key in opening order.
    open: BTreeMap<u64, DesktopState>,
    /// The last window closed, kept while no window is open, so the next
    /// launch opens it again rather than nothing.
    last_closed: Option<DesktopState>,
}

impl DesktopStore {
    pub fn new(path: PathBuf) -> DesktopStore {
        DesktopStore {
            path,
            windows: Mutex::new(Windows::default()),
            write: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record window `key`'s arrangement (not yet written).
    pub fn set(&self, key: u64, state: DesktopState) {
        if let Ok(mut w) = self.windows.lock() {
            w.open.insert(key, state);
            w.last_closed = None;
        }
    }

    /// Window `key` was closed: it is not opened again at the next launch,
    /// unless it was the last one open.
    pub fn forget(&self, key: u64) {
        if let Ok(mut w) = self.windows.lock()
            && let Some(s) = w.open.remove(&key)
            && w.open.is_empty()
        {
            w.last_closed = Some(s);
        }
    }

    /// What the file would hold now.
    pub fn windows(&self) -> Vec<DesktopState> {
        self.windows
            .lock()
            .map(|w| {
                if w.open.is_empty() {
                    w.last_closed.iter().cloned().collect()
                } else {
                    w.open.values().cloned().collect()
                }
            })
            .unwrap_or_default()
    }

    /// Write every window's entry. Blocks on the file: call it off the UI
    /// thread.
    pub fn save(&self) -> std::io::Result<()> {
        let _one = self
            .write
            .lock()
            .map_err(|_| std::io::Error::other("a save panicked"))?;
        save_desktop(&self.path, &self.windows())
    }
}

/// Which session to show, given the saved state and the server's sessions
/// as (id, name). The saved id only counts on the same endpoint and server
/// incarnation; after a restart the name decides; otherwise the first.
pub fn choose_session(
    saved: Option<&DesktopState>,
    endpoint: &str,
    incarnation: u64,
    sessions: &[(u32, &str)],
) -> Option<u32> {
    let first = sessions.first().map(|(id, _)| *id);
    let Some(s) = saved.filter(|s| s.endpoint == endpoint) else {
        return first;
    };
    let by_id = s
        .session_id
        .filter(|_| s.incarnation == incarnation)
        .filter(|id| sessions.iter().any(|(x, _)| x == id));
    let by_name = s
        .session_name
        .as_deref()
        .and_then(|n| sessions.iter().find(|(_, x)| *x == n).map(|(id, _)| *id));
    by_id.or(by_name).or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(incarnation: u64, id: u32, name: &str) -> DesktopState {
        DesktopState {
            endpoint: "local".into(),
            incarnation,
            session_id: Some(id),
            session_name: Some(name.into()),
            ..DesktopState::default()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dreamer-state-{name}-{}-{}",
            std::process::id(),
            TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_saved_id_counts_only_for_the_same_server_incarnation() {
        let sessions = [(0, "main"), (1, "logs"), (2, "work")];
        // Same server: the id decides, even if the name changed since.
        let s = saved(7, 2, "renamed-since");
        assert_eq!(choose_session(Some(&s), "local", 7, &sessions), Some(2));
        // Restarted server: ids mean nothing; the name finds the session.
        let s = saved(7, 2, "logs");
        assert_eq!(choose_session(Some(&s), "local", 8, &sessions), Some(1));
        // Restarted and no session of that name: the first, never id 2.
        let s = saved(7, 2, "gone");
        assert_eq!(choose_session(Some(&s), "local", 8, &sessions), Some(0));
        // Another endpoint's state is ignored.
        assert_eq!(choose_session(Some(&s), "remote", 7, &sessions), Some(0));
        assert_eq!(choose_session(None, "local", 7, &[]), None);
    }

    #[test]
    fn several_windows_round_trip_in_order_and_other_versions_are_ignored() {
        let dir = temp("windows");
        let path = dir.join("desktop.yaml");
        let mut a = saved(3, 1, "logs");
        a.sidebar = true;
        a.bounds = Some([10.0, 20.0, 800.0, 500.0]);
        let b = saved(3, 4, "work");
        save_desktop(&path, &[a.clone(), b.clone()]).expect("save");
        assert_eq!(load_desktop(&path), Some(vec![a, b]));
        std::fs::write(&path, "version: 99\nwindows: []\n").expect("write");
        assert_eq!(load_desktop(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_single_window_file_from_before_loads_as_one_window() {
        let dir = temp("single");
        let path = dir.join("desktop.yaml");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            &path,
            "version: 1\nendpoint: local\nincarnation: 3\nsession_id: 1\n\
             session_name: logs\nsidebar: true\nbounds: [10.0, 20.0, 800.0, 500.0]\n",
        )
        .expect("write");
        let mut want = saved(3, 1, "logs");
        want.sidebar = true;
        want.bounds = Some([10.0, 20.0, 800.0, 500.0]);
        assert_eq!(load_desktop(&path), Some(vec![want]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn windows_saving_at_once_keep_each_others_entries() {
        let dir = temp("concurrent");
        let store = std::sync::Arc::new(DesktopStore::new(dir.join("desktop.yaml")));
        let threads: Vec<_> = (0..8u32)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for round in 0..20u64 {
                        store.set(u64::from(i), saved(round, i, &format!("s{i}")));
                        store.save().expect("save");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("thread");
        }
        let loaded = load_desktop(store.path()).expect("load");
        let names: Vec<_> = loaded
            .iter()
            .map(|w| (w.session_name.clone().unwrap_or_default(), w.incarnation))
            .collect();
        let want: Vec<_> = (0..8).map(|i| (format!("s{i}"), 19)).collect();
        assert_eq!(names, want);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_closed_window_is_not_restored_unless_it_was_the_last() {
        let store = DesktopStore::new(PathBuf::from("/nonexistent/desktop.yaml"));
        store.set(1, saved(1, 1, "a"));
        store.set(2, saved(1, 2, "b"));
        store.forget(1);
        assert_eq!(store.windows(), vec![saved(1, 2, "b")]);
        store.forget(2);
        assert_eq!(store.windows(), vec![saved(1, 2, "b")]);
        store.set(3, saved(1, 3, "c"));
        assert_eq!(store.windows(), vec![saved(1, 3, "c")]);
    }
}
