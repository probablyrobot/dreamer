//! Choosing a theme from the palette. The highlighted theme is previewed
//! everywhere: the chrome at once, and the terminals by writing it to
//! `dreamer.yaml` (after a short pause, so running down the list does not
//! rewrite the file for every row), which the daemon applies to every pane
//! as it does any change to the file. Enter keeps the theme; Escape puts the
//! file back exactly as it was.
//!
//! Writes happen off the UI thread, one at a time, and a write that a later
//! choice has replaced is skipped, so the file always ends as the last
//! choice left it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{App, AsyncApp, Task};
use terminal_ghostty::core::schemes::Scheme;

/// The latest choice; a write for an older one is skipped.
static LATEST: AtomicU64 = AtomicU64::new(0);
/// The file as it was before the first preview, while previewing (`None`
/// inside: there was no file). Holding the lock is holding the file.
static BEFORE: Mutex<Option<Option<String>>> = Mutex::new(None);

/// How long the highlight rests on a theme before the terminals show it.
const SETTLE: Duration = Duration::from_millis(250);

/// Preview `scheme`. Keep the returned task: dropping it, or the next
/// preview, keeps its terminals' part from happening.
pub(crate) fn preview(cx: &mut App, scheme: &'static Scheme) -> Task<()> {
    crate::live::preview(cx, Some(scheme));
    let at = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor().timer(SETTLE).await;
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = write(at, Some(scheme.name), false) {
                    eprintln!("dreamer: previewing {}: {e}", scheme.title);
                }
            })
            .await;
    })
}

/// Keep `scheme`: written to the file now, and in force here at once.
pub(crate) fn keep(cx: &mut App, scheme: &'static Scheme) {
    crate::live::preview(cx, Some(scheme));
    let at = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    cx.spawn(async move |cx: &mut AsyncApp| {
        let kept = cx
            .background_executor()
            .spawn(async move {
                let kept = write(at, Some(scheme.name), true);
                if matches!(kept, Ok(Some(_))) {
                    autosave(scheme.title);
                }
                kept
            })
            .await;
        cx.update(|cx| match kept {
            Ok(Some(settings)) => {
                let themes = crate::live::with(|l| l.themes.clone()).unwrap_or_default();
                crate::live::apply(cx, &settings, themes);
            }
            Ok(None) => crate::live::preview(cx, None),
            Err(e) => {
                crate::live::preview(cx, None);
                crate::live::refuse(cx, format!("{} was not kept: {e}", scheme.title));
            }
        });
    })
    .detach();
}

/// Stop previewing: the chrome as it was, and the file put back.
pub(crate) fn cancel(cx: &mut App) {
    crate::live::preview(cx, None);
    let at = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = write(at, None, true) {
                eprintln!("dreamer: putting the settings back: {e}");
            }
        })
        .detach();
}

/// Write the choice `at` (a theme, or `None` to put the file back), unless
/// a later one came first. `last` ends the preview.
fn write(
    at: u64,
    theme: Option<&str>,
    last: bool,
) -> Result<Option<workspace_config::Settings>, String> {
    let mut before = BEFORE.lock().map_err(|_| "poisoned".to_owned())?;
    if LATEST.load(Ordering::SeqCst) != at {
        return Ok(None);
    }
    let path = workspace_config::settings_path().ok_or("no configuration directory")?;
    let out = match theme {
        Some(name) => {
            if before.is_none() && !last {
                *before = Some(match std::fs::read_to_string(&path) {
                    Ok(t) => Some(t),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                });
            }
            workspace_config::edit::set_theme(&path, name).map(Some)
        }
        None => match before.as_ref() {
            Some(was) => workspace_config::edit::restore(&path, was.as_deref()).map(|()| None),
            None => Ok(None),
        },
    };
    if last {
        *before = None;
    }
    out
}

/// Save the environment after keeping a theme (`env.autosave`), on the
/// background thread that wrote it. A failure is reported, never shown as
/// the theme failing.
fn autosave(title: &str) {
    let Some(dir) = workspace_config::config_dir() else {
        return;
    };
    if let Err(e) =
        environment::Environment::new(&dir).autosave(&["dreamer.yaml"], &format!("Theme {title}"))
    {
        eprintln!("dreamer: the environment was not saved: {e}");
    }
}
