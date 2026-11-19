//! The settings in force in this desktop, replaced live when the
//! configuration changes (`dreamer.yaml`, or a theme file beside it): the
//! terminal font, the chrome's theme, which links open, clipboard writes,
//! the fallback terminal, the windows' decorations, and how Dreamer Web is
//! offered. Every window
//! repaints with the new ones. A file that does not read changes nothing;
//! its error is kept here for the tab strip to show until a valid file
//! replaces it.
//!
//! The terminals' own colors and cursor are the daemon's (it applies the
//! same file to its mirrors), so terminals and chrome change together.

use std::sync::RwLock;
use std::time::Duration;

use gpui::App;
use workspace_config::{ClipboardWrite, LinkSettings, Settings};

use terminal_ghostty::core::schemes::Scheme;

use crate::TermFont;
use crate::theme::Theme;

/// Everything here that the configuration decides.
#[derive(Debug, Clone)]
pub(crate) struct Live {
    pub font: TermFont,
    /// The font size the configuration gives; `font.size` differs from it
    /// after the text was made bigger or smaller.
    pub font_base: f32,
    pub theme: Theme,
    pub links: LinkSettings,
    pub clipboard: ClipboardWrite,
    /// tmux buffers and the clipboard, copy on select, middle click.
    pub clipboard_tmux: workspace_config::ClipboardTmux,
    pub copy_on_select: bool,
    pub middle_click: workspace_config::MiddleClick,
    pub fallback: Vec<String>,
    /// The system's title bar and window buttons, or none.
    pub decorations: workspace_config::Decorations,
    /// Dreamer Web: whether it is offered, and where it downloads from.
    pub web: workspace_config::WebSettings,
    /// The scheme in force, and every theme there is to choose from.
    pub scheme: &'static Scheme,
    pub themes: Vec<&'static Scheme>,
    /// The chrome of a theme being previewed, drawn instead of `theme`.
    pub preview: Option<Theme>,
    /// Why the last change to the configuration was not applied.
    pub refused: Option<String>,
}

static LIVE: RwLock<Option<Live>> = RwLock::new(None);

/// Read the settings in force, if they were set.
pub(crate) fn with<R>(f: impl FnOnce(&Live) -> R) -> Option<R> {
    LIVE.read().ok().and_then(|l| l.as_ref().map(f))
}

/// Every theme: the user's files in `themes/`, then the built-ins. Reads
/// files, so not on the UI thread once windows are open.
pub(crate) fn themes() -> Vec<&'static Scheme> {
    let dir = workspace_config::themes::themes_dir();
    workspace_config::themes::all(dir.as_deref())
        .0
        .into_iter()
        .map(|l| l.scheme)
        .collect()
}

/// The theme list (for the palette's Theme command), once it is read: at
/// launch it is read in the background, after the window opens.
pub(crate) fn set_themes(themes: Vec<&'static Scheme>) {
    if let Ok(mut l) = LIVE.write()
        && let Some(l) = l.as_mut()
    {
        l.themes = themes;
    }
}

/// Draw the chrome in `scheme` until this is called with `None`; the
/// terminals follow when the file says so.
pub(crate) fn preview(cx: &mut App, scheme: Option<&'static Scheme>) {
    if let Ok(mut l) = LIVE.write()
        && let Some(l) = l.as_mut()
    {
        l.preview = scheme.map(crate::theme::for_scheme);
    }
    cx.refresh_windows();
}

/// Make `settings` the ones in force, and repaint every window.
pub(crate) fn apply(cx: &mut App, settings: &Settings, themes: Vec<&'static Scheme>) {
    let chosen = crate::fonts::choose(cx, settings);
    // Text made bigger or smaller stays so by as much on the new size.
    let step = with(|l| l.font.size - l.font_base).unwrap_or(0.0);
    let decorations = settings.window.decorations;
    let redecorate = with(|l| l.decorations != decorations).unwrap_or(false);
    let live = Live {
        font_base: settings.font.size,
        font: TermFont {
            family: chosen.family,
            size: clamp_font(settings.font.size + step),
            line_height: settings.font.line_height,
            letter_spacing: settings.font.letter_spacing,
            source: chosen.source,
        },
        // The same scheme the daemon gives the terminals: the chrome is
        // drawn from it, so the window is one theme.
        theme: crate::theme::for_scheme(settings.scheme()),
        links: settings.links.clone(),
        clipboard: settings.clipboard.write,
        clipboard_tmux: settings.clipboard.tmux,
        copy_on_select: settings.clipboard.copy_on_select,
        middle_click: settings.clipboard.middle_click,
        fallback: settings.fallback.launch.clone(),
        decorations,
        web: settings.web.clone(),
        scheme: settings.scheme(),
        themes,
        preview: None,
        refused: None,
    };
    if let Ok(mut l) = LIVE.write() {
        *l = Some(live);
    }
    // `web.component` may have changed what the daemon is told.
    crate::webcomp::publish();
    if redecorate {
        for w in cx.windows() {
            if let Some(w) = w.downcast::<crate::workspace::Workspace>() {
                let _ = w.update(cx, |_, window, _| {
                    crate::workspace::decorate(window, decorations);
                });
            }
        }
    }
    cx.refresh_windows();
}

/// The sizes the settings allow.
fn clamp_font(size: f32) -> f32 {
    size.clamp(6.0, 72.0)
}

/// The terminal text `by` points bigger (or smaller), or with `None` back
/// to the configured size, in every window. The file is not changed.
pub(crate) fn font_step(cx: &mut App, by: Option<f32>) {
    adjust(|l| {
        l.font.size = match by {
            Some(by) => clamp_font(l.font.size + by),
            None => l.font_base,
        }
    });
    cx.refresh_windows();
}

/// Change the settings in force directly, with no file (smoke runs).
pub(crate) fn adjust(f: impl FnOnce(&mut Live)) {
    if let Ok(mut l) = LIVE.write()
        && let Some(l) = l.as_mut()
    {
        f(l);
    }
}

/// Keep the settings in force, and say why the change was not applied.
pub(crate) fn refuse(cx: &mut App, why: String) {
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
                .spawn(async move {
                    workspace_config::load_settings()
                        .map(|s| (s, themes()))
                        .map_err(|e| e.to_string())
                })
                .await;
            cx.update(|cx| match read {
                Ok((s, themes)) => {
                    // A preview in progress stays on top until it ends.
                    let preview = with(|l| l.preview).flatten();
                    apply(cx, &s, themes);
                    if let Ok(mut l) = LIVE.write()
                        && let Some(l) = l.as_mut()
                    {
                        l.preview = preview;
                    }
                }
                Err(why) => refuse(cx, why),
            });
        }
    })
    .detach();
}
