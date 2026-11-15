//! Color schemes from files: the built-in ones (compiled into the program,
//! see `terminal_core::schemes`) and the user's own in
//! `<config dir>/themes/`. A user file is found by its name without the
//! extension (`.toml`, `.conf`, or none), in either format
//! `terminal_core::schemes::parse` reads: alacritty-theme TOML or a
//! Ghostty theme. A user theme with a built-in's name replaces it.

use std::path::{Path, PathBuf};

use terminal_core::schemes::{self, Scheme};

/// Where the user's theme files go: `<config dir>/themes`.
pub fn themes_dir() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("themes"))
}

/// Theme files in `dir`, by normalized name, sorted.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                && matches!(
                    p.extension().and_then(|x| x.to_str()),
                    None | Some("toml" | "conf")
                )
        })
        .filter_map(|p| {
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            Some((schemes::normalize(&stem), p))
        })
        .collect();
    out.sort();
    out
}

fn read(name: &str, path: &Path) -> Result<&'static Scheme, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let scheme = schemes::parse(name, &text).map_err(|e| format!("{}: {e}", path.display()))?;
    // Read once per process, and kept for its life like the built-ins.
    Ok(Box::leak(Box::new(scheme)))
}

/// The theme called `name`: the user's file in `dir` if there is one,
/// else the built-in. `Ok(None)` when neither exists; an unreadable or
/// incomplete user file is an error naming it.
pub fn find(name: &str, dir: Option<&Path>) -> Result<Option<&'static Scheme>, String> {
    let want = schemes::normalize(name);
    if let Some(dir) = dir
        && let Some((n, path)) = files(dir).into_iter().find(|(n, _)| *n == want)
    {
        return read(&n, &path).map(Some);
    }
    Ok(schemes::find(&want))
}

/// One theme as `dream themes` lists it.
#[derive(Debug)]
pub struct Listed {
    pub scheme: &'static Scheme,
    /// The user's file, for a theme from `themes/`.
    pub file: Option<PathBuf>,
}

/// Every theme: the user's (which replace built-ins of the same name),
/// then the built-ins. Files that do not read are returned apart, with why.
pub fn all(dir: Option<&Path>) -> (Vec<Listed>, Vec<String>) {
    let mut listed = Vec::new();
    let mut problems = Vec::new();
    if let Some(dir) = dir {
        for (name, path) in files(dir) {
            match read(&name, &path) {
                Ok(scheme) => listed.push(Listed {
                    scheme,
                    file: Some(path),
                }),
                Err(e) => problems.push(e),
            }
        }
    }
    for s in schemes::all() {
        if !listed.iter().any(|l| l.scheme.name == s.name) {
            listed.push(Listed {
                scheme: s,
                file: None,
            });
        }
    }
    (listed, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GHOSTTY: &str = "\
# A Ghostty theme
palette = 0=#1d1f21
palette = 1=#cc6666
palette = 2=#b5bd68
palette = 3=#f0c674
palette = 4=#81a2be
palette = 5=#b294bb
palette = 6=#8abeb7
palette = 7=#c5c8c6
palette = 8=#666666
palette = 9=#d54e53
palette = 10=#b9ca4a
palette = 11=#e7c547
palette = 12=#7aa6da
palette = 13=#c397d8
palette = 14=#70c0b1
palette = 15=#eaeaea
background = #1d1f21
foreground = #c5c8c6
cursor-color = #c5c8c6
selection-background = #373b41
";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("drm-themes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    #[test]
    fn a_users_ghostty_theme_is_found_by_name_and_replaces_a_built_in() {
        let d = dir("find");
        std::fs::write(d.join("Tomorrow Night"), GHOSTTY).expect("write");
        std::fs::write(
            d.join("nord.toml"),
            "[colors.primary]\nbackground = '#000000'\n",
        )
        .expect("write");
        std::fs::write(d.join("notes.md"), "not a theme").expect("write");
        let s = find("tomorrow-night", Some(&d))
            .expect("ok")
            .expect("found");
        assert_eq!(s.title, "Tomorrow Night");
        assert_eq!(s.ansi[4], schemes::rgb(0x81a2be));
        assert_eq!(s.selection, Some(schemes::rgb(0x373b41)));
        // A broken user file with a built-in's name is an error naming it,
        // not a silent fallback.
        let e = find("nord", Some(&d)).expect_err("incomplete");
        assert!(e.contains("nord.toml") && e.contains("foreground"), "{e}");
        // Built-ins without a user file of their name.
        assert_eq!(
            find("dreamer-light", Some(&d)).expect("ok").map(|s| s.name),
            Some("dreamer-light")
        );
        assert!(find("nope", Some(&d)).expect("ok").is_none());
        let (listed, problems) = all(Some(&d));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(listed.iter().any(|l| l.file.is_some()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
