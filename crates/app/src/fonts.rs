//! Terminal font: the configured family (default JetBrains Mono Nerd Font,
//! Mono variant), loaded from the files Dreamer ships, with an honest
//! fallback when it is not available.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use gpui::App;
use workspace_config::Settings;

/// Where shipped font files are looked for, in order.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("DREAMER_FONT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        // macOS bundle: Contents/MacOS/dreamer -> Contents/Resources/Fonts.
        dirs.push(exe_dir.join("../Resources/Fonts"));
        // Generic Linux install: <prefix>/bin/dreamer -> <prefix>/share/dreamer/fonts.
        dirs.push(exe_dir.join("../share/dreamer/fonts"));
    }
    // Development builds: the pinned copy `cargo xtask deps prepare` fetched.
    let cache = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    if let Some(versions) = cache.map(|c| c.join("dreamer/deps/fonts/nerdfonts"))
        && let Ok(entries) = std::fs::read_dir(versions)
    {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        dirs.extend(found.into_iter().rev());
    }
    dirs
}

/// The family to paint with, and where it came from (for diagnostics).
#[derive(Debug, Clone)]
pub struct ChosenFont {
    pub family: String,
    pub source: String,
}

/// Load shipped fonts, then pick the configured family if any source has
/// it (shipped files or the system), else the platform's monospace font.
pub fn choose(cx: &mut App, settings: &Settings) -> ChosenFont {
    let wanted = settings.font.family.clone();
    let on_system = cx.text_system().all_font_names().contains(&wanted);
    let mut loaded_from = None;
    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let fonts: Vec<Cow<'static, [u8]>> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ttf" || x == "otf"))
            .filter_map(|p| std::fs::read(p).ok())
            .map(Cow::Owned)
            .collect();
        if !fonts.is_empty() && cx.text_system().add_fonts(fonts).is_ok() {
            loaded_from = Some(dir);
            break;
        }
    }
    let names = cx.text_system().all_font_names();
    if names.contains(&wanted) {
        let source = match loaded_from {
            Some(dir) if !on_system => format!("loaded from {}", dir.display()),
            _ => "installed on the system".into(),
        };
        return ChosenFont {
            family: wanted,
            source,
        };
    }
    let fallback = if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    };
    let family = if names.iter().any(|n| n == fallback) {
        fallback.to_owned()
    } else {
        "monospace".to_owned()
    };
    eprintln!("dreamer: font `{wanted}` is not available; using `{family}`");
    ChosenFont {
        family,
        source: format!("fallback: `{wanted}` not found"),
    }
}
