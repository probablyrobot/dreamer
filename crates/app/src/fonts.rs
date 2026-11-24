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
type Read = Option<(PathBuf, Vec<Cow<'static, [u8]>>)>;

/// Font files being read on another thread since launch, for a family.
static PREFETCH: std::sync::Mutex<Option<(String, std::thread::JoinHandle<Read>)>> =
    std::sync::Mutex::new(None);

/// Start reading `family`'s shipped files now, off the main thread, so
/// that is done by the time the text system takes them (launch overlaps it
/// with GPUI's own startup).
pub fn prefetch(family: &str) {
    let wanted = family.to_owned();
    let for_thread = wanted.clone();
    if let Ok(handle) = std::thread::Builder::new()
        .name("dreamer-fonts".into())
        .spawn(move || read_family(&for_thread))
        && let Ok(mut p) = PREFETCH.lock()
    {
        *p = Some((wanted, handle));
    }
}

/// The first shipped font directory that has fonts, and the files of
/// `wanted` in it: by name (`JetBrainsMono Nerd Font Mono` is
/// `JetBrainsMonoNerdFontMono-*.ttf`), since the shipped families are
/// 46 MB and parsing them all cost launch about 40 ms; every file when none
/// matches, so a family named otherwise is still found.
fn read_family(wanted: &str) -> Read {
    let stem: String = wanted.chars().filter(|c| !c.is_whitespace()).collect();
    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ttf" || x == "otf"))
            .collect();
        if files.is_empty() {
            continue;
        }
        let matching: Vec<&PathBuf> = files
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{stem}-")))
            })
            .collect();
        let chosen: Vec<&PathBuf> = if matching.is_empty() {
            files.iter().collect()
        } else {
            matching
        };
        let fonts: Vec<Cow<'static, [u8]>> = chosen
            .into_iter()
            .filter_map(|p| std::fs::read(p).ok())
            .map(Cow::Owned)
            .collect();
        if !fonts.is_empty() {
            return Some((dir, fonts));
        }
    }
    None
}

/// Whether `family` resolves to itself, not to a fallback. One family
/// lookup: listing every installed font (`all_font_names`) takes over half
/// a second on macOS, and launch has 150 ms.
fn available(cx: &App, family: &str) -> bool {
    let ts = cx.text_system();
    let id = ts.resolve_font(&gpui::font(family.to_owned()));
    ts.get_font_for_id(id)
        .is_some_and(|f| f.family.as_ref() == family)
}

pub fn choose(cx: &mut App, settings: &Settings) -> ChosenFont {
    let wanted = settings.font.family.clone();
    let on_system = available(cx, &wanted);
    let mut loaded_from = None;
    let prefetched = PREFETCH
        .lock()
        .ok()
        .and_then(|mut p| p.take())
        .filter(|(family, _)| *family == wanted)
        .and_then(|(_, handle)| handle.join().ok());
    let found = prefetched.unwrap_or_else(|| read_family(&wanted));
    if let Some((dir, fonts)) = found
        && cx.text_system().add_fonts(fonts).is_ok()
    {
        loaded_from = Some(dir);
    }
    if on_system || available(cx, &wanted) {
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
    let family = if available(cx, fallback) {
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
