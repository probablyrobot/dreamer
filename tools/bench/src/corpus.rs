//! The byte corpora the `streams` workload replays. Each is generated from
//! a fixed seed by the code below, so every run replays the same bytes
//! without a large file in the repository or anything from the network.
//! `tests::corpora_are_fixed` pins each corpus by length and hash; a change
//! to a generator has to update those on purpose.
//!
//! Every corpus is written for the harness's 120x40 panes and replays in a
//! loop, so each starts from a known state.

/// Columns and rows of the panes the corpora are written for.
pub const COLS: usize = 120;
pub const ROWS: usize = 40;

/// Kitty graphics payload per APC chunk, as the protocol recommends.
const KITTY_CHUNK: usize = 4096;

pub const NAMES: [&str; 4] = ["text", "sgr", "altscreen", "images"];

/// What each corpus stands for, for the output.
pub fn describe(name: &str) -> &'static str {
    match name {
        "text" => "plain text: log lines, some wrapping, no escapes",
        "sgr" => "TUI redraw: cursor motion and 24-bit SGR colour on every few cells",
        "altscreen" => {
            "alternate screen: full redraws inside synchronized output (mode 2026), with scroll regions"
        }
        "images" => {
            "Kitty graphics: raw RGBA images 64 to 384 px, transmitted in 4 KiB chunks and placed, at most 24 stored (under 14 MiB of the 64 MiB budget)"
        }
        _ => "",
    }
}

pub fn generate(name: &str) -> Option<Vec<u8>> {
    let mut r = Rng(0x5eed_d4ea_4e72_0001 ^ fnv(name.as_bytes()));
    Some(match name {
        "text" => text(&mut r),
        "sgr" => sgr(&mut r),
        "altscreen" => altscreen(&mut r),
        "images" => images(&mut r),
        _ => return None,
    })
}

/// xorshift64*: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `lo..hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo) as u64) as usize
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.range(0, from.len())]
    }

    fn pick_side(&mut self, from: &[usize]) -> usize {
        from[self.range(0, from.len())]
    }
}

/// FNV-1a, 64-bit.
pub fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

const WORDS: &[&str] = &[
    "request", "handler", "session", "cache", "miss", "hit", "worker", "queue", "flush", "commit",
    "index", "segment", "retry", "upstream", "timeout", "socket", "buffer", "parse", "render",
    "layout", "compile", "link", "test", "passed", "warning", "unused", "variable", "module",
];

/// About 2 MiB of log lines; a few are longer than a row and wrap.
fn text(r: &mut Rng) -> Vec<u8> {
    let levels = ["INFO", "INFO", "INFO", "DEBUG", "WARN", "ERROR"];
    let mut out = Vec::with_capacity(2 << 20);
    let mut n = 0u64;
    while out.len() < 2 << 20 {
        n += 1;
        let mut line = format!(
            "2026-11-21T{:02}:{:02}:{:02}.{:03}Z {:<5} app::{}::{} id={:08x}",
            (n / 3_600_000) % 24,
            (n / 60_000) % 60,
            (n / 1000) % 60,
            n % 1000,
            r.pick(&levels),
            r.pick(WORDS),
            r.pick(WORDS),
            r.next() as u32
        );
        let words = if r.range(0, 10) == 0 {
            r.range(20, 40)
        } else {
            r.range(2, 12)
        };
        for _ in 0..words {
            line.push(' ');
            line.push_str(r.pick(WORDS));
        }
        line.push_str(&format!(" took {}ms\r\n", r.range(0, 2000)));
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn truecolor(r: &mut Rng, out: &mut Vec<u8>) {
    let c = r.next();
    out.extend_from_slice(
        format!(
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            c & 255,
            (c >> 8) & 255,
            (c >> 16) & 255,
            (c >> 24) & 63,
            (c >> 32) & 63,
            (c >> 40) & 63
        )
        .as_bytes(),
    );
}

const GLYPHS: &[&str] = &[
    "a", "e", "o", "s", "t", "0", "1", "7", "#", "-", "|", "─", "│", "█", "▌", "░", "▒", "•",
];

/// One screen of a TUI redraw: every row positioned, runs of 1 to 12 cells
/// in their own 24-bit colours, now and then bold or underlined, some
/// cells skipped with cursor-forward, and the rest of the row erased.
fn screen(r: &mut Rng, out: &mut Vec<u8>) {
    for row in 1..=ROWS {
        out.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        let mut col = 0;
        while col < COLS - 12 {
            let run = r.range(1, 13);
            match r.range(0, 8) {
                0 => out.extend_from_slice(b"\x1b[1m"),
                1 => out.extend_from_slice(b"\x1b[4m"),
                2 => {
                    out.extend_from_slice(format!("\x1b[{run}C").as_bytes());
                    col += run;
                    continue;
                }
                _ => {}
            }
            truecolor(r, out);
            for _ in 0..run {
                out.extend_from_slice(r.pick(GLYPHS).as_bytes());
            }
            out.extend_from_slice(b"\x1b[0m");
            col += run;
        }
        out.extend_from_slice(b"\x1b[K");
    }
}

/// About 2 MiB of full-screen redraws on the primary screen, as a
/// monitor such as `btop` draws them.
fn sgr(r: &mut Rng) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 << 20);
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J\x1b[?25l");
    while out.len() < 2 << 20 {
        screen(r, &mut out);
    }
    out.extend_from_slice(b"\x1b[0m\x1b[?25h");
    out
}

/// About 2 MiB on the alternate screen: each frame inside a synchronized
/// update, some frames a full redraw and some a scroll of a region with
/// new lines written at its bottom, as a pager or editor does.
fn altscreen(r: &mut Rng) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 << 20);
    out.extend_from_slice(b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J");
    while out.len() < 2 << 20 {
        out.extend_from_slice(b"\x1b[?2026h");
        if r.range(0, 3) == 0 {
            let top = r.range(2, 10);
            let bottom = r.range(ROWS - 8, ROWS);
            let n = r.range(1, 6);
            out.extend_from_slice(format!("\x1b[{top};{bottom}r\x1b[{n}S").as_bytes());
            for k in 0..n {
                out.extend_from_slice(format!("\x1b[{};1H", bottom - k).as_bytes());
                truecolor(r, &mut out);
                for _ in 0..r.range(20, COLS) {
                    out.extend_from_slice(r.pick(GLYPHS).as_bytes());
                }
                out.extend_from_slice(b"\x1b[0m\x1b[K");
            }
            out.extend_from_slice(b"\x1b[r");
        } else {
            out.extend_from_slice(b"\x1b[H");
            screen(r, &mut out);
        }
        out.extend_from_slice(b"\x1b[?2026l");
    }
    out.extend_from_slice(b"\x1b[0m\x1b[?25h\x1b[?1049l");
    out
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Image ids in use at once; each id is replaced when it comes round again.
pub const IMAGE_IDS: usize = 24;
/// The largest image side, in pixels.
pub const IMAGE_MAX_SIDE: usize = 384;

/// About 9 MiB: 40 RGBA images, each transmitted in 4 KiB chunks and
/// placed at a random cell without moving the cursor, a few earlier ones
/// placed again, and lines of text between them. Every loop starts by
/// deleting all images, so at most [`IMAGE_IDS`] are stored.
fn images(r: &mut Rng) -> Vec<u8> {
    let sides = [64, 128, 256, IMAGE_MAX_SIDE];
    let mut out = Vec::with_capacity(11 << 20);
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[0m\x1b[H\x1b[2J");
    for k in 0..40 {
        let id = k % IMAGE_IDS + 1;
        let (w, h) = (r.pick_side(&sides), r.pick_side(&sides));
        // Bands of colour with noise, so neighbouring pixels differ.
        let mut px = Vec::with_capacity(w * h * 4);
        let base = r.next();
        for y in 0..h {
            for _ in 0..w {
                let n = r.next();
                px.extend_from_slice(&[
                    ((base as usize + y) & 255) as u8 ^ (n as u8 & 15),
                    ((base >> 8) as u8).wrapping_add((n >> 8) as u8 & 31),
                    ((base >> 16) as u8) ^ (y as u8),
                    255,
                ]);
            }
        }
        let row = r.range(1, ROWS - 2);
        let col = r.range(1, COLS - 10);
        out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
        let b64 = base64(&px);
        let mut chunks = b64.as_bytes().chunks(KITTY_CHUNK).peekable();
        let mut first = true;
        while let Some(c) = chunks.next() {
            let more = u8::from(chunks.peek().is_some());
            if first {
                out.extend_from_slice(
                    format!("\x1b_Ga=T,f=32,s={w},v={h},i={id},q=2,C=1,m={more};").as_bytes(),
                );
                first = false;
            } else {
                out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
            }
            out.extend_from_slice(c);
            out.extend_from_slice(b"\x1b\\");
        }
        if k >= IMAGE_IDS / 2 && r.range(0, 3) == 0 {
            let again = r.range(1, k.min(IMAGE_IDS) + 1);
            out.extend_from_slice(
                format!(
                    "\x1b[{};{}H\x1b_Ga=p,i={again},q=2,C=1\x1b\\",
                    r.range(1, ROWS - 2),
                    r.range(1, COLS - 10)
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(
            format!(
                "\x1b[{ROWS};1H\x1b[0m{} {} {}\x1b[K",
                r.pick(WORDS),
                r.pick(WORDS),
                k
            )
            .as_bytes(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_ghostty::core::GridSize;
    use terminal_ghostty::{Engine, EngineConfig};

    #[test]
    fn corpora_are_fixed() {
        let got: Vec<(&str, usize, u64)> = NAMES
            .iter()
            .map(|n| {
                let b = generate(n).expect("corpus");
                (*n, b.len(), fnv(&b))
            })
            .collect();
        assert_eq!(got, PINNED, "a corpus changed; update PINNED on purpose");
    }

    const PINNED: [(&str, usize, u64); 4] = [
        ("text", 2097184, 0x913aa84bd4b9b277),
        ("sgr", 2101447, 0x156558c480786bec),
        ("altscreen", 2116644, 0x43695fa20c33789f),
        ("images", 9942913, 0x9225dcbf13876481),
    ];

    /// The images corpus really stores and places images in a mirror's
    /// engine, none skipped, and stays far inside the per-pane budget.
    #[test]
    fn the_images_corpus_places_images_within_the_budget() {
        let size = GridSize {
            cols: COLS as u16,
            rows: ROWS as u16,
        };
        let mut e = Engine::new(EngineConfig::new(size)).expect("engine");
        e.resize(size, terminal_ghostty::core::family::CANONICAL_CELL_PX)
            .expect("cell size");
        let bytes = generate("images").expect("corpus");
        for _ in 0..2 {
            for c in bytes.chunks(64 << 10) {
                e.feed(c).expect("feed");
            }
        }
        let f = e.frame().expect("frame");
        assert!(f.images.len() >= IMAGE_IDS / 2, "{} placed", f.images.len());
        assert_eq!(e.images_skipped(), 0);
        let stored = IMAGE_IDS * IMAGE_MAX_SIDE * IMAGE_MAX_SIDE * 4;
        assert!(stored as u64 <= terminal_ghostty::core::family::IMAGE_STORAGE_BYTES / 4);
    }

    #[test]
    fn synchronized_updates_are_balanced() {
        let b = generate("altscreen").expect("corpus");
        let count = |p: &[u8]| b.windows(p.len()).filter(|w| *w == p).count();
        let begin = count(b"\x1b[?2026h");
        assert!(begin > 100, "{begin} frames");
        assert_eq!(begin, count(b"\x1b[?2026l"));
    }
}
