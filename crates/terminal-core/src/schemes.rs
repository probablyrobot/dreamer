//! Built-in terminal color schemes: a terminal's default foreground,
//! background, cursor, and 16 ANSI colors (the rest of the 256-color
//! palette is the standard xterm cube and gray ramp).
//!
//! A terminal's colors are canonical state (spec 14.2): the daemon applies
//! the configured scheme to its mirrors, and views get it through
//! checkpoints. Two schemes are Dreamer's own, designed with the chrome
//! skins; the rest come from open-source projects (see `schemes/SOURCES.md`
//! and `THIRD_PARTY_NOTICES.md`), generated into `schemes_data.rs` by
//! `cargo xtask schemes`.

use crate::Rgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    /// The name settings use: lowercase, words joined by `-`.
    pub name: &'static str,
    /// For people: "Catppuccin Mocha".
    pub title: &'static str,
    pub appearance: Appearance,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub selection: Option<Rgb>,
    /// Black, red, green, yellow, blue, magenta, cyan, white; then the
    /// bright eight.
    pub ansi: [Rgb; 16],
}

pub(crate) const fn rgb(hex: u32) -> Rgb {
    Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

/// The default, designed with the dark chrome skin.
pub const DREAMER_DARK: Scheme = Scheme {
    name: "dreamer-dark",
    title: "Dreamer Dark",
    appearance: Appearance::Dark,
    foreground: rgb(0xe6e6e6),
    background: rgb(0x14161a),
    cursor: Some(rgb(0x8ab4f8)),
    selection: Some(rgb(0x2f3b52)),
    ansi: [
        rgb(0x2a2f38),
        rgb(0xf28b82),
        rgb(0x81c995),
        rgb(0xfdd663),
        rgb(0x8ab4f8),
        rgb(0xd7aefb),
        rgb(0x78d9ec),
        rgb(0xc4c7c5),
        rgb(0x5f6368),
        rgb(0xf6aea9),
        rgb(0xa8dab5),
        rgb(0xfde293),
        rgb(0xaecbfa),
        rgb(0xe9d2fd),
        rgb(0xa1e4f2),
        rgb(0xf1f3f4),
    ],
};

/// Designed with the light chrome skin: warm paper, deeper accents.
pub const DREAMER_LIGHT: Scheme = Scheme {
    name: "dreamer-light",
    title: "Dreamer Light",
    appearance: Appearance::Light,
    foreground: rgb(0x1f2328),
    background: rgb(0xf6f6f4),
    cursor: Some(rgb(0x2f6fde)),
    selection: Some(rgb(0xcfdcf3)),
    ansi: [
        rgb(0x1f2328),
        rgb(0xb3261e),
        rgb(0x1e7a3c),
        rgb(0x7d5600),
        rgb(0x2f6fde),
        rgb(0x8a3ab9),
        rgb(0x0b7285),
        rgb(0x6e7781),
        rgb(0x57606a),
        rgb(0xd1242f),
        rgb(0x2a8f4f),
        rgb(0x9a6700),
        rgb(0x4a86e8),
        rgb(0xa250d6),
        rgb(0x138a9e),
        rgb(0x8c959f),
    ],
};

/// The scheme used when none is configured.
pub const DEFAULT: &Scheme = &DREAMER_DARK;

/// Every built-in scheme: Dreamer's two first, then the vendored ones by
/// name.
pub fn all() -> impl Iterator<Item = &'static Scheme> {
    [&DREAMER_DARK, &DREAMER_LIGHT]
        .into_iter()
        .chain(crate::schemes_data::VENDORED.iter())
}

/// The built-in scheme called `name` (case-insensitive; `_` and spaces
/// count as `-`).
pub fn find(name: &str) -> Option<&'static Scheme> {
    let want: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '_' || c == ' ' { '-' } else { c })
        .collect();
    all().find(|s| s.name == want)
}

impl Scheme {
    /// The full 256-color palette: this scheme's 16, then the standard
    /// 6x6x6 color cube and 24-step gray ramp.
    pub fn palette(&self) -> [Rgb; 256] {
        let mut p = [Rgb::default(); 256];
        p[..16].copy_from_slice(&self.ansi);
        let level = |i: usize| if i == 0 { 0 } else { (55 + 40 * i) as u8 };
        for i in 0..216 {
            p[16 + i] = Rgb {
                r: level(i / 36),
                g: level((i / 6) % 6),
                b: level(i % 6),
            };
        }
        for i in 0..24 {
            let v = (8 + 10 * i) as u8;
            p[232 + i] = Rgb { r: v, g: v, b: v };
        }
        p
    }
}

/// WCAG 2 contrast ratio.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    fn lum(c: Rgb) -> f64 {
        let ch = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    }
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scheme_is_named_uniquely_and_its_text_is_readable() {
        let all: Vec<&Scheme> = all().collect();
        assert!(all.len() >= 30, "{} schemes", all.len());
        for s in &all {
            assert_eq!(
                all.iter().filter(|o| o.name == s.name).count(),
                1,
                "{}",
                s.name
            );
            assert!(
                s.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}",
                s.name
            );
            let r = contrast(s.foreground, s.background);
            assert!(r >= 4.5, "{}: foreground on background is {r:.2}:1", s.name);
            let dark = contrast(s.background, Rgb { r: 0, g: 0, b: 0 })
                < contrast(
                    s.background,
                    Rgb {
                        r: 255,
                        g: 255,
                        b: 255,
                    },
                );
            assert_eq!(dark, s.appearance == Appearance::Dark, "{}", s.name);
        }
    }

    #[test]
    fn names_are_found_loosely_and_the_palette_is_xterm_beyond_sixteen() {
        assert_eq!(
            find("Catppuccin_Mocha").map(|s| s.name),
            Some("catppuccin-mocha")
        );
        assert_eq!(find(" dreamer-dark ").map(|s| s.name), Some("dreamer-dark"));
        assert!(find("no-such-scheme").is_none());
        let p = DREAMER_DARK.palette();
        assert_eq!(p[1], DREAMER_DARK.ansi[1]);
        assert_eq!(p[16], Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(p[21], Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(
            p[231],
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(p[232], Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            p[255],
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }
}
