//! A pane's screen kept from the daemon's screen frames (the `frames`
//! feed, `docs/mirrors.md`): no engine, only the rows, cursor, modes,
//! colors, and images the frames carry.
//!
//! A full frame replaces everything. Any other frame applies only on top
//! of the frame numbered one less; a frame that does not is refused and
//! the view is left as it was, so it never shows rows over a hole.
//!
//! [`ScreenView::frame`] gives the renderer the same [`FrameSnapshot`] an
//! engine would for the same screen: rows shared while unchanged, the rows
//! damaged since the last call, and the view state input policy reads
//! (mouse tracking and the alternate screen from the frame's modes, `top`
//! as the viewport's offset, so decorations land on the same lines).

use std::collections::HashMap;
use std::sync::Arc;

use terminal_ghostty::core::{
    Attrs, Cell, CellWidth, Color, Colors, Cursor, CursorShape, FrameSnapshot, GridSize,
    ImageLayer, ImagePixels, ImagePlacement, InputModes, Rgb, Row, Style, Underline, ViewState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenError {
    /// A frame that is not full arrived when `expected` was next.
    Gap { expected: u64, got: u64 },
    /// The frame does not decode (a row longer than the grid, a style
    /// that is not in the table, text that runs out).
    Malformed(String),
}

impl std::fmt::Display for ScreenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenError::Gap { expected, got } => {
                write!(f, "frame {got} arrived when {expected} was next")
            }
            ScreenError::Malformed(why) => write!(f, "malformed frame: {why}"),
        }
    }
}

impl std::error::Error for ScreenError {}

/// Image ids are unique per pane mirror; this tells frame-fed images apart
/// from those of engines in the same process.
static VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 48);

#[derive(Debug, Clone)]
pub struct ScreenView {
    /// The last frame applied; 0 before the first.
    pub number: u64,
    /// The mirror sequence that frame was taken at.
    pub sequence: u64,
    pub size: GridSize,
    /// Row 0's number from the top of the pane's history, as decorations
    /// number lines.
    pub top: u64,
    pub total_rows: u64,
    pub rows: Vec<Arc<Row>>,
    pub cursor: Cursor,
    pub modes: InputModes,
    pub title: Option<String>,
    pub colors: Arc<Colors>,
    pub images: Arc<[ImagePlacement]>,
    pub render_hold: bool,
    pixels: HashMap<(u32, u64), Arc<ImagePixels>>,
    instance: u64,
    /// What the last render frame showed, and what changed since.
    last: Option<Arc<FrameSnapshot>>,
    damaged: Damage,
    revision: u64,
}

/// What changed since the last render frame.
#[derive(Debug, Clone, Default)]
enum Damage {
    /// Nothing: the last frame stands.
    #[default]
    None,
    /// These rows, and maybe the cursor, modes, colors, or images.
    Rows(Vec<u16>),
    All,
}

impl Damage {
    fn rows(&mut self, rows: impl IntoIterator<Item = u16>) {
        match self {
            Damage::All => {}
            Damage::None => *self = Damage::Rows(rows.into_iter().collect()),
            Damage::Rows(r) => r.extend(rows),
        }
    }
}

impl Default for ScreenView {
    fn default() -> Self {
        ScreenView {
            number: 0,
            sequence: 0,
            size: GridSize { cols: 0, rows: 0 },
            top: 0,
            total_rows: 0,
            rows: Vec::new(),
            cursor: Cursor::default(),
            modes: InputModes::default(),
            title: None,
            colors: Arc::default(),
            images: Arc::new([]),
            render_hold: false,
            pixels: HashMap::new(),
            instance: VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            last: None,
            damaged: Damage::None,
            revision: 0,
        }
    }
}

impl ScreenView {
    pub fn new() -> ScreenView {
        ScreenView::default()
    }

    /// Apply one frame. On an error nothing changes.
    pub fn apply(&mut self, f: &api::ScreenFrame) -> Result<(), ScreenError> {
        if !f.full && (self.number == 0 || f.number != self.number + 1) {
            return Err(ScreenError::Gap {
                expected: self.number + 1,
                got: f.number,
            });
        }
        let bad = |why: &str| ScreenError::Malformed(why.to_owned());
        let size = GridSize {
            cols: u16::try_from(f.cols).map_err(|_| bad("cols"))?,
            rows: u16::try_from(f.rows).map_err(|_| bad("rows"))?,
        };
        if !f.full && size != self.size {
            return Err(bad("a new size in a frame that is not full"));
        }
        let styles: Vec<Style> = f.styles.iter().map(style).collect();
        let mut rows = if f.full {
            let blank = Arc::new(blank_row(size.cols));
            vec![blank; usize::from(size.rows)]
        } else {
            self.rows.clone()
        };
        for r in &f.changed {
            let slot = rows
                .get_mut(r.index as usize)
                .ok_or_else(|| bad("a row past the grid"))?;
            *slot = Arc::new(decode_row(r, size.cols, &styles)?);
        }
        let mut pixels = if f.full {
            HashMap::new()
        } else {
            self.pixels.clone()
        };
        for p in &f.pixels {
            pixels.insert(
                (p.image, p.generation),
                Arc::new(ImagePixels {
                    engine: self.instance,
                    id: p.image,
                    generation: p.generation,
                    width: p.width,
                    height: p.height,
                    rgba: p.rgba.clone().into_boxed_slice(),
                }),
            );
        }
        let images = match &f.images {
            Some(i) => {
                let mut placed = Vec::with_capacity(i.placements.len());
                for p in &i.placements {
                    // A placement whose pixels never arrived is not drawn.
                    let Some(image) = pixels.get(&(p.image, p.generation)) else {
                        continue;
                    };
                    placed.push(ImagePlacement {
                        image: Arc::clone(image),
                        layer: match p.layer {
                            0 => ImageLayer::BelowBackground,
                            1 => ImageLayer::BelowText,
                            _ => ImageLayer::AboveText,
                        },
                        z: p.z,
                        col: p.col,
                        row: p.row,
                        offset_x: p.offset_x,
                        offset_y: p.offset_y,
                        width: p.width,
                        height: p.height,
                        source_x: p.source_x,
                        source_y: p.source_y,
                        source_width: p.source_width,
                        source_height: p.source_height,
                    });
                }
                // Pixels no placement uses any more are let go.
                pixels.retain(|k, _| i.placements.iter().any(|p| (p.image, p.generation) == *k));
                placed.into()
            }
            None if f.full => Arc::from([]),
            None => Arc::clone(&self.images),
        };
        let colors = match &f.colors {
            Some(c) => {
                let mut palette = Box::new([Rgb::default(); 256]);
                for (dst, src) in palette.iter_mut().zip(&c.palette) {
                    *dst = rgb(*src);
                }
                Arc::new(Colors {
                    foreground: rgb(c.foreground),
                    background: rgb(c.background),
                    cursor: c.cursor.map(rgb),
                    palette,
                })
            }
            None if f.full => Arc::default(),
            None => Arc::clone(&self.colors),
        };
        let cursor = f.cursor.as_ref().map(|c| Cursor {
            visible: c.visible,
            x: c.x as u16,
            y: c.y as u16,
            shape: match c.shape {
                1 => CursorShape::BlockHollow,
                2 => CursorShape::Bar,
                3 => CursorShape::Underline,
                _ => CursorShape::Block,
            },
            blinking: c.blinking,
            on_wide_tail: c.on_wide_tail,
        });
        let cursor = cursor.unwrap_or_default();
        let modes = f.modes.as_ref().map(modes).unwrap_or_default();
        if f.full {
            self.damaged = Damage::All;
        } else if !f.changed.is_empty()
            || cursor != self.cursor
            || modes != self.modes
            || (f.top, f.total_rows) != (self.top, self.total_rows)
            || f.colors.is_some()
            || f.images.is_some()
            || f.render_hold != self.render_hold
        {
            self.damaged.rows(f.changed.iter().map(|r| r.index as u16));
        }
        // Commit: nothing above changed the view.
        self.number = f.number;
        self.sequence = f.sequence;
        self.size = size;
        self.top = f.top;
        self.total_rows = f.total_rows;
        self.rows = rows;
        self.cursor = cursor;
        self.modes = modes;
        if f.full || f.title.is_some() {
            self.title.clone_from(&f.title);
        }
        self.colors = colors;
        self.images = images;
        self.pixels = pixels;
        self.render_hold = f.render_hold;
        Ok(())
    }

    /// Where the screen is in the pane's history, and the modes a view
    /// decides input by.
    pub fn view_state(&self) -> ViewState {
        ViewState {
            total_rows: self.total_rows,
            offset: self.top,
            len: u64::from(self.size.rows),
            mouse_tracking: self.modes.mouse_tracking(),
            alt_screen: self.modes.alt_screen,
        }
    }

    /// The screen as the renderer draws it, before the first frame `None`.
    /// When nothing changed since the last call, that frame itself.
    pub fn frame(&mut self) -> Option<Arc<FrameSnapshot>> {
        if self.number == 0 {
            return None;
        }
        let damaged_rows = match std::mem::take(&mut self.damaged) {
            Damage::None => {
                if let Some(last) = &self.last {
                    return Some(Arc::clone(last));
                }
                (0..self.size.rows).collect()
            }
            Damage::All => (0..self.size.rows).collect(),
            Damage::Rows(mut r) => {
                r.sort_unstable();
                r.dedup();
                r
            }
        };
        self.revision += 1;
        let frame = Arc::new(FrameSnapshot {
            revision: self.revision,
            size: self.size,
            rows: self.rows.clone(),
            cursor: self.cursor,
            colors: Arc::clone(&self.colors),
            damaged_rows,
            render_hold: self.render_hold,
            view: self.view_state(),
            selection: Vec::new(),
            images: Arc::clone(&self.images),
        });
        self.last = Some(Arc::clone(&frame));
        Some(frame)
    }

    /// The screen's text, one row per line (trailing blanks trimmed).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            out.push_str(&r.text());
            out.push('\n');
        }
        out
    }
}

/// Where two frames would be drawn differently: size, rows (cells and
/// styles), cursor, colors, the hold, view state, selection, and image
/// placements with their pixels. What only says how a frame was made
/// (revision, damage, which engine and generation an image's pixels
/// came from) is not compared; the pixels themselves are.
/// Empty when the renderer would draw both the same.
pub fn differences(a: &FrameSnapshot, b: &FrameSnapshot) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, same: bool, detail: &dyn Fn() -> String| {
        if !same {
            out.push(format!("{what}: {}", detail()));
        }
    };
    check("size", a.size == b.size, &|| {
        format!("{:?} / {:?}", a.size, b.size)
    });
    for (y, (ra, rb)) in a.rows.iter().zip(&b.rows).enumerate() {
        if ra != rb {
            let x = ra
                .cells
                .iter()
                .zip(rb.cells.iter())
                .position(|(ca, cb)| ca != cb)
                .unwrap_or(0);
            check(&format!("row {y}"), false, &|| {
                format!(
                    "{:?} / {:?} (first at column {x}: {:?} / {:?})",
                    ra.text(),
                    rb.text(),
                    ra.cells.get(x),
                    rb.cells.get(x)
                )
            });
        }
    }
    check("rows", a.rows.len() == b.rows.len(), &|| {
        format!("{} / {}", a.rows.len(), b.rows.len())
    });
    check("cursor", a.cursor == b.cursor, &|| {
        format!("{:?} / {:?}", a.cursor, b.cursor)
    });
    check("colors", a.colors == b.colors, &|| {
        format!(
            "fg {:?} bg {:?} / fg {:?} bg {:?}",
            a.colors.foreground, a.colors.background, b.colors.foreground, b.colors.background
        )
    });
    check("render hold", a.render_hold == b.render_hold, &|| {
        format!("{} / {}", a.render_hold, b.render_hold)
    });
    check("view", a.view == b.view, &|| {
        format!("{:?} / {:?}", a.view, b.view)
    });
    check("selection", a.selection == b.selection, &|| {
        format!("{:?} / {:?}", a.selection, b.selection)
    });
    let placed = |f: &FrameSnapshot| -> Vec<String> {
        f.images
            .iter()
            .map(|p| {
                let i = &p.image;
                let pixels = i.rgba.iter().fold(0u64, |h, b| {
                    h.wrapping_mul(1_099_511_628_211) ^ u64::from(*b)
                });
                format!(
                    "{:?} z{} at {},{}+{},{} {}x{} from {},{} {}x{}: image {} {}x{} {pixels:x}",
                    p.layer,
                    p.z,
                    p.col,
                    p.row,
                    p.offset_x,
                    p.offset_y,
                    p.width,
                    p.height,
                    p.source_x,
                    p.source_y,
                    p.source_width,
                    p.source_height,
                    i.id,
                    i.width,
                    i.height
                )
            })
            .collect()
    };
    let (ia, ib) = (placed(a), placed(b));
    check("images", ia == ib, &|| format!("{ia:?} / {ib:?}"));
    out
}

fn blank_row(cols: u16) -> Row {
    Row {
        cells: vec![Cell::default(); usize::from(cols)].into(),
    }
}

/// One row of a frame as engine-neutral cells.
pub fn decode_row(r: &api::ScreenRow, cols: u16, styles: &[Style]) -> Result<Row, ScreenError> {
    let bad = |why: &str| ScreenError::Malformed(format!("row {}: {why}", r.index));
    if r.cells.len() > usize::from(cols) {
        return Err(bad("more cells than columns"));
    }
    let mut cells = Vec::with_capacity(usize::from(cols));
    let mut chars = r.text.chars();
    for v in &r.cells {
        let n = (v >> 2) as usize;
        let text: String = chars.by_ref().take(n).collect();
        if text.chars().count() != n {
            return Err(bad("the text runs out"));
        }
        cells.push(Cell {
            text: text.into_boxed_str(),
            width: match v & 3 {
                1 => CellWidth::Wide,
                2 => CellWidth::SpacerTail,
                3 => CellWidth::SpacerHead,
                _ => CellWidth::Narrow,
            },
            style: Style::default(),
        });
    }
    cells.resize(usize::from(cols), Cell::default());
    let mut x = 0usize;
    for run in r.styles.chunks(2) {
        let [n, s] = run else {
            return Err(bad("a style run without its style"));
        };
        let style = match *s {
            0 => Style::default(),
            i => *styles
                .get(i as usize - 1)
                .ok_or_else(|| bad("a style not in the table"))?,
        };
        let end = x + *n as usize;
        for c in cells
            .get_mut(x..end)
            .ok_or_else(|| bad("styles past the row"))?
        {
            c.style = style;
        }
        x = end;
    }
    Ok(Row {
        cells: cells.into(),
    })
}

fn rgb(v: u32) -> Rgb {
    Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    }
}

fn color(v: u32) -> Color {
    match v {
        0 => Color::Default,
        1..=256 => Color::Palette((v - 1) as u8),
        _ => Color::Rgb(rgb(v)),
    }
}

fn style(s: &api::CellStyle) -> Style {
    Style {
        fg: color(s.fg),
        bg: color(s.bg),
        underline_color: color(s.underline_color),
        attrs: Attrs(s.attrs as u8),
        underline: match s.underline {
            1 => Underline::Single,
            2 => Underline::Double,
            3 => Underline::Curly,
            4 => Underline::Dotted,
            5 => Underline::Dashed,
            _ => Underline::None,
        },
    }
}

fn modes(m: &api::InputModes) -> InputModes {
    InputModes {
        cursor_keys: m.cursor_keys,
        keypad_keys: m.keypad_keys,
        mouse_x10: m.mouse_x10,
        mouse_normal: m.mouse_normal,
        mouse_button: m.mouse_button,
        mouse_any: m.mouse_any,
        mouse_utf8: m.mouse_utf8,
        mouse_sgr: m.mouse_sgr,
        mouse_urxvt: m.mouse_urxvt,
        mouse_sgr_pixels: m.mouse_sgr_pixels,
        mouse_alternate_scroll: m.mouse_alternate_scroll,
        focus_events: m.focus_events,
        bracketed_paste: m.bracketed_paste,
        kitty_paste_events: m.kitty_paste_events,
        ignore_keypad_with_numlock: m.ignore_keypad_with_numlock,
        alt_esc_prefix: m.alt_esc_prefix,
        alt_sends_escape: m.alt_sends_escape,
        linefeed: m.linefeed,
        keyboard_locked: m.keyboard_locked,
        alt_screen: m.alt_screen,
        kitty_keyboard: m.kitty_keyboard as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(number: u64, full: bool, changed: Vec<api::ScreenRow>) -> api::ScreenFrame {
        api::ScreenFrame {
            number,
            full,
            cols: 4,
            rows: 2,
            changed,
            ..Default::default()
        }
    }

    fn row(index: u32, text: &str, cells: Vec<u32>, styles: Vec<u32>) -> api::ScreenRow {
        api::ScreenRow {
            index,
            text: text.into(),
            cells,
            styles,
        }
    }

    #[test]
    fn frames_apply_in_order_and_a_hole_is_refused() {
        let mut v = ScreenView::new();
        assert_eq!(
            v.apply(&frame(2, false, vec![])),
            Err(ScreenError::Gap {
                expected: 1,
                got: 2
            })
        );
        // "a" then a wide 世 and its tail; the wide character in style 1.
        let mut first = frame(
            1,
            true,
            vec![row(0, "a世", vec![4, 5, 2], vec![1, 0, 2, 1])],
        );
        first.styles = vec![api::CellStyle {
            fg: 2,
            attrs: 1,
            ..Default::default()
        }];
        v.apply(&first).expect("full");
        assert_eq!(v.rows[0].text(), "a世");
        assert_eq!(v.rows[0].cells[1].width, CellWidth::Wide);
        assert_eq!(v.rows[0].cells[1].style.fg, Color::Palette(1));
        assert!(v.rows[0].cells[1].style.attrs.has(Attrs::BOLD));
        assert_eq!(v.rows[0].cells[3], Cell::default());
        assert_eq!(v.rows[1].text(), "");
        assert!(matches!(
            v.apply(&frame(3, false, vec![])),
            Err(ScreenError::Gap { .. })
        ));
        v.apply(&frame(2, false, vec![row(1, "zz", vec![4, 4], vec![])]))
            .expect("next");
        assert_eq!(
            (v.rows[0].text(), v.rows[1].text()),
            ("a世".into(), "zz".into())
        );
        assert!(matches!(
            v.apply(&frame(3, false, vec![row(5, "", vec![], vec![])])),
            Err(ScreenError::Malformed(_))
        ));
        assert_eq!(v.number, 2, "a refused frame changes nothing");
    }

    /// What a daemon sends one view: the first frame full, then the rows
    /// and parts that differ from what it sent last, each encoded for the
    /// wire as the daemon encodes it.
    struct Sender {
        number: u64,
        last: Option<Arc<FrameSnapshot>>,
        pixels: std::collections::HashSet<(u32, u64)>,
    }

    impl Sender {
        fn new() -> Sender {
            Sender {
                number: 0,
                last: None,
                pixels: Default::default(),
            }
        }

        fn next(&mut self, e: &mut terminal_ghostty::Engine) -> api::ScreenFrame {
            let f = e.frame().expect("frame");
            let modes = e.input_modes().expect("modes");
            self.number += 1;
            let mut sf = terminal_ghostty::core::ScreenFrame::full(
                &f,
                modes,
                Some("t".into()),
                self.number,
                self.number,
            );
            if let Some(last) = self.last.as_ref().filter(|l| l.size == f.size) {
                sf.full = false;
                sf.title = None;
                sf.rows
                    .retain(|(i, r)| !Arc::ptr_eq(r, &last.rows[usize::from(*i)]));
                if Arc::ptr_eq(&last.colors, &f.colors) {
                    sf.colors = None;
                }
                if Arc::ptr_eq(&last.images, &f.images) {
                    sf.images = None;
                }
                sf.pixels
                    .retain(|p| !self.pixels.contains(&(p.id, p.generation)));
            }
            if sf.full || sf.images.is_some() {
                self.pixels = f
                    .images
                    .iter()
                    .map(|p| (p.image.id, p.image.generation))
                    .collect();
            }
            self.last = Some(f);
            client::frames::encode(
                &sf,
                client::frames::Head {
                    subscription: 1,
                    pane: 1,
                    epoch: 1,
                    generation: 1,
                },
            )
        }
    }

    fn engine() -> terminal_ghostty::Engine {
        use terminal_ghostty::core::family::CANONICAL_CELL_PX;
        let size = GridSize { cols: 40, rows: 8 };
        let mut c = terminal_ghostty::EngineConfig::new(size);
        c.capture_replies = false;
        c.scrollback_lines = Some(100);
        let mut e = terminal_ghostty::Engine::new(c).expect("engine");
        e.resize(size, CANONICAL_CELL_PX).expect("resize");
        e
    }

    /// Feed `chunks` to an engine, as a replica would, and after each one
    /// send a frame of the same engine to a frame-backed view: both must
    /// be drawn the same.
    fn same_on_frames(name: &str, chunks: &[&[u8]]) -> ScreenView {
        let mut e = engine();
        let mut sender = Sender::new();
        let mut view = ScreenView::new();
        assert!(view.frame().is_none(), "nothing to draw before a frame");
        let mut prev: Option<Arc<FrameSnapshot>> = None;
        for (i, chunk) in chunks.iter().enumerate() {
            e.feed(chunk).expect("feed");
            let wire = sender.next(&mut e);
            let wire = api::ScreenFrame::decode(wire.encode_to_vec().as_slice()).expect("decode");
            view.apply(&wire).expect("apply");
            let want = sender.last.clone().expect("engine frame");
            let got = view.frame().expect("view frame");
            let diff = differences(&want, &got);
            assert!(diff.is_empty(), "{name}, after chunk {i}: {diff:#?}");
            // A row the damage leaves out is the one drawn before.
            if let Some(p) = prev.as_ref().filter(|p| p.size == got.size) {
                for y in 0..got.size.rows {
                    if !got.damaged_rows.contains(&y) {
                        let y = usize::from(y);
                        assert!(Arc::ptr_eq(&p.rows[y], &got.rows[y]), "{name}: row {y}");
                    }
                }
            }
            prev = Some(got);
        }
        view
    }

    use prost::Message as _;

    #[test]
    fn every_renderer_fixture_draws_the_same_from_frames() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../terminal-render/fixtures");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).expect("fixtures") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|x| x != "vt") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("fixture");
            let name = path.display().to_string();
            // At once, and in three parts cut anywhere (inside sequences
            // too), as a busy pane's frames would see it.
            same_on_frames(&name, &[&bytes]);
            let (a, b) = (bytes.len() / 3, 2 * bytes.len() / 3);
            same_on_frames(&name, &[&bytes[..a], &bytes[a..b], &bytes[b..]]);
            n += 1;
        }
        assert!(n >= 12, "only {n} fixtures");
    }

    #[test]
    fn a_scripted_session_draws_the_same_from_frames() {
        let steps: &[&[u8]] = &[
            b"\x1b]2;title\x07plain \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[31mred\x1b[0m\r\n",
            b"\x1b[38;5;208m256\x1b[0m \x1b[38;2;10;200;30mrgb\x1b[0m \x1b[48;5;24mbg\x1b[0m \x1b[7minv\x1b[0m \x1b[9mstrike\x1b[0m\r\n",
            b"\x1b[4:3mcurly\x1b[0m \x1b[58;2;255;0;0m\x1b[4mred line\x1b[0m \x1b[2mfaint\x1b[0m\r\n",
            "wide 世界 emoji 👍🏽 e\u{301} 한국어\r\n".as_bytes(),
            // A wide character that wraps at the edge.
            "\x1b[38G世界世\r\n".as_bytes(),
            b"\x1b[6 qbar",
            b"\x1b[4 qunderline",
            b"\x1b[2 q\x1b[?25lhidden",
            b"\x1b[?25h\x1b]10;#c0c0c0\x07\x1b]11;#101820\x07\x1b]4;1;#ff8800\x07\x1b[31mpalette\x1b[0m",
            // The alternate screen with mouse reporting and bracketed paste.
            b"\x1b[?1049h\x1b[H\x1b[2Jalt screen\x1b[?1000h\x1b[?1006h\x1b[?2004h",
            b"\x1b[5;5Hmoved",
            b"\x1b[?1000l\x1b[?1002h",
            b"\x1b[?1002l\x1b[?1006l\x1b[?1049l",
            // Scrolled: history grows and the screen's top moves.
            b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12\r\n",
            b"\x1b[?2026hheld",
            b"\x1b[?2026l\x1b[2J\x1b[Hcleared",
        ];
        let view = same_on_frames("session", steps);
        assert!(view.top > 0, "the screen scrolled into history");
        assert_eq!(view.title.as_deref(), Some("t"));
    }

    #[test]
    fn the_view_state_follows_the_modes_the_frame_carries() {
        let mut e = engine();
        let mut sender = Sender::new();
        let mut view = ScreenView::new();
        for (bytes, tracking, alt) in [
            (&b"x"[..], false, false),
            (b"\x1b[?9h", true, false),
            (b"\x1b[?9l\x1b[?1000h", true, false),
            (b"\x1b[?1000l\x1b[?1002h", true, false),
            (b"\x1b[?1002l\x1b[?1003h\x1b[?1006h", true, false),
            (b"\x1b[?1003l\x1b[?1049h", false, true),
            (b"\x1b[?1000h", true, true),
            (b"\x1b[?1000l\x1b[?1049l", false, false),
        ] {
            e.feed(bytes).expect("feed");
            view.apply(&sender.next(&mut e)).expect("apply");
            let f = view.frame().expect("frame");
            assert_eq!(f.view, e.view_state().expect("view"), "{bytes:?}");
            assert_eq!((f.view.mouse_tracking, f.view.alt_screen), (tracking, alt));
            assert_eq!(view.modes, e.input_modes().expect("modes"));
        }
        e.feed(b"\x1b[?2004h\x1b[?1h\x1b[?1004h").expect("feed");
        view.apply(&sender.next(&mut e)).expect("apply");
        assert!(view.modes.bracketed_paste && view.modes.cursor_keys && view.modes.focus_events);
        // Nothing new: the same frame again.
        let a = view.frame().expect("frame");
        assert!(Arc::ptr_eq(&a, &view.frame().expect("frame")));
    }
}
