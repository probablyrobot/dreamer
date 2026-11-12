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
    }) };
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

        // Text: runs of narrow cells with one style, wide cells alone.
        let mut x = 0usize;
        while x < row.cells.len() {
            let cell = &row.cells[x];
            match cell.width {
                CellWidth::SpacerTail | CellWidth::SpacerHead => {
                    x += 1;
                    continue;
                }
                CellWidth::Wide => {
                    if !cell.text.is_empty() {
                        let (fg, _) = cell_colors(cell, colors);
                        let text: SharedString = cell.text.to_string().into();
                        let run = text_run(cell, text.len(), &el.font, colors, fg);
                        let line = window.text_system().shape_line(
                            text,
                            el.font_size,
                            &[run],
                            Some(m.width * 2.0),
                        );
                        let at = point(origin.x + m.width * x as f32, top);
                        let _ = line.paint(at, m.height, TextAlign::Left, None, window, cx);
                        stats.wide_cells += 1;
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
                    if text.trim().is_empty() && style.underline == Underline::None {
                        continue;
                    }
                    let (fg, _) = cell_colors(&row.cells[start], colors);
                    let run = text_run(&row.cells[start], text.len(), &el.font, colors, fg);
                    let line = window.text_system().shape_line(
                        text.into(),
                        el.font_size,
                        &[run],
                        Some(m.width),
                    );
                    let at = point(origin.x + m.width * start as f32, top);
                    let _ = line.paint(at, m.height, TextAlign::Left, None, window, cx);
                    stats.text_runs += 1;
                }
            }
        }
    }

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
