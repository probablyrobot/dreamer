//! Where Dreamer's user configuration lives.
//!
//! `~/.config/dreamer` on macOS and Linux alike, so the same dotfiles work
//! on both. Order: `DREAMER_CONFIG_DIR`, then `$XDG_CONFIG_HOME/dreamer`,
//! then `$HOME/.config/dreamer`.

use std::path::{Path, PathBuf};

pub fn config_dir() -> Option<PathBuf> {
    resolve(
        std::env::var_os("DREAMER_CONFIG_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Some(dir);
    }
    // XDG requires an absolute path; a relative one is ignored.
    if let Some(xdg) = xdg.filter(|p| p.is_absolute()) {
        return Some(xdg.join("dreamer"));
    }
    home.filter(|p| !p.as_os_str().is_empty())
        .map(|h| h.join(".config").join("dreamer"))
}

/// `<config dir>/workspaces`.
pub fn workspace_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("workspaces"))
}

/// Resolve a workspace argument: an existing path is used as is; a bare
/// name means `<workspace dir>/<name>.yaml` (or `.yml`).
pub fn workspace_path(name_or_path: &str) -> Option<PathBuf> {
    let direct = Path::new(name_or_path);
    if direct.is_file() {
        return Some(direct.to_path_buf());
    }
    let dir = workspace_dir()?;
    ["yaml", "yml"]
        .iter()
        .map(|ext| dir.join(format!("{name_or_path}.{ext}")))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::path::PathBuf;

    #[test]
    fn explicit_then_xdg_then_home() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(resolve(p("/x"), p("/xdg"), p("/home/u")), p("/x"));
        assert_eq!(resolve(None, p("/xdg"), p("/home/u")), p("/xdg/dreamer"));
        assert_eq!(
            resolve(None, p("relative"), p("/home/u")),
            p("/home/u/.config/dreamer")
        );
        assert_eq!(
            resolve(None, None, p("/Users/u")),
            p("/Users/u/.config/dreamer")
        );
        assert_eq!(resolve(None, None, None), None);
    }
}
