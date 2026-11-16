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

/// How far apart cells sit, beyond what the font itself asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// Multiple of the font's own line height (ascent plus descent).
    pub line_height: f32,
    /// Logical pixels added to (or, negative, taken from) every cell's
    /// width.
    pub letter_spacing: f32,
}

/// Canonical cell geometry in logical pixels. Rows are snapped to device
/// pixels, because glyphs have no vertical subpixel positions; the width
/// is the font's exact advance, because they do have horizontal ones, and
/// rounding it would spread or squeeze every line of text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: Pixels,
    pub height: Pixels,
    /// Distance from the cell top to the text baseline.
    pub ascent: Pixels,
}

/// Derive cell metrics from a font. The width is the advance of `M` plus
/// the letter spacing; the height is the font's line height times
/// `line_height`.
pub fn cell_metrics(
    window: &Window,
    font: &Font,
    font_size: Pixels,
    spacing: Spacing,
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
    let height = snap(natural * spacing.line_height);
    // Center the natural line box inside the (possibly taller) cell.
    let ascent = snap(ascent + (height - natural) / 2.0);
    CellMetrics {
        width: (width + px(spacing.letter_spacing)).max(px(1.0)),
        height,
        ascent,
    }
}

/// Paints one terminal frame.
#[derive(Debug, Clone)]
pub struct TerminalElement {
    pub frame: Arc<FrameSnapshot>,
    pub font: Font,
    pub font_size: Pixels,
    pub spacing: Spacing,
}

/// Counts from the last paint, for diagnostics and smoke evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintStats {
    pub background_quads: usize,
    pub text_runs: usize,
    pub wide_cells: usize,
    pub images: usize,
    /// Image textures created in this paint (cache misses); a repaint of
    /// unchanged images creates none.
    pub image_uploads: usize,
    /// Image placements cut by the edge of the grid.
    pub images_clipped: usize,
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
        let metrics = cell_metrics(window, &self.font, self.font_size, self.spacing);
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
        let started = std::time::Instant::now();
        let stats = paint_frame(self, bounds, layout.metrics, window, cx);
        PAINT_NANOS.fetch_add(
            started.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        LAST_STATS.with(|s| s.set(stats));
        PAINTED.with(|p| {
            let mut p = p.borrow_mut();
            if p.len() == 256 {
                p.pop_front();
            }
            p.push_back(Arc::as_ptr(&self.frame) as usize);
        });
        PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

static PAINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PAINT_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The frames painted lately, by address (the benchmark asks whether a
    /// given frame has reached the screen).
    static PAINTED: std::cell::RefCell<std::collections::VecDeque<usize>> =
        std::cell::RefCell::new(std::collections::VecDeque::with_capacity(256));
}

/// Whether `frame` was among the last 256 frames painted on this thread.
pub fn was_painted(frame: &Arc<FrameSnapshot>) -> bool {
    let at = Arc::as_ptr(frame) as usize;
    PAINTED.with(|p| p.borrow().contains(&at))
}

/// CPU time spent painting terminal elements since the process started,
/// in nanoseconds (the benchmark's frame work).
pub fn paint_nanos() -> u64 {
    PAINT_NANOS.load(std::sync::atomic::Ordering::Relaxed)
}

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
        image_uploads: 0,
        images_clipped: 0,
        shaped_lines: 0,
        reused_lines: 0,
        decorated_runs: 0,
        cursor: None,
    }) };
    static SHAPES: std::cell::RefCell<ShapeCache> = std::cell::RefCell::new(ShapeCache::default());
    static TEXTURES: std::cell::RefCell<TextureCache> = std::cell::RefCell::new(TextureCache::default());
}

/// GPU-ready images by (engine, image id, generation): an unchanged image
/// is never converted or uploaded again. Entries the current frame no longer uses
/// are dropped after the paint and their atlas tiles released, so replaced
/// and deleted images, and everything from a replaced engine (whose
/// generations are new), go at once.
#[derive(Default)]
struct TextureCache {
    entries: std::collections::HashMap<(u64, u32, u64), Arc<RenderImage>>,
    used: std::collections::HashSet<(u64, u32, u64)>,
}

impl TextureCache {
    fn get(
        &mut self,
        img: &terminal_core::ImagePixels,
        stats: &mut PaintStats,
    ) -> Option<Arc<RenderImage>> {
        let key = (img.engine, img.id, img.generation);
        self.used.insert(key);
        if let Some(t) = self.entries.get(&key) {
            return Some(Arc::clone(t));
        }
        let t = rgba_image(img.width, img.height, img.rgba.to_vec())?;
        stats.image_uploads += 1;
        self.entries.insert(key, Arc::clone(&t));
        Some(t)
    }

    /// Drop what this paint did not use and free its atlas tiles.
    fn end_paint(&mut self, window: &mut Window) {
        let used = std::mem::take(&mut self.used);
        let stale: Vec<_> = self
            .entries
            .keys()
            .filter(|k| !used.contains(k))
            .copied()
            .collect();
        for key in stale {
            if let Some(t) = self.entries.remove(&key) {
                let _ = window.drop_image(t);
            }
        }
    }
}

/// Paint the frame's placements in one layer. Pixel values are canonical
/// (10x20 per cell) and scaled to this view's cells. A source crop is drawn
/// by placing the whole image so the crop lands on the destination and
/// clipping to the destination; the texture is the same for every crop.
fn paint_images(
    frame: &FrameSnapshot,
    layer: terminal_core::ImageLayer,
    bounds: Bounds<Pixels>,
    m: CellMetrics,
    window: &mut Window,
    stats: &mut PaintStats,
) {
    for p in frame.images.iter().filter(|p| p.layer == layer) {
        let Some((visible, whole)) = placement_rects(p, bounds, m) else {
            continue;
        };
        let Some(texture) = TEXTURES.with(|t| t.borrow_mut().get(&p.image, stats)) else {
            continue;
        };
        if clipped(visible, whole_dest(p, bounds, m)) {
            stats.images_clipped += 1;
        }
        if window
            .paint_image(visible, whole, Corners::default(), texture, 0, false)
            .is_ok()
        {
            stats.images += 1;
        }
    }
}

/// Whether the grid cut part of a placement off. Edges are compared with a
/// small tolerance: with fractional cell sizes (a 1.5 scale factor gives
/// 20.67 px cells) intersecting the same rectangle can move an edge by a
/// rounding error, which is not a clip.
fn clipped(visible: Bounds<Pixels>, dest: Bounds<Pixels>) -> bool {
    const EPS: f32 = 0.01;
    let far = |b: Bounds<Pixels>| {
        (
            f32::from(b.origin.x + b.size.width),
            f32::from(b.origin.y + b.size.height),
        )
    };
    let (vr, vb) = far(visible);
    let (dr, db) = far(dest);
    (f32::from(visible.origin.x) - f32::from(dest.origin.x)).abs() > EPS
        || (f32::from(visible.origin.y) - f32::from(dest.origin.y)).abs() > EPS
        || (vr - dr).abs() > EPS
        || (vb - db).abs() > EPS
}

/// Where a placement lands: its destination rectangle in this view.
fn whole_dest(
    p: &terminal_core::ImagePlacement,
    bounds: Bounds<Pixels>,
    m: CellMetrics,
) -> Bounds<Pixels> {
    let (cw, ch) = terminal_core::family::CANONICAL_CELL_PX;
    let sx = f32::from(m.width) / cw as f32;
    let sy = f32::from(m.height) / ch as f32;
    Bounds::new(
        point(
            bounds.origin.x + m.width * p.col as f32 + px(p.offset_x as f32 * sx),
            bounds.origin.y + m.height * p.row as f32 + px(p.offset_y as f32 * sy),
        ),
        size(px(p.width as f32 * sx), px(p.height as f32 * sy)),
    )
}

/// The visible part of a placement (its destination clipped to the grid)
/// and the rectangle the whole image is drawn into so that the source crop
/// lands exactly on the destination. `None` when nothing is visible.
fn placement_rects(
    p: &terminal_core::ImagePlacement,
    bounds: Bounds<Pixels>,
    m: CellMetrics,
) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
    if p.width == 0 || p.height == 0 || p.source_width == 0 || p.source_height == 0 {
        return None;
    }
    let dest = whole_dest(p, bounds, m);
    // Scale from source pixels to destination pixels.
    let kx = f32::from(dest.size.width) / p.source_width as f32;
    let ky = f32::from(dest.size.height) / p.source_height as f32;
    let whole = Bounds::new(
        point(
            dest.origin.x - px(p.source_x as f32 * kx),
            dest.origin.y - px(p.source_y as f32 * ky),
        ),
        size(
            px(p.image.width as f32 * kx),
            px(p.image.height as f32 * ky),
        ),
    );
    let visible = dest.intersect(&bounds);
    if visible.size.width <= Pixels::ZERO || visible.size.height <= Pixels::ZERO {
        return None;
    }
    Some((visible, whole))
}

/// Statistics from the most recent paint on this thread.
pub fn last_paint_stats() -> PaintStats {
    LAST_STATS.with(std::cell::Cell::get)
}

/// WCAG 2 contrast ratio between two colors.
fn contrast(a: Rgb, b: Rgb) -> f32 {
    let lum = |c: Rgb| {
        let ch = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// The cursor's color: the terminal's, unless it barely shows on the
/// background (some schemes pick a cursor close to their background); then
/// the text color, so the cursor is always findable. A view choice only:
/// the terminal's colors are unchanged.
fn cursor_color(colors: &Colors) -> Rgb {
    match colors.cursor {
        Some(c) if contrast(c, colors.background) >= 3.0 => c,
        _ => colors.foreground,
    }
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

/// Selection highlight: translucent, so cell backgrounds still show.
const SELECTION: gpui::Rgba = gpui::Rgba {
    r: 0.29,
    g: 0.44,
    b: 0.65,
    a: 0.45,
};

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
    // Column edges on device pixels, so neighbouring backgrounds meet
    // without a blended seam even when the cell width is fractional.
    let scale = window.scale_factor();
    let col_x = |col: usize| {
        let x = origin.x + m.width * col as f32;
        px((f32::from(x) * scale).round() / scale)
    };
    let span = |start: usize, end: usize, top: Pixels| {
        Bounds::new(
            point(col_x(start), top),
            size(col_x(end) - col_x(start), m.height),
        )
    };
    window.paint_quad(fill(bounds, hsla(colors.background)));

    // Layer order (spec 9.3): default background, images below cell
    // backgrounds, cell backgrounds, images below text, selection, text,
    // images above text, cursor.
    use terminal_core::ImageLayer;
    paint_images(
        frame,
        ImageLayer::BelowBackground,
        bounds,
        m,
        window,
        &mut stats,
    );
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
                window.paint_quad(fill(span(start, x, top), bg));
                stats.background_quads += 1;
            }
        }
    }
    paint_images(frame, ImageLayer::BelowText, bounds, m, window, &mut stats);
    for (y, row) in frame.rows.iter().enumerate() {
        let top = origin.y + m.height * y as f32;

        // Selection: over backgrounds, under text. View state only; the
        // cells themselves are never changed.
        for sel in frame.selection.iter().filter(|s| usize::from(s.row) == y) {
            let rect = span(usize::from(sel.start), usize::from(sel.end) + 1, top);
            window.paint_quad(fill(rect, SELECTION));
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
    paint_images(frame, ImageLayer::AboveText, bounds, m, window, &mut stats);
    TEXTURES.with(|t| t.borrow_mut().end_paint(window));

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
        let mut color = hsla(cursor_color(colors));
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

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::{ImageLayer, ImagePixels, ImagePlacement};

    fn placement(col: i32, row: i32, w: u32, h: u32, src: (u32, u32, u32, u32)) -> ImagePlacement {
        ImagePlacement {
            image: Arc::new(ImagePixels {
                engine: 1,
                id: 1,
                generation: 1,
                width: 40,
                height: 40,
                rgba: vec![0; 40 * 40 * 4].into(),
            }),
            layer: ImageLayer::AboveText,
            z: 0,
            col,
            row,
            offset_x: 5,
            offset_y: 10,
            width: w,
            height: h,
            source_x: src.0,
            source_y: src.1,
            source_width: src.2,
            source_height: src.3,
        }
    }

    /// 8x16 cells: canonical pixels (10x20) scale by 0.8.
    const M: CellMetrics = CellMetrics {
        width: px(8.0),
        height: px(16.0),
        ascent: px(12.0),
    };

    fn grid() -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(80.0), px(64.0)))
    }

    #[test]
    fn a_crop_lands_exactly_on_the_destination() {
        // The 20x20 centre of a 40x40 image, drawn 20x20 canonical pixels
        // at cell (2, 1) with a (5, 10) offset.
        let p = placement(2, 1, 20, 20, (10, 10, 20, 20));
        let (visible, whole) = placement_rects(&p, grid(), M).expect("visible");
        assert_eq!(
            visible,
            Bounds::new(point(px(20.0), px(24.0)), size(px(16.0), px(16.0)))
        );
        // The whole image is twice the crop, shifted so the crop's corner
        // is at the destination's corner.
        assert_eq!(
            whole,
            Bounds::new(point(px(12.0), px(16.0)), size(px(32.0), px(32.0)))
        );
    }

    #[test]
    fn placements_are_clipped_to_the_grid_or_skipped() {
        // Starts two cells from the right edge, four cells wide.
        let p = placement(8, 0, 40, 20, (0, 0, 40, 40));
        let (visible, _) = placement_rects(&p, grid(), M).expect("partly visible");
        assert_eq!(visible.origin.x, px(68.0));
        assert_eq!(visible.size.width, px(12.0), "cut at the grid's right edge");
        assert_ne!(visible, whole_dest(&p, grid(), M));
        // Entirely above the viewport: nothing to draw.
        assert!(placement_rects(&placement(0, -5, 10, 20, (0, 0, 40, 40)), grid(), M).is_none());
        // A zero-sized crop is never drawn.
        assert!(placement_rects(&placement(0, 0, 10, 20, (0, 0, 0, 40)), grid(), M).is_none());
    }

    #[test]
    fn fractional_cells_do_not_count_a_whole_image_as_clipped() {
        // X11 at scale 1.5: 8 x 20.666666 px cells in a grid whose origin is
        // fractional too.
        let m = CellMetrics {
            width: px(8.0),
            height: px(20.666_666),
            ascent: px(15.333_333),
        };
        let mut exact_differs = 0;
        for k in 0..30 {
            let bounds = Bounds::new(
                point(px(0.0), px(k as f32 / 3.0)),
                size(px(900.0), m.height * 20.0),
            );
            for row in 0..17 {
                let p = placement(0, row, 20, 40, (0, 0, 40, 40));
                let (visible, _) = placement_rects(&p, bounds, m).expect("visible");
                let dest = whole_dest(&p, bounds, m);
                exact_differs += usize::from(visible != dest);
                assert!(!clipped(visible, dest), "row {row}, origin {k}/3");
            }
            // A real cut is still a clip.
            let p = placement(110, 0, 40, 20, (0, 0, 40, 40));
            let (visible, _) = placement_rects(&p, bounds, m).expect("partly visible");
            assert!(clipped(visible, whole_dest(&p, bounds, m)));
        }
        // The case this guards against does occur with these sizes.
        assert!(exact_differs > 0, "no rounding difference to guard against");
    }

    #[test]
    fn a_faint_cursor_is_drawn_in_the_text_color() {
        let mut c = Colors {
            foreground: Rgb {
                r: 0x57,
                g: 0x52,
                b: 0x79,
            },
            background: Rgb {
                r: 0xfa,
                g: 0xf4,
                b: 0xed,
            },
            cursor: Some(Rgb {
                r: 0xf2,
                g: 0xe9,
                b: 0xe1,
            }),
            ..Colors::default()
        };
        assert_eq!(
            cursor_color(&c),
            c.foreground,
            "too close to the background"
        );
        let own = Rgb {
            r: 0x28,
            g: 0x69,
            b: 0x83,
        };
        c.cursor = Some(own);
        assert_eq!(cursor_color(&c), own, "the scheme's own");
    }
}
