//! Engine-neutral terminal types.
//!
//! Everything here is owned, immutable once built, and free of foreign
//! pointers, so frames can cross threads and reach the renderer without any
//! tie to the engine that produced them. Rows are shared through `Arc` so an
//! unchanged row costs nothing in the next frame.

pub mod family;

use std::fmt::Write as _;
use std::sync::Arc;

/// Terminal grid dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A cell color as the terminal state describes it, before resolution
/// against the palette and defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    #[default]
    Default,
    Palette(u8),
    Rgb(Rgb),
}

/// Underline shapes (SGR 4 and 4:x).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Text attribute bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attrs(pub u8);

impl Attrs {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const FAINT: u8 = 1 << 2;
    pub const BLINK: u8 = 1 << 3;
    pub const INVERSE: u8 = 1 << 4;
    pub const INVISIBLE: u8 = 1 << 5;
    pub const STRIKETHROUGH: u8 = 1 << 6;
    pub const OVERLINE: u8 = 1 << 7;

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub attrs: Attrs,
    pub underline: Underline,
}

/// How a cell participates in the grid. The engine decides widths; the
/// renderer must never re-derive them from glyph advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CellWidth {
    #[default]
    Narrow,
    /// First column of a two-column character.
    Wide,
    /// Second column of a wide character; draws nothing itself.
    SpacerTail,
    /// Placeholder at the end of a row when a wide character wrapped.
    SpacerHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Cell {
    /// The grapheme cluster in this cell; empty for blank and spacer cells.
    pub text: Box<str>,
    pub width: CellWidth,
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Row {
    pub cells: Box<[Cell]>,
}

impl Row {
    /// The row's text with trailing blanks removed; spacer cells contribute
    /// nothing, blank cells contribute a space.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for cell in self.cells.iter() {
            match cell.width {
                CellWidth::SpacerTail | CellWidth::SpacerHead => {}
                _ if cell.text.is_empty() => s.push(' '),
                _ => s.push_str(&cell.text),
            }
        }
        s.truncate(s.trim_end_matches(' ').len());
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CursorShape {
    #[default]
    Block,
    BlockHollow,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Cursor {
    /// Visible and inside the viewport.
    pub visible: bool,
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub blinking: bool,
    /// Cursor sits on the tail of a wide character.
    pub on_wide_tail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub palette: Box<[Rgb; 256]>,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: Rgb::default(),
            background: Rgb::default(),
            cursor: None,
            palette: Box::new([Rgb::default(); 256]),
        }
    }
}

impl Colors {
    pub fn resolve(&self, color: Color, default: Rgb) -> Rgb {
        match color {
            Color::Default => default,
            Color::Palette(i) => self.palette[usize::from(i)],
            Color::Rgb(rgb) => rgb,
        }
    }
}

/// One immutable frame of a terminal's viewport.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSnapshot {
    /// Increases whenever the engine produced a changed frame.
    pub revision: u64,
    pub size: GridSize,
    pub rows: Vec<Arc<Row>>,
    pub cursor: Cursor,
    pub colors: Arc<Colors>,
    /// Indices of rows that changed since the previous revision; every row
    /// when the whole frame must be redrawn.
    pub damaged_rows: Vec<u16>,
    /// The application asked to hold presentation (synchronized output).
    pub render_hold: bool,
}

impl FrameSnapshot {
    /// Deterministic plain-text dump for fixtures and diagnostics: one line
    /// per row, then the cursor.
    pub fn text_dump(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&row.text());
            out.push('\n');
        }
        let c = &self.cursor;
        let _ = writeln!(
            out,
            "-- {}x{} cursor {},{} {:?}{}",
            self.size.cols,
            self.size.rows,
            c.x,
            c.y,
            c.shape,
            if c.visible { "" } else { " hidden" }
        );
        out
    }

    /// Like [`Self::text_dump`], with one line per styled run so fixtures can
    /// pin colors and attributes. Unstyled cells are not listed.
    pub fn style_dump(&self) -> String {
        let mut out = self.text_dump();
        for (y, row) in self.rows.iter().enumerate() {
            let mut x = 0usize;
            while x < row.cells.len() {
                let style = row.cells[x].style;
                let start = x;
                while x < row.cells.len() && row.cells[x].style == style {
                    x += 1;
                }
                if style != Style::default() {
                    let _ = writeln!(out, "style {y}:{start}..{x} {style:?}");
                }
            }
        }
        out
    }
}

/// Origin of bytes that should be written back toward the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySource {
    /// Generated by the terminal engine in answer to a query.
    Protocol,
}

/// Side effects observed while ingesting bytes, copied out of the engine.
/// Nothing here has been acted on; the caller applies its own policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Effects {
    pub bells: u32,
    /// Latest title, when it changed during this ingestion.
    pub title: Option<String>,
    /// Latest working directory report, when it changed.
    pub pwd: Option<String>,
    /// Synchronized-output transitions in order (`true` = hold begins).
    pub render_hold: Vec<bool>,
    /// Engine-generated replies. Empty unless reply capture was enabled.
    pub replies: Vec<(ReplySource, Vec<u8>)>,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        *self == Effects::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, width: CellWidth) -> Cell {
        Cell {
            text: text.into(),
            width,
            style: Style::default(),
        }
    }

    #[test]
    fn row_text_skips_spacers_and_trims() {
        let row = Row {
            cells: vec![
                cell("a", CellWidth::Narrow),
                cell("", CellWidth::Narrow),
                cell("世", CellWidth::Wide),
                cell("", CellWidth::SpacerTail),
                cell("", CellWidth::Narrow),
            ]
            .into(),
        };
        assert_eq!(row.text(), "a 世");
    }

    #[test]
    fn colors_resolve_palette_and_defaults() {
        let mut colors = Colors::default();
        colors.palette[1] = Rgb { r: 1, g: 2, b: 3 };
        let fallback = Rgb { r: 9, g: 9, b: 9 };
        assert_eq!(
            colors.resolve(Color::Palette(1), fallback),
            Rgb { r: 1, g: 2, b: 3 }
        );
        assert_eq!(colors.resolve(Color::Default, fallback), fallback);
    }
}
