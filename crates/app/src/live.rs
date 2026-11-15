//! The settings in force in this desktop, replaced live when the
//! configuration changes (`dreamer.yaml`, or a theme file beside it): the
//! terminal font, the chrome's theme, which links open, clipboard writes,
//! and the fallback terminal. Every window repaints with the new ones. A
//! file that does not read changes nothing; its error is kept here for the
//! tab strip to show until a valid file replaces it.
//!
//! The terminals' own colors and cursor are the daemon's (it applies the
//! same file to its mirrors), so terminals and chrome change together.

use std::sync::RwLock;
use std::time::Duration;

use gpui::App;
use workspace_config::{ClipboardWrite, LinkSettings, Settings};

use crate::TermFont;
use crate::theme::Theme;

/// Everything here that the configuration decides.
#[derive(Debug, Clone)]
pub(crate) struct Live {
    pub font: TermFont,
    pub theme: Theme,
    pub links: LinkSettings,
    pub clipboard: ClipboardWrite,
    pub fallback: Vec<String>,
    /// Why the last change to the configuration was not applied.
    pub refused: Option<String>,
}

static LIVE: RwLock<Option<Live>> = RwLock::new(None);

/// Read the settings in force, if they were set.
pub(crate) fn with<R>(f: impl FnOnce(&Live) -> R) -> Option<R> {
    LIVE.read().ok().and_then(|l| l.as_ref().map(f))
}

/// Make `settings` the ones in force, and repaint every window.
pub(crate) fn apply(cx: &mut App, settings: &Settings) {
    let chosen = crate::fonts::choose(cx, settings);
    let live = Live {
        font: TermFont {
            family: chosen.family,
            size: settings.font.size,
            line_height: settings.font.line_height,
            letter_spacing: settings.font.letter_spacing,
            source: chosen.source,
        },
        // The same scheme the daemon gives the terminals: the chrome is
        // drawn from it, so the window is one theme.
        theme: crate::theme::for_scheme(settings.scheme()),
        links: settings.links.clone(),
        clipboard: settings.clipboard.write,
        fallback: settings.fallback.launch.clone(),
        refused: None,
    };
    if let Ok(mut l) = LIVE.write() {
        *l = Some(live);
    }
    cx.refresh_windows();
}

/// Keep the settings in force, and say why the change was not applied.
fn refuse(cx: &mut App, why: String) {
    if let Ok(mut l) = LIVE.write()
        && let Some(l) = l.as_mut()
    {
        l.refused = Some(why);
    }
    cx.refresh_windows();
}

/// Watch the configuration for this desktop's life: once a second, off the
/// UI thread, look at the files; on a change, read them there too, and
/// apply the result here.
pub(crate) fn watch(cx: &mut App) {
    let Some(dir) = workspace_config::config_dir() else {
        return;
    };
    cx.spawn(async move |cx| {
        let look = dir.clone();
        let mut seen = cx
            .background_executor()
            .spawn(async move { workspace_config::check::stamp(&look) })
            .await;
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let look = dir.clone();
            let now = cx
                .background_executor()
                .spawn(async move { workspace_config::check::stamp(&look) })
                .await;
            if now == seen {
                continue;
            }
            seen = now;
            let read = cx
                .background_executor()
                .spawn(async move { workspace_config::load_settings().map_err(|e| e.to_string()) })
                .await;
            cx.update(|cx| match read {
                Ok(s) => apply(cx, &s),
                Err(why) => refuse(cx, why),
            });
        }
    })
    .detach();
}
