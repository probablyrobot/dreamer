//! Keyboard, text, IME, and paste from the window to the pane.
//!
//! One policy for every key press:
//! 1. Paste shortcuts (Cmd-V on macOS, Ctrl-Shift-V elsewhere) paste the
//!    clipboard.
//! 2. Other Cmd (macOS) shortcuts belong to the application and are left
//!    alone.
//! 3. Special keys (arrows, Enter, F-keys, ...) and anything with Ctrl or
//!    Alt are sent as key events, encoded by the daemon against the pane's
//!    current modes.
//! 4. Everything else is text. It reaches the platform's text input, so
//!    IME composition works: preedit is only drawn, and committed text is
//!    sent.
//!
//! Mouse presses, releases, drags, and wheel steps go to the pane as cells
//! of the grid; the daemon encodes them only if the application tracks the
//! mouse.

use api::terminal_input::Event;
use gpui::{Keystroke, Modifiers, Pixels, Point, ScrollDelta, Size};

/// W3C `KeyboardEvent.code` for a GPUI key name, when it has one.
pub fn code_for(key: &str) -> Option<String> {
    let special = match key {
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "escape" => "Escape",
        "space" => "Space",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "insert" => "Insert",
        "delete" => "Delete",
        "-" => "Minus",
        "=" => "Equal",
        "[" => "BracketLeft",
        "]" => "BracketRight",
        "\\" => "Backslash",
        ";" => "Semicolon",
        "'" => "Quote",
        "," => "Comma",
        "." => "Period",
        "/" => "Slash",
        "`" => "Backquote",
        _ => "",
    };
    if !special.is_empty() {
        return Some(special.to_owned());
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=25).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => {
            Some(format!("Key{}", c.to_ascii_uppercase()))
        }
        (Some(c), None) if c.is_ascii_digit() => Some(format!("Digit{c}")),
        _ => None,
    }
}

fn is_special(key: &str) -> bool {
    matches!(
        key,
        "enter"
            | "tab"
            | "backspace"
            | "escape"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "insert"
            | "delete"
    ) || (key.starts_with('f') && key.len() > 1 && key[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn mods_bits(m: &Modifiers) -> u32 {
    u32::from(m.shift)
        | (u32::from(m.control) << 1)
        | (u32::from(m.alt) << 2)
        | (u32::from(m.platform) << 3)
}

/// Shift-PageUp/PageDown and Shift-Home/End scroll the local history by a
/// page (`page` rows) or to its ends.
pub fn scroll_key(k: &Keystroke, page: isize) -> Option<replica::feed::Scroll> {
    use replica::feed::Scroll;
    let m = &k.modifiers;
    if !m.shift || m.control || m.alt || m.platform {
        return None;
    }
    match k.key.as_str() {
        "pageup" => Some(Scroll::Delta(-page)),
        "pagedown" => Some(Scroll::Delta(page)),
        "home" => Some(Scroll::Top),
        "end" => Some(Scroll::Bottom),
        _ => None,
    }
}

/// An arrow key per wheel step, for full-screen programs on the alternate
/// screen that do not track the mouse (it has no history to scroll).
pub fn wheel_arrows(steps: i32) -> Vec<Event> {
    let key = if steps > 0 { "ArrowUp" } else { "ArrowDown" };
    (0..steps.unsigned_abs())
        .map(|_| {
            Event::Key(api::KeyInput {
                key: key.into(),
                ..api::KeyInput::default()
            })
        })
        .collect()
}

/// The modifier that turns a click into "open this link": Cmd on macOS,
/// Ctrl elsewhere.
pub fn open_modifier(m: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    }
}

/// What a key press means for the terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyIntent {
    /// Paste the clipboard.
    Paste,
    /// Copy the selection.
    Copy,
    /// Send this event.
    Send(Event),
    /// Not ours: an application shortcut, or text the platform text input
    /// delivers (so IME keeps working).
    Pass,
}

/// The workspace's zoom toggle: Cmd-Shift-Enter on macOS, Ctrl-Shift-Enter
/// elsewhere. Never sent to the pane.
pub fn is_zoom_shortcut(k: &Keystroke) -> bool {
    let m = &k.modifiers;
    let primary = if cfg!(target_os = "macos") {
        m.platform && !m.control
    } else {
        m.control && !m.platform
    };
    k.key == "enter" && primary && m.shift && !m.alt
}

/// The workspace's command palette: Cmd-Shift-P on macOS, Ctrl-Shift-P
/// elsewhere. Never sent to the pane.
pub fn is_palette_shortcut(k: &Keystroke) -> bool {
    let m = &k.modifiers;
    let primary = if cfg!(target_os = "macos") {
        m.platform && !m.control
    } else {
        m.control && !m.platform
    };
    k.key == "p" && primary && m.shift && !m.alt
}

/// The clipboard shortcuts: Cmd-C and Cmd-V on macOS, Ctrl-Shift-C and
/// Ctrl-Shift-V elsewhere (plain Ctrl-C belongs to the program).
fn clipboard_intent(k: &Keystroke) -> Option<KeyIntent> {
    let m = &k.modifiers;
    let primary = if cfg!(target_os = "macos") {
        m.platform && !m.control && !m.alt
    } else {
        m.control && m.shift && !m.alt && !m.platform
    };
    match k.key.as_str() {
        "v" if primary => Some(KeyIntent::Paste),
        "c" if primary => Some(KeyIntent::Copy),
        _ => None,
    }
}

pub fn intent(k: &Keystroke, held: bool) -> KeyIntent {
    let m = &k.modifiers;
    if is_zoom_shortcut(k) || is_palette_shortcut(k) {
        return KeyIntent::Pass;
    }
    if let Some(i) = clipboard_intent(k) {
        return i;
    }
    if cfg!(target_os = "macos") && m.platform {
        return KeyIntent::Pass;
    }
    if !(is_special(&k.key) || m.control || m.alt) {
        return KeyIntent::Pass;
    }
    match code_for(&k.key) {
        Some(code) => KeyIntent::Send(Event::Key(api::KeyInput {
            key: code,
            text: k.key_char.clone().filter(|_| !m.control),
            mods: mods_bits(m),
            held,
        })),
        None => KeyIntent::Pass,
    }
}

/// What a key press means for a native app view (an app pane or panel).
#[derive(Debug, Clone, PartialEq)]
pub enum AppKey {
    /// Copy the view's selection.
    Copy,
    /// Paste the clipboard into the focused input.
    Paste,
    /// Send this key, by the app model's names (`Enter`, `Up`, `Ctrl-c`).
    Key(String),
    /// Not the view's: a Dreamer or application shortcut, or text that the
    /// platform's text input delivers (so IME composition works).
    Pass,
}

/// The same policy as [`intent`], for an app view: the clipboard
/// shortcuts copy and paste, Dreamer's own shortcuts and Cmd (macOS) or
/// Alt combinations are left alone, special keys and Ctrl combinations
/// are keys by the model's names, and text (Space included) goes through
/// the platform's text input.
pub fn app_intent(k: &Keystroke) -> AppKey {
    let m = &k.modifiers;
    if is_zoom_shortcut(k) || is_palette_shortcut(k) {
        return AppKey::Pass;
    }
    match clipboard_intent(k) {
        Some(KeyIntent::Copy) => return AppKey::Copy,
        Some(KeyIntent::Paste) => return AppKey::Paste,
        _ => {}
    }
    if m.platform || m.alt {
        return AppKey::Pass;
    }
    let named = match k.key.as_str() {
        "enter" => "Enter",
        "escape" => "Esc",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            return AppKey::Key(f.to_uppercase());
        }
        _ => "",
    };
    if !named.is_empty() {
        return AppKey::Key(named.to_owned());
    }
    let mut chars = k.key.chars();
    match (chars.next(), chars.next()) {
        _ if !m.control => AppKey::Pass,
        (Some(ch), None) => AppKey::Key(format!("Ctrl-{}", ch.to_ascii_lowercase())),
        _ => AppKey::Pass,
    }
}

/// Wheel steps in a native app view, positive down (the selection moves
/// with the content), from a scroll of `delta`; `carry` keeps the part of
/// a step a trackpad has not finished. `line` is the height of one row.
pub fn app_wheel_steps(carry: &mut f32, delta: ScrollDelta, line: Pixels) -> i32 {
    *carry -= match delta {
        ScrollDelta::Lines(d) => d.y,
        ScrollDelta::Pixels(d) => f32::from(d.y) / f32::from(line).max(1.0),
    };
    let steps = carry.trunc();
    *carry -= steps;
    steps.clamp(-MAX_WHEEL_STEPS, MAX_WHEEL_STEPS) as i32
}

/// Where the grid is drawn, for turning window positions into cells.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    pub origin: Point<Pixels>,
    pub cell: Size<Pixels>,
    pub cols: u16,
    pub rows: u16,
}

impl Grid {
    /// A window position in cells, fractional and not clamped: selection
    /// needs the side of a cell and positions past the edges.
    pub fn cell_pos(&self, p: Point<Pixels>) -> replica::feed::CellPos {
        replica::feed::CellPos {
            x: f32::from(p.x - self.origin.x) / f32::from(self.cell.width).max(1.0),
            y: f32::from(p.y - self.origin.y) / f32::from(self.cell.height).max(1.0),
        }
    }

    /// The cell under a window position, clamped to the grid.
    pub fn cell_at(&self, p: Point<Pixels>) -> (u32, u32) {
        let at = |v: Pixels, origin: Pixels, size: Pixels, n: u16| {
            let i = (f32::from(v - origin) / f32::from(size).max(1.0)).floor();
            i.clamp(0.0, f32::from(n.saturating_sub(1))) as u32
        };
        (
            at(p.x, self.origin.x, self.cell.width, self.cols),
            at(p.y, self.origin.y, self.cell.height, self.rows),
        )
    }
}

/// Most wheel steps one scroll event may send.
const MAX_WHEEL_STEPS: f32 = 32.0;

/// Mouse state between events: the held button, the last reported cell
/// (motion is sent only when the cell changes), and the part of a wheel
/// step not yet sent (trackpads scroll in pixels).
#[derive(Debug, Default)]
pub struct Pointer {
    held: Option<api::MouseButton>,
    last: Option<(u32, u32)>,
    wheel: f32,
}

fn button(b: gpui::MouseButton) -> Option<api::MouseButton> {
    match b {
        gpui::MouseButton::Left => Some(api::MouseButton::Left),
        gpui::MouseButton::Middle => Some(api::MouseButton::Middle),
        gpui::MouseButton::Right => Some(api::MouseButton::Right),
        gpui::MouseButton::Navigate(_) => None,
    }
}

fn mouse(
    action: api::MouseAction,
    button: Option<api::MouseButton>,
    (col, row): (u32, u32),
    m: &Modifiers,
    buttons_held: bool,
) -> Event {
    Event::Mouse(api::MouseInput {
        action: action as i32,
        button: button.unwrap_or(api::MouseButton::None) as i32,
        col,
        row,
        mods: mods_bits(m),
        buttons_held,
    })
}

impl Pointer {
    pub fn down(
        &mut self,
        g: &Grid,
        b: gpui::MouseButton,
        p: Point<Pixels>,
        m: &Modifiers,
    ) -> Option<Event> {
        let b = button(b)?;
        let cell = g.cell_at(p);
        self.held = Some(b);
        self.last = Some(cell);
        Some(mouse(api::MouseAction::Press, Some(b), cell, m, true))
    }

    pub fn up(
        &mut self,
        g: &Grid,
        b: gpui::MouseButton,
        p: Point<Pixels>,
        m: &Modifiers,
    ) -> Option<Event> {
        let b = button(b)?;
        if self.held != Some(b) {
            return None;
        }
        self.held = None;
        let cell = g.cell_at(p);
        self.last = Some(cell);
        Some(mouse(api::MouseAction::Release, Some(b), cell, m, false))
    }

    pub fn moved(&mut self, g: &Grid, p: Point<Pixels>, m: &Modifiers) -> Option<Event> {
        let cell = g.cell_at(p);
        if self.last == Some(cell) {
            return None;
        }
        self.last = Some(cell);
        Some(mouse(
            api::MouseAction::Motion,
            self.held,
            cell,
            m,
            self.held.is_some(),
        ))
    }

    /// Whole wheel steps in a scroll, positive up; the fraction of a step
    /// carries over to the next scroll.
    pub fn steps(&mut self, g: &Grid, delta: ScrollDelta) -> i32 {
        self.wheel += match delta {
            ScrollDelta::Lines(d) => d.y,
            ScrollDelta::Pixels(d) => f32::from(d.y) / f32::from(g.cell.height).max(1.0),
        };
        let steps = self.wheel.trunc();
        self.wheel -= steps;
        steps.clamp(-MAX_WHEEL_STEPS, MAX_WHEEL_STEPS) as i32
    }

    /// Wheel steps for a scroll, one press each; positive y scrolls up.
    pub fn scroll(
        &mut self,
        g: &Grid,
        p: Point<Pixels>,
        delta: ScrollDelta,
        m: &Modifiers,
    ) -> Vec<Event> {
        let steps = self.steps(g, delta);
        let b = if steps > 0 {
            api::MouseButton::WheelUp
        } else {
            api::MouseButton::WheelDown
        };
        let cell = g.cell_at(p);
        (0..steps.unsigned_abs())
            .map(|_| {
                mouse(
                    api::MouseAction::Press,
                    Some(b),
                    cell,
                    m,
                    self.held.is_some(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn grid() -> Grid {
        Grid {
            origin: point(px(8.0), px(8.0)),
            cell: size(px(10.0), px(20.0)),
            cols: 80,
            rows: 24,
        }
    }

    fn cell_of(e: Event) -> (i32, i32, u32, u32) {
        match e {
            Event::Mouse(m) => (m.action, m.button, m.col, m.row),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn shift_page_keys_scroll_history_and_plain_ones_do_not() {
        use replica::feed::Scroll;
        assert_eq!(
            scroll_key(&ks("shift-pageup"), 23),
            Some(Scroll::Delta(-23))
        );
        assert_eq!(
            scroll_key(&ks("shift-pagedown"), 23),
            Some(Scroll::Delta(23))
        );
        assert_eq!(scroll_key(&ks("shift-home"), 23), Some(Scroll::Top));
        assert_eq!(scroll_key(&ks("shift-end"), 23), Some(Scroll::Bottom));
        assert_eq!(scroll_key(&ks("pageup"), 23), None);
        assert_eq!(scroll_key(&ks("ctrl-shift-pageup"), 23), None);
    }

    #[test]
    fn positions_become_clamped_cells() {
        let g = grid();
        assert_eq!(g.cell_at(point(px(8.0), px(8.0))), (0, 0));
        assert_eq!(g.cell_at(point(px(47.9), px(48.0))), (3, 2));
        assert_eq!(g.cell_at(point(px(0.0), px(-50.0))), (0, 0));
        assert_eq!(g.cell_at(point(px(5000.0), px(5000.0))), (79, 23));
    }

    #[test]
    fn drags_report_only_cell_changes_and_wheel_keeps_fractions() {
        use api::{MouseAction as A, MouseButton as B};
        let (g, m) = (grid(), Modifiers::default());
        let mut p = Pointer::default();
        let down = p.down(&g, gpui::MouseButton::Left, point(px(20.0), px(10.0)), &m);
        assert_eq!(
            cell_of(down.expect("press")),
            (A::Press as i32, B::Left as i32, 1, 0)
        );
        assert!(
            p.moved(&g, point(px(25.0), px(12.0)), &m).is_none(),
            "same cell"
        );
        let drag = p.moved(&g, point(px(31.0), px(12.0)), &m).expect("drag");
        assert_eq!(cell_of(drag), (A::Motion as i32, B::Left as i32, 2, 0));
        assert!(
            p.up(&g, gpui::MouseButton::Right, point(px(31.0), px(12.0)), &m)
                .is_none()
        );
        let up = p
            .up(&g, gpui::MouseButton::Left, point(px(31.0), px(12.0)), &m)
            .expect("release");
        assert_eq!(cell_of(up), (A::Release as i32, B::Left as i32, 2, 0));
        // Trackpad pixels: 30 px is one and a half rows; the half is kept.
        let at = point(px(8.0), px(8.0));
        let one = p.scroll(&g, at, ScrollDelta::Pixels(point(px(0.0), px(30.0))), &m);
        assert_eq!(one.len(), 1);
        assert_eq!(cell_of(one[0].clone()).1, B::WheelUp as i32);
        assert_eq!(
            p.scroll(&g, at, ScrollDelta::Pixels(point(px(0.0), px(10.0))), &m)
                .len(),
            1
        );
        let down = p.scroll(&g, at, ScrollDelta::Lines(point(0.0, -3.0)), &m);
        assert_eq!(down.len(), 3);
        assert_eq!(cell_of(down[0].clone()).1, B::WheelDown as i32);
    }

    #[test]
    fn app_views_get_keys_by_the_models_names_and_text_goes_to_the_text_input() {
        let k = |s: &'static str| app_intent(&ks(s));
        let key = |s: &str| AppKey::Key(s.to_owned());
        assert_eq!(k("enter"), key("Enter"));
        assert_eq!(k("down"), key("Down"));
        assert_eq!(k("shift-tab"), key("Tab"));
        assert_eq!(k("ctrl-c"), key("Ctrl-c"));
        assert_eq!(k("f2"), key("F2"));
        // Text, Space included, goes through the platform's text input
        // (IME); the view decides whether it is typed into an input.
        assert_eq!(k("a"), AppKey::Pass);
        assert_eq!(k("space"), AppKey::Pass);
        assert_eq!(k("shift-a"), AppKey::Pass);
        // Dreamer's shortcuts stay Dreamer's; the clipboard ones copy and
        // paste, never reaching the app as Ctrl-c (which quits it).
        assert_eq!(k("alt-b"), AppKey::Pass);
        if cfg!(target_os = "macos") {
            assert_eq!(k("cmd-c"), AppKey::Copy);
            assert_eq!(k("cmd-v"), AppKey::Paste);
            assert_eq!(k("cmd-shift-p"), AppKey::Pass);
            assert_eq!(k("cmd-j"), AppKey::Pass);
        } else {
            assert_eq!(k("ctrl-shift-c"), AppKey::Copy);
            assert_eq!(k("ctrl-shift-v"), AppKey::Paste);
            assert_eq!(k("ctrl-shift-p"), AppKey::Pass);
        }
    }

    #[test]
    fn the_wheel_moves_an_app_selection_by_whole_rows() {
        let mut carry = 0.0;
        // Scrolling down (negative y) moves the selection down.
        assert_eq!(
            app_wheel_steps(&mut carry, ScrollDelta::Lines(point(0.0, -2.0)), px(16.0)),
            2
        );
        assert_eq!(
            app_wheel_steps(
                &mut carry,
                ScrollDelta::Pixels(point(px(0.0), px(24.0))),
                px(16.0)
            ),
            -1
        );
        // The half row left over counts with the next scroll.
        assert_eq!(
            app_wheel_steps(
                &mut carry,
                ScrollDelta::Pixels(point(px(0.0), px(8.0))),
                px(16.0)
            ),
            -1
        );
    }

    fn ks(s: &'static str) -> Keystroke {
        Keystroke::parse(s).expect("keystroke")
    }

    fn key(i: KeyIntent) -> (String, u32) {
        match i {
            KeyIntent::Send(Event::Key(k)) => (k.key, k.mods),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn special_and_modified_keys_are_key_events_and_text_is_left_to_ime() {
        assert_eq!(key(intent(&ks("up"), false)), ("ArrowUp".into(), 0));
        assert_eq!(key(intent(&ks("ctrl-c"), false)), ("KeyC".into(), 2));
        assert_eq!(
            key(intent(&ks("shift-left"), false)),
            ("ArrowLeft".into(), 1)
        );
        assert_eq!(key(intent(&ks("alt-b"), false)), ("KeyB".into(), 4));
        assert_eq!(key(intent(&ks("f5"), false)), ("F5".into(), 0));
        assert_eq!(key(intent(&ks("ctrl-["), false)), ("BracketLeft".into(), 2));
        assert_eq!(intent(&ks("a"), false), KeyIntent::Pass);
        assert_eq!(intent(&ks("shift-a"), false), KeyIntent::Pass);
        if cfg!(target_os = "macos") {
            assert_eq!(intent(&ks("cmd-c"), false), KeyIntent::Copy);
            assert_eq!(intent(&ks("cmd-v"), false), KeyIntent::Paste);
            assert_eq!(intent(&ks("cmd-q"), false), KeyIntent::Pass);
        } else {
            assert_eq!(intent(&ks("ctrl-shift-v"), false), KeyIntent::Paste);
            assert_eq!(intent(&ks("ctrl-shift-c"), false), KeyIntent::Copy);
            assert_eq!(key(intent(&ks("ctrl-c"), false)), ("KeyC".into(), 2));
        }
    }
}
