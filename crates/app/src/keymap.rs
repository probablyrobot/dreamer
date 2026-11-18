//! The desktop's commands, their shortcuts, and the macOS menu bar, all
//! from one table so a shortcut, its menu item, and its handler never
//! disagree.
//!
//! Shortcuts follow each platform's convention: Cmd on macOS, Ctrl+Shift
//! on Linux, where plain Ctrl combinations belong to the terminal
//! (Ctrl+C, Ctrl+W, and Ctrl+Q mean something to the program in the pane
//! and are never taken). Closing a window or quitting closes views only:
//! tmux sessions and panes keep running.

use gpui::{App, AppContext as _, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

gpui::actions!(
    dreamer,
    [
        /// The app's name and version.
        About,
        /// Open `dreamer.yaml` in the user's editor, creating it if missing.
        OpenSettings,
        /// Quit the desktop app. tmux and the daemon keep running.
        Quit,
        /// Hide the app (macOS).
        Hide,
        /// Hide the other apps (macOS).
        HideOthers,
        /// Show all apps (macOS).
        ShowAll,
        /// Open another window on a new tmux session.
        NewWindow,
        /// A new tab: a new tmux window in this window's session.
        NewTab,
        /// Split the focused pane: the new pane on the right.
        SplitRight,
        /// Split the focused pane: the new pane below.
        SplitDown,
        /// Close this window; its tmux session keeps running.
        CloseWindow,
        Minimize,
        /// The window's zoom (macOS "Zoom"): fill the screen without full screen.
        ZoomWindow,
        ToggleFullScreen,
        /// Every Dreamer window in front of other apps' windows.
        BringAllToFront,
        /// The session's next or previous tmux window.
        NextTab,
        PreviousTab,
        /// Copy the focused pane's selection.
        Copy,
        /// Paste into the focused pane (bracketed when the program asked).
        Paste,
        /// Cut the selection in a web page.
        Cut,
        /// Select everything in a web page's focused field or document.
        SelectAll,
        CommandPalette,
        /// Show only the focused pane, or all of them again.
        ZoomPane,
        ToggleSidebar,
        /// Find in the focused pane's history and screen.
        Find,
        /// The harness drawer: show and focus it, focus it, or hide it.
        ToggleDrawer,
        /// Start a fresh harness in the drawer.
        NewDrawer,
        /// The terminal text one point bigger or smaller, or back to the
        /// configured size, in every window (the file is not changed).
        FontBigger,
        FontSmaller,
        FontReset,
    ]
);

/// One command's shortcut: (macOS, Linux). `None` where the platform's
/// window manager or menu provides it, or nothing should take a key.
pub(crate) struct Shortcut {
    pub mac: Option<&'static str>,
    pub linux: Option<&'static str>,
    make: fn() -> Box<dyn gpui::Action>,
    /// The binding of these keys to the command, in a key context or
    /// everywhere.
    bind: fn(&'static str, Option<&'static str>) -> KeyBinding,
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
            bind: |keys, context| KeyBinding::new(keys, $action, context),
        }
    };
}

/// Every command with a menu item, a shortcut, or both. Shifted symbols
/// are written as the symbol (`cmd-}` is Cmd+Shift+]), as GPUI reports them.
pub(crate) const SHORTCUTS: &[Shortcut] = &[
    shortcut!(About, None, None),
    shortcut!(OpenSettings, Some("cmd-,"), None),
    shortcut!(Quit, Some("cmd-q"), Some("ctrl-shift-q")),
    shortcut!(Hide, Some("cmd-h"), None),
    shortcut!(HideOthers, Some("alt-cmd-h"), None),
    shortcut!(ShowAll, None, None),
    shortcut!(NewWindow, Some("cmd-n"), Some("ctrl-shift-n")),
    shortcut!(NewTab, Some("cmd-t"), Some("ctrl-shift-t")),
    shortcut!(SplitRight, Some("cmd-d"), Some("ctrl-shift-o")),
    shortcut!(SplitDown, Some("cmd-shift-d"), Some("ctrl-shift-e")),
    shortcut!(CloseWindow, Some("cmd-w"), Some("ctrl-shift-w")),
    shortcut!(Minimize, Some("cmd-m"), None),
    shortcut!(ZoomWindow, None, None),
    shortcut!(ToggleFullScreen, Some("ctrl-cmd-f"), Some("f11")),
    shortcut!(BringAllToFront, None, None),
    shortcut!(NextTab, Some("cmd-}"), Some("ctrl-shift-pagedown")),
    shortcut!(PreviousTab, Some("cmd-{"), Some("ctrl-shift-pageup")),
    shortcut!(Copy, Some("cmd-c"), Some("ctrl-shift-c")),
    shortcut!(Paste, Some("cmd-v"), Some("ctrl-shift-v")),
    shortcut!(CommandPalette, Some("cmd-shift-p"), Some("ctrl-shift-p")),
    shortcut!(ZoomPane, Some("cmd-shift-enter"), Some("ctrl-shift-enter")),
    shortcut!(ToggleSidebar, Some("cmd-shift-s"), Some("ctrl-shift-s")),
    shortcut!(Find, Some("cmd-f"), Some("ctrl-shift-f")),
    shortcut!(ToggleDrawer, Some("cmd-j"), Some("ctrl-shift-j")),
    shortcut!(NewDrawer, Some("cmd-shift-j"), Some("ctrl-alt-shift-j")),
    shortcut!(FontBigger, Some("cmd-="), None),
    shortcut!(FontSmaller, Some("cmd--"), None),
    shortcut!(FontReset, Some("cmd-0"), None),
];

/// The key context of a web pane (`crate::web::WebPane`).
pub(crate) const WEB_PANE: &str = "WebPane";

/// Shortcuts only a web page takes. With Copy and Paste above, they become
/// Chromium's own edit commands there. Every other key goes to the page,
/// except the shortcuts above, which work in a page as anywhere.
pub(crate) const WEB_SHORTCUTS: &[Shortcut] = &[
    shortcut!(Cut, Some("cmd-x"), Some("ctrl-shift-x")),
    shortcut!(SelectAll, Some("cmd-a"), Some("ctrl-shift-a")),
];

fn bindings() -> Vec<KeyBinding> {
    SHORTCUTS
        .iter()
        .map(|s| (s, None))
        .chain(WEB_SHORTCUTS.iter().map(|s| (s, Some(WEB_PANE))))
        .filter_map(|(s, context)| Some((s.bind)(s.keys()?, context)))
        .collect()
}

/// This platform's shortcut for `action`, written for a menu: `⇧⌘D` on
/// macOS, `Ctrl+Shift+O` on Linux.
pub(crate) fn shortcut_label(action: &dyn gpui::Action) -> Option<String> {
    let keys = SHORTCUTS
        .iter()
        .find(|s| (s.make)().partial_eq(action))?
        .keys()?;
    let ks = gpui::Keystroke::parse(keys).ok()?;
    let m = ks.modifiers;
    let key = match ks.key.as_str() {
        "enter" => "Return".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        "pageup" => "PageUp".to_owned(),
        k => k.to_uppercase(),
    };
    Some(if cfg!(target_os = "macos") {
        let mut s = String::new();
        for (on, sym) in [
            (m.control, "⌃"),
            (m.alt, "⌥"),
            (m.shift, "⇧"),
            (m.platform, "⌘"),
        ] {
            if on {
                s.push_str(sym);
            }
        }
        s + &key
    } else {
        let mut parts: Vec<&str> = Vec::new();
        for (on, name) in [(m.control, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift")] {
            if on {
                parts.push(name);
            }
        }
        parts.push(&key);
        parts.join("+")
    })
}

/// The macOS menu bar, short like a terminal's. Each item's shortcut is
/// shown from the bindings. The menu named "Window" is the app's window
/// menu, so macOS lists the open windows under it.
fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Dreamer").items([
            MenuItem::action("About Dreamer", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
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
            MenuItem::action("New Tab", NewTab),
            MenuItem::separator(),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
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
            MenuItem::action("Harness Drawer", ToggleDrawer),
            MenuItem::action("Zoom Pane", ZoomPane),
            MenuItem::separator(),
            MenuItem::action("Bigger", FontBigger),
            MenuItem::action("Smaller", FontSmaller),
            MenuItem::action("Actual Size", FontReset),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullScreen),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", ZoomWindow),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PreviousTab),
            MenuItem::separator(),
            MenuItem::action("Bring All to Front", BringAllToFront),
        ]),
    ]
}

/// What opens another workspace window (the daemon arguments it follows).
pub(crate) struct Opener(pub Box<dyn Fn(&mut App)>);

impl gpui::Global for Opener {}

/// Bind the shortcuts, set the menu bar, and handle the app-wide commands.
/// View commands (copy, paste, tabs, splits, palette, zoom, sidebar,
/// drawer) are handled by the focused view; without one they do nothing.
pub(crate) fn install(cx: &mut App, open_window: impl Fn(&mut App) + 'static) {
    cx.set_global(Opener(Box::new(open_window)));
    cx.bind_keys(bindings());
    cx.set_menus(menus());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &NewWindow, cx| new_window(cx));
    cx.on_action(|_: &About, cx| about(cx));
    cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
    cx.on_action(|_: &FontBigger, cx| crate::live::font_step(cx, Some(1.0)));
    cx.on_action(|_: &FontSmaller, cx| crate::live::font_step(cx, Some(-1.0)));
    cx.on_action(|_: &FontReset, cx| crate::live::font_step(cx, None));
    cx.on_action(|_: &BringAllToFront, cx| bring_all_to_front(cx));
    let on_active = |cx: &mut App, f: fn(&mut gpui::Window, &mut App)| {
        if let Some(w) = cx.active_window() {
            let _ = w.update(cx, |_, window, cx| f(window, cx));
        }
    };
    cx.on_action(move |_: &CloseWindow, cx| on_active(cx, crate::workspace::close_window));
    cx.on_action(move |_: &Minimize, cx| on_active(cx, |w, _| w.minimize_window()));
    cx.on_action(move |_: &ZoomWindow, cx| on_active(cx, |w, _| w.zoom_window()));
    cx.on_action(move |_: &ToggleFullScreen, cx| on_active(cx, |w, _| w.toggle_fullscreen()));
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

/// A minimal about box: the name and the version.
fn about(cx: &mut App) {
    let Some(w) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    let _ = w.update(cx, |_, window, cx| {
        let answer = window.prompt(
            gpui::PromptLevel::Info,
            "Dreamer",
            Some(&format!("Version {}", env!("CARGO_PKG_VERSION"))),
            &["OK"],
            cx,
        );
        cx.foreground_executor()
            .spawn(async move {
                let _ = answer.await;
            })
            .detach();
    });
}

/// Open `dreamer.yaml` in the user's editor (`open -t` on macOS,
/// `xdg-open` on Linux), first creating it with no settings if it is
/// missing. Off the UI thread.
fn open_settings(cx: &mut App) {
    let Some(path) = workspace_config::settings_path() else {
        eprintln!("dreamer: no configuration directory (HOME is not set)");
        return;
    };
    cx.background_spawn(async move {
        if let Err(e) = workspace_config::ensure_settings_file(&path) {
            eprintln!("dreamer: cannot create {}: {e}", path.display());
            return;
        }
        let mut open = if cfg!(target_os = "macos") {
            let mut c = std::process::Command::new("open");
            c.arg("-t");
            c
        } else {
            std::process::Command::new("xdg-open")
        };
        match open.arg(&path).status() {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("dreamer: opening {} failed ({s})", path.display()),
            Err(e) => eprintln!("dreamer: cannot open {}: {e}", path.display()),
        }
    })
    .detach();
}

/// Every window to the front, the active one last so it stays on top.
fn bring_all_to_front(cx: &mut App) {
    let active = cx.active_window();
    let mut windows = cx.windows();
    windows.retain(|w| Some(*w) != active);
    windows.extend(active);
    for w in windows {
        let _ = w.update(cx, |_, window, _| window.activate_window());
    }
    cx.activate(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command a keystroke runs: in a web page, or (`web` false) in
    /// another pane.
    fn command(keys: &'static str, web: bool) -> Option<String> {
        let keymap = gpui::Keymap::new(bindings());
        let context: Vec<gpui::KeyContext> = if web {
            vec![gpui::KeyContext::parse(WEB_PANE).expect("context")]
        } else {
            Vec::new()
        };
        let ks = gpui::Keystroke::parse(keys).expect("keystroke");
        let (found, _) = keymap.bindings_for_input(&[ks], &context);
        found.first().map(|b| b.action().name().to_owned())
    }

    #[test]
    fn a_page_gets_every_key_but_dreamers_shortcuts_and_edits_with_chromium() {
        let (copy, paste, cut, all, drawer) = if cfg!(target_os = "macos") {
            ("cmd-c", "cmd-v", "cmd-x", "cmd-a", "cmd-j")
        } else {
            (
                "ctrl-shift-c",
                "ctrl-shift-v",
                "ctrl-shift-x",
                "ctrl-shift-a",
                "ctrl-shift-j",
            )
        };
        // Dreamer's own shortcuts work from inside a page.
        assert_eq!(
            command(drawer, true).as_deref(),
            Some("dreamer::ToggleDrawer")
        );
        // The edit shortcuts become Chromium's edit commands there.
        for (keys, action) in [
            (copy, "dreamer::Copy"),
            (paste, "dreamer::Paste"),
            (cut, "dreamer::Cut"),
            (all, "dreamer::SelectAll"),
        ] {
            assert_eq!(command(keys, true).as_deref(), Some(action), "{keys}");
        }
        // Cut and select all are taken only in a page: elsewhere the keys
        // reach the program in the pane.
        assert_eq!(command(cut, false), None);
        assert_eq!(command(all, false), None);
        // Everything else goes to the page.
        for keys in [
            "a",
            "shift-a",
            "enter",
            "escape",
            "backspace",
            "ctrl-a",
            "alt-f",
        ] {
            assert_eq!(command(keys, true), None, "{keys}");
        }
    }

    #[test]
    fn every_shortcut_parses_and_none_is_taken_twice() {
        for platform in [true, false] {
            let keys: Vec<&str> = SHORTCUTS
                .iter()
                .chain(WEB_SHORTCUTS)
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
        for s in SHORTCUTS.iter().chain(WEB_SHORTCUTS) {
            let Some(k) = s.linux else { continue };
            let ks = gpui::Keystroke::parse(k).expect("parse");
            let m = ks.modifiers;
            let function_key = ks.key.starts_with('f') && ks.key.len() > 1;
            assert!(
                function_key || (m.control && m.shift),
                "{k} would take a key the program in the pane may need"
            );
            // GPUI drops Shift from a shifted symbol (Ctrl+Shift+, arrives
            // as Ctrl+<), so a symbol here would never match.
            assert!(
                ks.key.len() > 1 || ks.key.chars().all(|c| c.is_ascii_alphabetic()),
                "{k}: use a letter or a named key with Ctrl+Shift"
            );
        }
    }

    #[test]
    fn every_menu_item_is_a_command_in_the_table() {
        fn walk(items: &[MenuItem], out: &mut Vec<String>) {
            for item in items {
                match item {
                    MenuItem::Action { name, action, .. } => {
                        if !SHORTCUTS.iter().any(|s| (s.make)().partial_eq(&**action)) {
                            out.push(name.to_string());
                        }
                    }
                    MenuItem::Submenu(m) => walk(&m.items, out),
                    MenuItem::Separator | MenuItem::SystemMenu(_) => {}
                }
            }
        }
        let mut missing = Vec::new();
        for m in menus() {
            walk(&m.items, &mut missing);
        }
        assert!(missing.is_empty(), "not in SHORTCUTS: {missing:?}");
    }

    #[test]
    fn menu_shortcut_labels_name_the_platforms_keys() {
        let label = shortcut_label(&SplitDown).expect("split down");
        if cfg!(target_os = "macos") {
            assert_eq!(label, "⇧⌘D");
        } else {
            assert_eq!(label, "Ctrl+Shift+E");
        }
        assert_eq!(shortcut_label(&About), None);
    }
}
