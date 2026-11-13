//! The desktop's local arrangement (spec 14.3): which session a window
//! shows, whether its sidebar is open, and where the window was. Client-only
//! state, kept apart from preferences (`dreamer.yaml`) and from anything on
//! the host. Written by the app; not meant to be edited.
//!
//! tmux session ids are only meaningful within one run of a tmux server
//! (its incarnation). A saved id is reused only for the same incarnation;
//! otherwise the session is found again by name, never by a stale id.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bump when the file's meaning changes; other versions are ignored.
pub const DESKTOP_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesktopState {
    pub version: u32,
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

/// The saved state, if there is a readable one of this version.
pub fn load_desktop_state(path: &Path) -> Option<DesktopState> {
    let text = std::fs::read_to_string(path).ok()?;
    let s: DesktopState = serde_norway::from_str(&text).ok()?;
    (s.version == DESKTOP_STATE_VERSION).then_some(s)
}

/// Write the state atomically (temporary file, then rename).
pub fn save_desktop_state(path: &Path, state: &DesktopState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut s = state.clone();
    s.version = DESKTOP_STATE_VERSION;
    let text = serde_norway::to_string(&s).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
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
            version: DESKTOP_STATE_VERSION,
            endpoint: "local".into(),
            incarnation,
            session_id: Some(id),
            session_name: Some(name.into()),
            ..DesktopState::default()
        }
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
    fn the_state_round_trips_and_other_versions_are_ignored() {
        let dir = std::env::temp_dir().join(format!("dreamer-state-{}", std::process::id()));
        let path = dir.join("desktop.yaml");
        let mut s = saved(3, 1, "logs");
        s.sidebar = true;
        s.bounds = Some([10.0, 20.0, 800.0, 500.0]);
        save_desktop_state(&path, &s).expect("save");
        assert_eq!(load_desktop_state(&path), Some(s.clone()));
        std::fs::write(
            &path,
            "version: 99\nendpoint: local\nincarnation: 1\nsidebar: false\n",
        )
        .expect("write");
        assert_eq!(load_desktop_state(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
