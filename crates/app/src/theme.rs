//! Design tokens for the desktop's chrome (spec 14.2, 14.4): two default
//! skins designed together, a dark and a light one, neither an inversion of
//! the other. Terminal text is the brightest, sharpest thing on screen;
//! chrome stays lower in contrast with one accent color.
//!
//! The chrome is drawn from the same color scheme the daemon gives the
//! terminals (`Settings::scheme`), so the window is one theme: Dreamer's
//! two schemes come with these hand-tuned skins, and any other scheme gets
//! chrome derived from its own background, foreground, and ANSI colors.
//! The terminal's colors themselves stay canonical state (spec 14.2), set
//! by the daemon; the chrome only follows them.

use gpui::{Rgba, Window, rgb, rgba};
use terminal_core::schemes::Scheme;
use terminal_ghostty::core as terminal_core;

/// Semantic chrome colors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    pub name: &'static str,
    /// The window behind everything.
    pub background: u32,
    /// Tab strip and sidebar.
    pub surface: u32,
    /// Floating layers: the command palette.
    pub raised: u32,
    /// The gaps between panes.
    pub divider: u32,
    /// The selected tab, session, or palette row.
    pub selected: u32,
    pub text: u32,
    /// Secondary text: inactive tabs, labels.
    pub muted: u32,
    /// The quietest readable text: hints, the zoom control.
    pub faint: u32,
    /// The one accent: drag preview, links.
    pub accent: u32,
    /// The focused pane's hairline and the palette border.
    pub focus: u32,
    pub danger: u32,
    pub danger_surface: u32,
    pub danger_text: u32,
    pub warning: u32,
    pub warning_surface: u32,
    pub warning_text: u32,
    /// The visual bell's wash: RGBA.
    pub flash: u32,
    /// A highlighted line's wash (a filter's decoration): RGBA.
    pub mark: u32,
}

pub(crate) const DARK: Theme = Theme {
    name: "default-dark",
    background: 0x14161a,
    surface: 0x101216,
    raised: 0x1f232a,
    divider: 0x1f232a,
    selected: 0x2a2f38,
    text: 0xe6e6e6,
    muted: 0xb8bec8,
    faint: 0x9aa0aa,
    accent: 0x8ab4f8,
    focus: 0x3d4a5c,
    danger: 0xf28b82,
    danger_surface: 0x5a2a2a,
    danger_text: 0xf0d0d0,
    warning: 0xd7b46a,
    warning_surface: 0x3a2f1c,
    warning_text: 0xf0dcb0,
    flash: 0xffffff22,
    mark: 0x8ab4f833,
};

/// Warm neutrals rather than an inverted dark: paper-like background,
/// slightly darker chrome, a deeper accent so it reads on light ground.
pub(crate) const LIGHT: Theme = Theme {
    name: "default-light",
    background: 0xf6f6f4,
    surface: 0xecebe7,
    raised: 0xffffff,
    divider: 0xd9d7d1,
    selected: 0xdedcd5,
    text: 0x1f2328,
    muted: 0x454b56,
    faint: 0x585f6b,
    accent: 0x2f6fde,
    focus: 0x9db4dc,
    danger: 0xb3261e,
    danger_surface: 0xfbe3e0,
    danger_text: 0x6e1a14,
    warning: 0x7d5600,
    warning_surface: 0xfbefd2,
    warning_text: 0x4d3600,
    flash: 0x0000001a,
    mark: 0x2f6fde2e,
};

/// The skin for the configured scheme, chosen once at startup like the
/// daemon's (see `main`).
pub(crate) static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

/// The window's skin.
pub(crate) fn for_window(_window: &Window) -> &'static Theme {
    THEME.get_or_init(|| for_scheme(terminal_core::schemes::default()))
}

fn hex(c: terminal_core::Rgb) -> u32 {
    (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
}

/// `a` moved toward `b` by `t` (0 to 1), per channel.
fn mix(a: u32, b: u32, t: f64) -> u32 {
    let ch = |c: u32, s: u32| f64::from((c >> s) & 0xff);
    let m = |s: u32| ((ch(a, s) * (1.0 - t) + ch(b, s) * t).round() as u32).min(255) << s;
    m(16) | m(8) | m(0)
}

/// WCAG 2 contrast ratio between two sRGB colors.
pub(crate) fn contrast(a: u32, b: u32) -> f64 {
    fn lum(c: u32) -> f64 {
        let ch = |shift: u32| {
            let v = f64::from((c >> shift) & 0xff) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
    }
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `fg` softened toward `bg` by up to `t`, but never below 4.5:1 against
/// any of `grounds` (or the scheme's own contrast, if that is lower).
fn readable(fg: u32, bg: u32, t: f64, grounds: &[u32]) -> u32 {
    let floor = grounds
        .iter()
        .map(|g| contrast(fg, *g))
        .fold(f64::MAX, f64::min)
        .min(4.5);
    let mut k = t;
    loop {
        let c = mix(fg, bg, k);
        if k <= 0.0 || grounds.iter().all(|g| contrast(c, *g) >= floor) {
            return c;
        }
        k = (k - 0.02).max(0.0);
    }
}

/// The skin for `scheme`: Dreamer's own pair as designed, any other scheme
/// derived from its colors.
pub(crate) fn for_scheme(scheme: &Scheme) -> Theme {
    match scheme.name {
        "dreamer-dark" => return DARK,
        "dreamer-light" => return LIGHT,
        _ => {}
    }
    let light = scheme.appearance == terminal_core::schemes::Appearance::Light;
    let bg = hex(scheme.background);
    let fg = hex(scheme.foreground);
    let ansi = |i: usize| hex(scheme.ansi[i]);
    // A surface `t` of the way toward the text, but never so far that the
    // text on it drops below what the scheme itself gives (at most 4.5:1).
    let floor = contrast(fg, bg).min(4.5);
    let step = |t: f64| {
        let mut k = t;
        while k > 0.0 && contrast(fg, mix(bg, fg, k)) < floor {
            k = (k - 0.01).max(0.0);
        }
        mix(bg, fg, k)
    };
    let (surface, raised, divider, selected) = if light {
        (step(0.04), mix(bg, 0xffffff, 0.6), step(0.12), step(0.10))
    } else {
        (step(0.03), step(0.07), step(0.08), step(0.13))
    };
    let grounds = [bg, surface, raised, selected];
    // The scheme's blue: a cursor color is often pale or close to the
    // background, which reads poorly as an accent line.
    let accent = if light { ansi(4) } else { ansi(12) };
    let danger = ansi(1);
    let warning = ansi(3);
    Theme {
        name: scheme.name,
        background: bg,
        surface,
        raised,
        divider,
        selected,
        text: fg,
        muted: readable(fg, bg, 0.25, &grounds),
        faint: readable(fg, bg, 0.4, &grounds),
        accent,
        focus: mix(bg, accent, 0.5),
        danger: readable(danger, bg, 0.0, &[raised]),
        danger_surface: mix(bg, danger, 0.18),
        danger_text: readable(fg, danger, 0.0, &[mix(bg, danger, 0.18)]),
        warning: readable(warning, bg, 0.0, &[surface]),
        warning_surface: mix(bg, warning, 0.16),
        warning_text: readable(fg, warning, 0.0, &[mix(bg, warning, 0.16)]),
        flash: (if light { 0x0000_0000 } else { 0xffff_ff00 }) | 0x22,
        mark: (accent << 8) | 0x33,
    }
}

pub(crate) fn c(hex: u32) -> Rgba {
    rgb(hex)
}

pub(crate) fn ca(hex: u32) -> Rgba {
    rgba(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_skins_keep_readable_text_on_every_surface() {
        for t in [DARK, LIGHT] {
            for (fg, name) in [(t.text, "text"), (t.muted, "muted"), (t.faint, "faint")] {
                for (bg, where_) in [
                    (t.background, "background"),
                    (t.surface, "surface"),
                    (t.raised, "raised"),
                    (t.selected, "selected"),
                ] {
                    let r = contrast(fg, bg);
                    assert!(r >= 4.5, "{}: {name} on {where_} is {r:.2}:1", t.name);
                }
            }
            for (fg, bg, name) in [
                (t.danger_text, t.danger_surface, "danger text"),
                (t.warning_text, t.warning_surface, "warning text"),
                (t.danger, t.raised, "kill label in the palette"),
                (t.warning, t.surface, "status in the tab strip"),
            ] {
                let r = contrast(fg, bg);
                assert!(r >= 4.5, "{}: {name} is {r:.2}:1", t.name);
            }
        }
    }

    #[test]
    fn the_light_skin_is_designed_not_inverted() {
        let invert = |c: u32| 0xffffff - c;
        let pairs = [
            (DARK.background, LIGHT.background),
            (DARK.surface, LIGHT.surface),
            (DARK.accent, LIGHT.accent),
            (DARK.text, LIGHT.text),
        ];
        assert!(pairs.iter().all(|(d, l)| invert(*d) != *l));
    }

    #[test]
    fn every_scheme_gets_chrome_drawn_from_its_own_colors() {
        for scheme in terminal_core::schemes::all() {
            let t = for_scheme(scheme);
            assert_eq!(t.background, hex(scheme.background), "{}", scheme.name);
            // Chrome text is as readable as the scheme's own text allows.
            let own = contrast(hex(scheme.foreground), hex(scheme.background)).min(4.5);
            for (fg, name) in [(t.text, "text"), (t.muted, "muted"), (t.faint, "faint")] {
                for bg in [t.background, t.surface, t.raised, t.selected] {
                    let r = contrast(fg, bg);
                    assert!(
                        r + 0.5 >= own,
                        "{}: {name} is {r:.2}:1 (scheme text {own:.2}:1)",
                        scheme.name
                    );
                }
            }
        }
    }
}
