//! Terminal color schemes: a terminal's default foreground, background,
//! cursor, and 16 ANSI colors (the rest of the 256-color palette is the
//! standard xterm cube and gray ramp).
//!
//! Schemes are files, as in other terminals. The built-in ones are the
//! files in `schemes/`, compiled into the program (`schemes_data.rs`, from
//! `cargo xtask schemes`), so they always ship with it: Dreamer's own two,
//! designed with the chrome, and a selection from open-source projects (see
//! `schemes/SOURCES.md` and `THIRD_PARTY_NOTICES.md`). [`parse`] reads the
//! same format, alacritty-theme TOML, and Ghostty's theme files, so users
//! can add any scheme published for either (see `workspace-config`).
//!
//! A terminal's colors are canonical state (spec 14.2): the daemon applies
//! the configured scheme to its mirrors, and views get it through
//! checkpoints.

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
    /// Chrome colors the file sets itself, by token (see [`CHROME`]); the
    /// rest are derived from the colors above.
    pub chrome: &'static [(&'static str, Rgb)],
}

/// The desktop chrome's color tokens a scheme file may set: the window
/// behind everything, tab strip and sidebar, floating layers, hairlines,
/// selected rows, three levels of text, the accent, the focused pane's
/// edge, and danger and warning with their surfaces and text.
pub const CHROME: [&str; 16] = [
    "background",
    "surface",
    "raised",
    "divider",
    "selected",
    "text",
    "muted",
    "faint",
    "accent",
    "focus",
    "danger",
    "danger_surface",
    "danger_text",
    "warning",
    "warning_surface",
    "warning_text",
];

pub const fn rgb(hex: u32) -> Rgb {
    Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

/// The built-in schemes, parsed once: Dreamer's two first, then the others
/// by name.
fn builtin() -> &'static [Scheme] {
    static ALL: std::sync::OnceLock<Vec<Scheme>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<Scheme> = crate::schemes_data::FILES
            .iter()
            .map(|(name, text)| {
                // The files are checked by tests and by `cargo xtask schemes`.
                parse(name, text).unwrap_or_else(|e| panic!("built-in scheme {name}: {e}"))
            })
            .collect();
        all.sort_by_key(|s| (!s.name.starts_with("dreamer-"), s.name));
        all
    })
}

/// Dreamer's dark scheme, designed with the dark chrome.
pub fn dreamer_dark() -> &'static Scheme {
    find("dreamer-dark").expect("built in")
}

/// Dreamer's light scheme, designed with the light chrome.
pub fn dreamer_light() -> &'static Scheme {
    find("dreamer-light").expect("built in")
}

/// The scheme used when none is configured.
pub fn default() -> &'static Scheme {
    dreamer_dark()
}

/// Every built-in scheme: Dreamer's two first, then the others by name.
pub fn all() -> impl Iterator<Item = &'static Scheme> {
    builtin().iter()
}

/// A scheme name as settings use it: lowercase, words joined by `-` (`_`
/// and spaces count as `-`).
pub fn normalize(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '_' || c == ' ' { '-' } else { c })
        .collect()
}

/// The built-in scheme called `name` (normalized).
pub fn find(name: &str) -> Option<&'static Scheme> {
    let want = normalize(name);
    all().find(|s| s.name == want)
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// "catppuccin_mocha" as "Catppuccin Mocha".
fn title(name: &str) -> String {
    name.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `#rrggbb`, `rrggbb`, quoted or not.
fn color(value: &str) -> Option<Rgb> {
    let v = value.trim().trim_matches(['\'', '"']).trim();
    let hex = v.strip_prefix('#').unwrap_or(v);
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
        .map(rgb)
}

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

/// Read a scheme file called `name` (its file name without the
/// extension): alacritty-theme TOML (`[colors.primary]` and the rest), a
/// Ghostty theme (`background = …`, `palette = 0=#…`), or a kitty theme
/// (`background #…`, `color0 #…`). Any of them may also set chrome colors
/// ([`CHROME`]). Only the colors a
/// terminal's defaults need are read; other keys are ignored. Errors give
/// the line.
pub fn parse(name: &str, text: &str) -> Result<Scheme, String> {
    let mut fg = None;
    let mut bg = None;
    let mut cursor = None;
    let mut selection = None;
    let mut ansi: [Option<Rgb>; 16] = [None; 16];
    let mut chrome: Vec<(&'static str, Rgb)> = Vec::new();
    let mut section = String::new();
    for (n, raw) in text.lines().enumerate() {
        let at = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).trim().to_owned();
            continue;
        }
        // kitty writes `key value`; the others `key = value`.
        let (key, value) = match line.split_once('=') {
            Some((k, v)) if !k.trim().contains(char::is_whitespace) => (k, v),
            _ if section.is_empty() => line
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("line {at}: not `key = value` or `key value`"))?,
            _ => return Err(format!("line {at}: not `key = value`")),
        };
        let key = key.trim().trim_matches(['\'', '"']);
        // A comment after the value: ` #` past its first character (the
        // value itself may start with `#`).
        let value = value.trim();
        let value = match value.get(1..).and_then(|v| v.find(" #")) {
            Some(i) => &value[..=i],
            None => value,
        };
        let need =
            |v: &str| color(v).ok_or_else(|| format!("line {at}: `{}` is not a color", v.trim()));
        // Chrome colors: `[dreamer.chrome]` in TOML, `dreamer-chrome-TOKEN`
        // (Ghostty) or `dreamer_chrome_TOKEN` (kitty) elsewhere.
        let token = match section.as_str() {
            "dreamer.chrome" => Some(key),
            "" => key
                .strip_prefix("dreamer-chrome-")
                .or_else(|| key.strip_prefix("dreamer_chrome_")),
            _ => None,
        };
        if let Some(token) = token {
            let token = token.replace('-', "_");
            let Some(known) = CHROME.iter().find(|t| **t == token) else {
                return Err(format!(
                    "line {at}: no chrome color `{token}`; there are: {}",
                    CHROME.join(", ")
                ));
            };
            chrome.push((*known, need(value)?));
            continue;
        }
        match (section.as_str(), key) {
            // kitty
            ("", k) if k.starts_with("color") && k[5..].parse::<usize>().is_ok() => {
                let i: usize = k[5..].parse().unwrap_or(99);
                if i < 16 {
                    ansi[i] = Some(need(value)?);
                }
            }
            ("", "cursor") => cursor = color(value),
            ("", "selection_background") => selection = color(value),
            // Ghostty
            ("", "palette") => {
                let (i, c) = value
                    .split_once('=')
                    .ok_or_else(|| format!("line {at}: palette needs `N=#rrggbb`"))?;
                let i: usize = i
                    .trim()
                    .parse()
                    .map_err(|_| format!("line {at}: palette index `{}`", i.trim()))?;
                if i < 16 {
                    ansi[i] = Some(need(c)?);
                }
            }
            ("", "background") | ("colors.primary", "background") => bg = Some(need(value)?),
            ("", "foreground") | ("colors.primary", "foreground") => fg = Some(need(value)?),
            ("", "cursor-color") => cursor = color(value),
            ("", "selection-background") => selection = color(value),
            // alacritty-theme
            ("colors.cursor", "cursor") => cursor = color(value),
            ("colors.selection", "background") => selection = color(value),
            ("colors.normal" | "colors.bright", k) => {
                if let Some(i) = ANSI_NAMES.iter().position(|n| *n == k) {
                    let i = if section == "colors.bright" { i + 8 } else { i };
                    ansi[i] = Some(need(value)?);
                }
            }
            _ => {}
        }
    }
    let foreground = fg.ok_or("no foreground color")?;
    let background = bg.ok_or("no background color")?;
    let mut colors = [Rgb::default(); 16];
    for (i, c) in ansi.iter().enumerate() {
        colors[i] = c.ok_or_else(|| format!("no color {i} ({})", ANSI_NAMES[i % 8]))?;
    }
    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    let black = Rgb { r: 0, g: 0, b: 0 };
    let appearance = if contrast(background, black) < contrast(background, white) {
        Appearance::Dark
    } else {
        Appearance::Light
    };
    let name = normalize(name);
    Ok(Scheme {
        title: leak(title(&name)),
        name: leak(name),
        appearance,
        foreground,
        background,
        cursor,
        selection,
        ansi: colors,
        chrome: Box::leak(chrome.into_boxed_slice()),
    })
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
        let p = dreamer_dark().palette();
        assert_eq!(p[1], dreamer_dark().ansi[1]);
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

    #[test]
    fn dreamers_own_schemes_read_from_their_files_as_designed() {
        let d = dreamer_dark();
        assert_eq!((d.background, d.foreground), (rgb(0x14161a), rgb(0xe6e6e6)));
        assert_eq!(
            (d.cursor, d.selection),
            (Some(rgb(0x8ab4f8)), Some(rgb(0x2f3b52)))
        );
        assert_eq!(d.title, "Dreamer Dark");
        let l = dreamer_light();
        assert_eq!((l.background, l.ansi[2]), (rgb(0xf6f6f4), rgb(0x1e7a3c)));
        assert_eq!(l.appearance, Appearance::Light);
        assert_eq!(all().next().map(|s| s.name), Some("dreamer-dark"));
    }

    #[test]
    fn a_ghostty_theme_reads_like_an_alacritty_one() {
        let mut g = String::new();
        for i in 0..16 {
            g.push_str(&format!("palette = {i}=#{:02x}{:02x}{:02x}\n", i, i, i));
        }
        g.push_str("background = 101010 # a note\nforeground = #f0f0f0\n");
        let s = parse("My Theme", &g).expect("ghostty");
        assert_eq!((s.name, s.title), ("my-theme", "My Theme"));
        assert_eq!(s.background, rgb(0x101010));
        assert_eq!(s.ansi[15], rgb(0x0f0f0f));
        let e = parse("x", "background = #000000\n").expect_err("incomplete");
        assert!(e.contains("foreground"), "{e}");
        let e = parse("x", "foreground = nope\n").expect_err("bad color");
        assert!(e.contains("line 1"), "{e}");
    }

    #[test]
    fn a_kitty_theme_reads_and_any_format_may_set_chrome_colors() {
        let mut k = String::from("# kitty\nbackground #202020\nforeground   #e0e0e0\n");
        for i in 0..16 {
            k.push_str(&format!("color{i} #{:02x}0000\n", i * 8));
        }
        k.push_str("dreamer_chrome_accent #ff8800\n");
        let s = parse("kitty one", &k).expect("kitty");
        assert_eq!(s.background, rgb(0x202020));
        assert_eq!(s.ansi[2], rgb(0x100000));
        assert_eq!(s.chrome, &[("accent", rgb(0xff8800))]);

        let toml = "[colors.primary]\nbackground = '#000000'\nforeground = '#ffffff'\n\
                    [dreamer.chrome]\nsurface = '#111111'\nwarning_text = '#eeeeee'\n";
        let mut toml = toml.to_owned();
        for (section, base) in [("normal", 0), ("bright", 8)] {
            toml.push_str(&format!("[colors.{section}]\n"));
            for (i, n) in ANSI_NAMES.iter().enumerate() {
                toml.push_str(&format!("{n} = '#0000{:02x}'\n", base + i));
            }
        }
        let s = parse("t", &toml).expect("toml");
        assert_eq!(
            s.chrome,
            &[("surface", rgb(0x111111)), ("warning_text", rgb(0xeeeeee))]
        );
        // A chrome token that does not exist is an error, not ignored.
        let e = parse(
            "t",
            &format!("{toml}[dreamer.chrome]\naccnet = '#000000'\n"),
        )
        .expect_err("typo");
        assert!(e.contains("accnet"), "{e}");
    }
}
