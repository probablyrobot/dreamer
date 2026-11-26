//! A pane's screen kept from the daemon's screen frames (the `frames`
//! feed, `docs/mirrors.md`): no engine, only the rows, cursor, modes,
//! colors, and images the frames carry.
//!
//! A full frame replaces everything. Any other frame applies only on top
//! of the frame numbered one less; a frame that does not is refused and
//! the view is left as it was, so it never shows rows over a hole.

use std::collections::HashMap;
use std::sync::Arc;

use terminal_ghostty::core::{
    Attrs, Cell, CellWidth, Color, Colors, Cursor, CursorShape, GridSize, ImageLayer, ImagePixels,
    ImagePlacement, InputModes, Rgb, Row, Style, Underline,
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
        // Commit: nothing above changed the view.
        self.number = f.number;
        self.sequence = f.sequence;
        self.size = size;
        self.top = f.top;
        self.total_rows = f.total_rows;
        self.rows = rows;
        self.cursor = cursor.unwrap_or_default();
        self.modes = f.modes.as_ref().map(modes).unwrap_or_default();
        if f.full || f.title.is_some() {
            self.title.clone_from(&f.title);
        }
        self.colors = colors;
        self.images = images;
        self.pixels = pixels;
        self.render_hold = f.render_hold;
        Ok(())
    }
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
}
