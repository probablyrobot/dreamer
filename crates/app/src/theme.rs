//! Design tokens for the desktop's chrome (spec 14.2, 14.4): two default
//! skins designed together, a dark and a light one, neither an inversion of
//! the other. Terminal text is the brightest, sharpest thing on screen;
//! chrome stays lower in contrast with one accent color.
//!
//! These are chrome colors only. A terminal's own default colors and
//! palette are canonical terminal state and change only through an
//! explicit, owner-authorized configuration (spec 14.2); they are not part
//! of a skin here.

use gpui::{Rgba, Window, WindowAppearance, rgb, rgba};

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

/// Which skin the window uses: `appearance` from the settings (`system`
/// follows the OS, live), and the window's current appearance.
pub(crate) fn for_window(window: &Window) -> &'static Theme {
    use workspace_config::Appearance;
    let setting = crate::APPEARANCE.get().copied().unwrap_or_default();
    let dark = match setting {
        Appearance::Dark => true,
        Appearance::Light => false,
        Appearance::System => matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    if dark { &DARK } else { &LIGHT }
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

    /// WCAG 2 contrast ratio between two sRGB colors.
    fn contrast(a: u32, b: u32) -> f64 {
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
}
