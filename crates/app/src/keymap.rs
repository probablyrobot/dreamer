//! The desktop's commands, their shortcuts, and the macOS menu bar, all
//! from one table so a shortcut, its menu item, and its handler never
//! disagree.
//!
//! Shortcuts follow each platform's convention: Cmd on macOS, Ctrl+Shift
//! on Linux, where plain Ctrl combinations belong to the terminal
//! (Ctrl+C, Ctrl+W, and Ctrl+Q mean something to the program in the pane
//! and are never taken). Closing a window or quitting closes views only:
//! tmux sessions and panes keep running.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

gpui::actions!(
    dreamer,
    [
        /// Quit the desktop app. tmux and the daemon keep running.
        Quit,
        /// Hide the app (macOS).
        Hide,
        /// Hide the other apps (macOS).
        HideOthers,
        /// Show all apps (macOS).
        ShowAll,
        /// Open another workspace window.
        NewWindow,
        /// Close this window; its tmux session keeps running.
        CloseWindow,
        Minimize,
        /// The window's zoom (macOS "Zoom"): fill the screen without full screen.
        ZoomWindow,
        ToggleFullScreen,
        /// Copy the focused pane's selection.
        Copy,
        /// Paste into the focused pane (bracketed when the program asked).
        Paste,
        CommandPalette,
        /// Show only the focused pane, or all of them again.
        ZoomPane,
        ToggleSidebar,
        /// Find in the focused pane's history and screen.
        Find,
    ]
);

/// One command's shortcut: (macOS, Linux). `None` where the platform's
/// window manager or menu provides it, or nothing should take a key.
pub(crate) struct Shortcut {
    pub mac: Option<&'static str>,
    pub linux: Option<&'static str>,
    make: fn() -> Box<dyn gpui::Action>,
}

impl Shortcut {
    /// This platform's keystroke.
    pub(crate) fn keys(&self) -> Option<&'static str> {
        if cfg!(target_os = "macos") {
            self.mac
        } else {
            self.linux
        }
    }
}

macro_rules! shortcut {
    ($action:ident, $mac:expr, $linux:expr) => {
        Shortcut {
            mac: $mac,
            linux: $linux,
            make: || Box::new($action),
        }
    };
}

pub(crate) const SHORTCUTS: &[Shortcut] = &[
    shortcut!(Quit, Some("cmd-q"), Some("ctrl-shift-q")),
    shortcut!(Hide, Some("cmd-h"), None),
    shortcut!(HideOthers, Some("alt-cmd-h"), None),
    shortcut!(NewWindow, Some("cmd-n"), Some("ctrl-shift-n")),
    shortcut!(CloseWindow, Some("cmd-w"), Some("ctrl-shift-w")),
    shortcut!(Minimize, Some("cmd-m"), None),
    shortcut!(ToggleFullScreen, Some("ctrl-cmd-f"), Some("f11")),
    shortcut!(Copy, Some("cmd-c"), Some("ctrl-shift-c")),
    shortcut!(Paste, Some("cmd-v"), Some("ctrl-shift-v")),
    shortcut!(CommandPalette, Some("cmd-shift-p"), Some("ctrl-shift-p")),
    shortcut!(ZoomPane, Some("cmd-shift-enter"), Some("ctrl-shift-enter")),
    shortcut!(ToggleSidebar, Some("cmd-shift-s"), Some("ctrl-shift-s")),
    shortcut!(Find, Some("cmd-f"), Some("ctrl-shift-f")),
];

fn bindings(cx: &App) -> Vec<KeyBinding> {
    SHORTCUTS
        .iter()
        .filter_map(|s| {
            let keys = s.keys()?;
            Some(KeyBinding::load(keys, (s.make)(), None, false, None, cx))
        })
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("a built-in shortcut does not parse: {e}"))
}

/// The macOS menu bar. Each item's shortcut is shown from the bindings.
fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Dreamer").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Dreamer", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Dreamer", Quit),
        ]),
        Menu::new("Shell").items([
            MenuItem::action("New Window", NewWindow),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::action("Find…", Find),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", CommandPalette),
            MenuItem::action("Sessions Sidebar", ToggleSidebar),
            MenuItem::action("Zoom Pane", ZoomPane),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullScreen),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", ZoomWindow),
        ]),
    ]
}

/// What opens another workspace window (the daemon arguments it follows).
pub(crate) struct Opener(pub Box<dyn Fn(&mut App)>);

impl gpui::Global for Opener {}

/// Bind the shortcuts, set the menu bar, and handle the app-wide commands.
/// View commands (copy, paste, palette, zoom, sidebar) are handled by the
/// focused view; without one they do nothing.
pub(crate) fn install(cx: &mut App, open_window: impl Fn(&mut App) + 'static) {
    cx.set_global(Opener(Box::new(open_window)));
    let b = bindings(cx);
    cx.bind_keys(b);
    cx.set_menus(menus());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &NewWindow, cx| new_window(cx));
    let on_active = |cx: &mut App, f: fn(&mut gpui::Window)| {
        if let Some(w) = cx.active_window() {
            let _ = w.update(cx, |_, window, _| f(window));
        }
    };
    cx.on_action(move |_: &CloseWindow, cx| on_active(cx, gpui::Window::remove_window));
    cx.on_action(move |_: &Minimize, cx| on_active(cx, |w| w.minimize_window()));
    cx.on_action(move |_: &ZoomWindow, cx| on_active(cx, |w| w.zoom_window()));
    cx.on_action(move |_: &ToggleFullScreen, cx| on_active(cx, |w| w.toggle_fullscreen()));
    // On Linux an app without a window has nothing left to show; macOS
    // apps stay in the Dock and reopen a window from there.
    if !cfg!(target_os = "macos") {
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    }
}

/// Open another workspace window.
pub(crate) fn new_window(cx: &mut App) {
    if !cx.has_global::<Opener>() {
        return;
    }
    let opener = cx.remove_global::<Opener>();
    (opener.0)(cx);
    cx.set_global(opener);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shortcut_parses_and_none_is_taken_twice() {
        for platform in [true, false] {
            let keys: Vec<&str> = SHORTCUTS
                .iter()
                .filter_map(|s| if platform { s.mac } else { s.linux })
                .collect();
            for k in &keys {
                gpui::Keystroke::parse(k).unwrap_or_else(|e| panic!("{k}: {e}"));
                assert_eq!(keys.iter().filter(|x| *x == k).count(), 1, "{k} twice");
            }
        }
    }

    #[test]
    fn linux_shortcuts_leave_plain_ctrl_and_alt_keys_to_the_terminal() {
        for s in SHORTCUTS {
            let Some(k) = s.linux else { continue };
            let ks = gpui::Keystroke::parse(k).expect("parse");
            let m = ks.modifiers;
            let function_key = ks.key.starts_with('f') && ks.key.len() > 1;
            assert!(
                function_key || (m.control && m.shift),
                "{k} would take a key the program in the pane may need"
            );
        }
    }
}
