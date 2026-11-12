//! Cell-accurate terminal rendering on GPUI.
//!
//! [`TerminalElement`] paints a [`FrameSnapshot`] produced by the terminal
//! engine. Grid geometry comes from canonical cell metrics; glyph advances
//! never move the grid. Narrow cells with the same style are shaped as one
//! run with every glyph forced to the cell width; wide cells are shaped on
//! their own and given two cells.
//!
//! Paint order: default background, cell backgrounds, text with its
//! decorations, cursor, inline images.
//!
//! This is the M0 proof renderer: no glyph or shaping caches yet, and images
//! are placed by the caller (Kitty placement extraction arrives with the
//! image milestone).

use std::sync::Arc;

use gpui::{
    App, Bounds, Corners, Element, ElementId, Font, GlobalElementId, Hsla, InspectorElementId,
    LayoutId, Pixels, RenderImage, SharedString, StrikethroughStyle, Style, TextAlign, TextRun,
    UnderlineStyle, Window, fill, point, px, size,
};
use terminal_core::{
    Attrs, Cell, CellWidth, Color, Colors, CursorShape, FrameSnapshot, Rgb, Underline,
};

/// Canonical cell geometry in logical pixels, snapped to device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: Pixels,
    pub height: Pixels,
    /// Distance from the cell top to the text baseline.
    pub ascent: Pixels,
}

/// Derive cell metrics from a font. The width is the advance of `M`; the
/// height is the font's line height times `line_height`.
pub fn cell_metrics(
    window: &Window,
    font: &Font,
    font_size: Pixels,
    line_height: f32,
) -> CellMetrics {
    let ts = window.text_system();
    let id = ts.resolve_font(font);
    let width = ts
        .advance(id, font_size, 'M')
        .map(|s| s.width)
        .unwrap_or(font_size * 0.6);
    let ascent = ts.ascent(id, font_size);
    let descent = ts.descent(id, font_size);
    let natural = ascent + descent.abs();
    let scale = window.scale_factor();
    let snap = |p: Pixels| px((f32::from(p) * scale).round() / scale);
    let height = snap(natural * line_height);
    // Center the natural line box inside the (possibly taller) cell.
    let ascent = snap(ascent + (height - natural) / 2.0);
    CellMetrics {
        width: snap(width),
        height,
        ascent,
    }
}

/// An image to composite over the grid, positioned in cells.
#[derive(Clone)]
pub struct InlineImage {
    pub image: Arc<RenderImage>,
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

impl std::fmt::Debug for InlineImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineImage")
            .field("col", &self.col)
            .field("row", &self.row)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

/// Paints one terminal frame.
#[derive(Debug, Clone)]
pub struct TerminalElement {
    pub frame: Arc<FrameSnapshot>,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: f32,
    pub images: Vec<InlineImage>,
}

/// Counts from the last paint, for diagnostics and smoke evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintStats {
    pub background_quads: usize,
    pub text_runs: usize,
    pub wide_cells: usize,
    pub images: usize,
    /// Text lines shaped in this paint (cache misses).
    pub shaped_lines: usize,
    /// Text lines reused from the shaping cache.
    pub reused_lines: usize,
    /// Runs painted with an underline or strikethrough.
    pub decorated_runs: usize,
    /// The cursor shape painted, if the cursor was visible.
    pub cursor: Option<&'static str>,
}

#[derive(Debug)]
pub struct Layout {
    metrics: CellMetrics,
}

impl Element for TerminalElement {
    type RequestLayoutState = Layout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Layout) {
        let metrics = cell_metrics(window, &self.font, self.font_size, self.line_height);
        let mut style = Style::default();
        style.size.width = (metrics.width * f32::from(self.frame.size.cols)).into();
        style.size.height = (metrics.height * f32::from(self.frame.size.rows)).into();
        (window.request_layout(style, [], cx), Layout { metrics })
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut Layout,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Layout,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let stats = paint_frame(self, bounds, layout.metrics, window, cx);
        LAST_STATS.with(|s| s.set(stats));
        PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

static PAINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Terminal elements painted since the process started. A static view
/// must not move it (spec 16.6: zero render wakeups).
pub fn paint_count() -> u64 {
    PAINTS.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    static LAST_STATS: std::cell::Cell<PaintStats> = const { std::cell::Cell::new(PaintStats {
        background_quads: 0,
        text_runs: 0,
        wide_cells: 0,
        images: 0,
        shaped_lines: 0,
        reused_lines: 0,
        decorated_runs: 0,
        cursor: None,
    }) };
    static SHAPES: std::cell::RefCell<ShapeCache> = std::cell::RefCell::new(ShapeCache::default());
}

/// Statistics from the most recent paint on this thread.
pub fn last_paint_stats() -> PaintStats {
    LAST_STATS.with(std::cell::Cell::get)
}

fn hsla(c: Rgb) -> Hsla {
    gpui::rgb((u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)).into()
}

/// Foreground and background after inverse, faint, and invisible.
fn cell_colors(cell: &Cell, colors: &Colors) -> (Hsla, Option<Hsla>) {
    let style = &cell.style;
    let mut fg = colors.resolve(style.fg, colors.foreground);
    let mut bg = match style.bg {
        Color::Default => None,
        other => Some(colors.resolve(other, colors.background)),
    };
    if style.attrs.has(Attrs::INVERSE) {
        let new_bg = fg;
        fg = bg.unwrap_or(colors.background);
        bg = Some(new_bg);
    }
    let mut fg = hsla(fg);
    if style.attrs.has(Attrs::FAINT) {
        fg.a *= 0.5;
    }
    if style.attrs.has(Attrs::INVISIBLE) {
        fg.a = 0.0;
    }
    (fg, bg.map(hsla))
}

/// One shaped piece of a row: a run of narrow cells with one style, or a
/// single wide cell.
struct Shaped {
    col: usize,
    line: gpui::ShapedLine,
    wide: bool,
    decorated: bool,
}

/// Shaped rows by shared row identity (spec 9.2: unchanged rows are shared
/// between frames, so their shaping is too). Each entry holds its row, so
/// the address it is keyed by cannot be reused while the entry exists.
#[derive(Default)]
struct ShapeCache {
    rows: std::collections::HashMap<usize, ShapeEntry>,
    paints: u64,
}

struct ShapeEntry {
    row: Arc<terminal_core::Row>,
    colors: Arc<Colors>,
    key: (Font, Pixels, Pixels),
    lines: std::rc::Rc<Vec<Shaped>>,
    used: u64,
}

/// Rows kept beyond those painted recently.
const SHAPE_CACHE_ROWS: usize = 4096;

impl ShapeCache {
    fn lookup(
        &mut self,
        row: &Arc<terminal_core::Row>,
        colors: &Arc<Colors>,
        key: &(Font, Pixels, Pixels),
    ) -> Option<std::rc::Rc<Vec<Shaped>>> {
        let e = self.rows.get_mut(&(Arc::as_ptr(row) as usize))?;
        if Arc::ptr_eq(&e.row, row) && Arc::ptr_eq(&e.colors, colors) && e.key == *key {
            e.used = self.paints;
            Some(e.lines.clone())
        } else {
            None
        }
    }

    fn insert(
        &mut self,
        row: &Arc<terminal_core::Row>,
        colors: &Arc<Colors>,
        key: (Font, Pixels, Pixels),
        lines: std::rc::Rc<Vec<Shaped>>,
    ) {
        self.rows.insert(
            Arc::as_ptr(row) as usize,
            ShapeEntry {
                row: row.clone(),
                colors: colors.clone(),
                key,
                lines,
                used: self.paints,
            },
        );
    }

    /// Forget rows not painted lately once the cache is large.
    fn end_paint(&mut self) {
        self.paints += 1;
        if self.rows.len() > SHAPE_CACHE_ROWS {
            let keep_from = self.paints.saturating_sub(4);
            self.rows.retain(|_, e| e.used >= keep_from);
        }
    }
}

fn shape_row(
    row: &terminal_core::Row,
    el: &TerminalElement,
    m: CellMetrics,
    colors: &Colors,
    window: &mut Window,
) -> Vec<Shaped> {
    let mut out = Vec::new();
    let mut x = 0usize;
    while x < row.cells.len() {
        let cell = &row.cells[x];
        match cell.width {
            CellWidth::SpacerTail | CellWidth::SpacerHead => x += 1,
            CellWidth::Wide => {
                if !cell.text.is_empty() {
                    let (fg, _) = cell_colors(cell, colors);
                    let text: SharedString = cell.text.to_string().into();
                    let run = text_run(cell, text.len(), &el.font, colors, fg);
                    let decorated = run.underline.is_some() || run.strikethrough.is_some();
                    let line = window.text_system().shape_line(
                        text,
                        el.font_size,
                        &[run],
                        Some(m.width * 2.0),
                    );
                    out.push(Shaped {
                        col: x,
                        line,
                        wide: true,
                        decorated,
                    });
                }
                x += 1;
            }
            CellWidth::Narrow => {
                let start = x;
                let style = cell.style;
                let mut text = String::new();
                while x < row.cells.len()
                    && row.cells[x].width == CellWidth::Narrow
                    && row.cells[x].style == style
                {
                    let t = &row.cells[x].text;
                    text.push_str(if t.is_empty() { " " } else { t });
                    x += 1;
                }
                if text.trim().is_empty()
                    && style.underline == Underline::None
                    && !style.attrs.has(Attrs::STRIKETHROUGH)
                {
                    continue;
                }
                let (fg, _) = cell_colors(&row.cells[start], colors);
                let run = text_run(&row.cells[start], text.len(), &el.font, colors, fg);
                let decorated = run.underline.is_some() || run.strikethrough.is_some();
                let line = window.text_system().shape_line(
                    text.into(),
                    el.font_size,
                    &[run],
                    Some(m.width),
                );
                out.push(Shaped {
                    col: start,
                    line,
                    wide: false,
                    decorated,
                });
            }
        }
    }
    out
}

fn text_run(cell: &Cell, len: usize, font: &Font, colors: &Colors, fg: Hsla) -> TextRun {
    let mut font = font.clone();
    if cell.style.attrs.has(Attrs::BOLD) {
        font.weight = gpui::FontWeight::BOLD;
    }
    if cell.style.attrs.has(Attrs::ITALIC) {
        font.style = gpui::FontStyle::Italic;
    }
    let underline = match cell.style.underline {
        Underline::None => None,
        kind => Some(UnderlineStyle {
            thickness: px(1.0),
            color: match cell.style.underline_color {
                Color::Default => Some(fg),
                c => Some(hsla(colors.resolve(c, colors.foreground))),
            },
            wavy: kind == Underline::Curly,
        }),
    };
    TextRun {
        len,
        font,
        color: fg,
        background_color: None,
        underline,
        strikethrough: cell
            .style
            .attrs
            .has(Attrs::STRIKETHROUGH)
            .then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(fg),
            }),
    }
}

fn paint_frame(
    el: &TerminalElement,
    bounds: Bounds<Pixels>,
    m: CellMetrics,
    window: &mut Window,
    cx: &mut App,
) -> PaintStats {
    let mut stats = PaintStats::default();
    let frame = &el.frame;
    let colors = &frame.colors;
    let origin = bounds.origin;
    window.paint_quad(fill(bounds, hsla(colors.background)));

    for (y, row) in frame.rows.iter().enumerate() {
        let top = origin.y + m.height * y as f32;

        // Backgrounds: merge adjacent cells with the same color.
        let mut x = 0usize;
        while x < row.cells.len() {
            let (_, bg) = cell_colors(&row.cells[x], colors);
            let start = x;
            while x < row.cells.len() && cell_colors(&row.cells[x], colors).1 == bg {
                x += 1;
            }
            if let Some(bg) = bg {
                let rect = Bounds::new(
                    point(origin.x + m.width * start as f32, top),
                    size(m.width * (x - start) as f32, m.height),
                );
                window.paint_quad(fill(rect, bg));
                stats.background_quads += 1;
            }
        }

        // Text: shaped once per shared row and reused for as long as the
        // row, its colors, the font, the size, and the cell width are the same.
        let key = (el.font.clone(), el.font_size, m.width);
        let lines = match SHAPES.with(|c| c.borrow_mut().lookup(row, colors, &key)) {
            Some(lines) => {
                stats.reused_lines += lines.len();
                lines
            }
            None => {
                let lines = std::rc::Rc::new(shape_row(row, el, m, colors, window));
                stats.shaped_lines += lines.len();
                SHAPES.with(|c| c.borrow_mut().insert(row, colors, key, lines.clone()));
                lines
            }
        };
        for s in lines.iter() {
            let at = point(origin.x + m.width * s.col as f32, top);
            let _ = s
                .line
                .paint(at, m.height, TextAlign::Left, None, window, cx);
            if s.wide {
                stats.wide_cells += 1;
            } else {
                stats.text_runs += 1;
            }
            if s.decorated {
                stats.decorated_runs += 1;
            }
        }
    }
    SHAPES.with(|c| c.borrow_mut().end_paint());

    // Cursor.
    let c = frame.cursor;
    if c.visible {
        let wide = frame
            .rows
            .get(usize::from(c.y))
            .and_then(|r| r.cells.get(usize::from(c.x)))
            .is_some_and(|cell| cell.width == CellWidth::Wide);
        let w = if wide { m.width * 2.0 } else { m.width };
        let at = point(
            origin.x + m.width * f32::from(c.x),
            origin.y + m.height * f32::from(c.y),
        );
        let mut color = hsla(colors.cursor.unwrap_or(colors.foreground));
        stats.cursor = Some(match c.shape {
            CursorShape::Block => "block",
            CursorShape::Bar => "bar",
            CursorShape::Underline => "underline",
            CursorShape::BlockHollow => "block_hollow",
        });
        let rect = match c.shape {
            CursorShape::Block => {
                color.a = 0.55;
                Bounds::new(at, size(w, m.height))
            }
            CursorShape::Bar => Bounds::new(at, size(px(2.0), m.height)),
            CursorShape::Underline => {
                Bounds::new(point(at.x, at.y + m.height - px(2.0)), size(w, px(2.0)))
            }
            CursorShape::BlockHollow => {
                let outline = gpui::quad(
                    Bounds::new(at, size(w, m.height)),
                    Corners::default(),
                    gpui::transparent_black(),
                    px(1.0),
                    color,
                    gpui::BorderStyle::Solid,
                );
                window.paint_quad(outline);
                Bounds::default()
            }
        };
        if rect.size.width > Pixels::ZERO {
            window.paint_quad(fill(rect, color));
        }
    }

    // Images, clipped to the grid.
    for img in &el.images {
        let rect = Bounds::new(
            point(
                origin.x + m.width * f32::from(img.col),
                origin.y + m.height * f32::from(img.row),
            ),
            size(
                m.width * f32::from(img.cols),
                m.height * f32::from(img.rows),
            ),
        );
        if window
            .paint_image(
                bounds,
                rect,
                Corners::default(),
                Arc::clone(&img.image),
                0,
                false,
            )
            .is_ok()
        {
            stats.images += 1;
        }
    }
    stats
}

/// Build a GPUI image from tightly packed RGBA pixels.
pub fn rgba_image(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    // GPUI expects BGRA.
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
    let frame = image::Frame::new(buffer);
    Some(Arc::new(RenderImage::new(smallvec::smallvec![frame])))
}
