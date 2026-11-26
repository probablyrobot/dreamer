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
static SHAPE_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static IMAGE_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The frames painted lately, by address (the benchmark asks whether a
    /// given frame has reached the screen).
    static PAINTED: std::cell::RefCell<std::collections::VecDeque<usize>> =
        std::cell::RefCell::new(std::collections::VecDeque::with_capacity(256));
}

/// Forget which frames were painted: what is painted from now on counts.
pub fn forget_painted() {
    PAINTED.with(|p| p.borrow_mut().clear());
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

/// Runs drawn as placed ASCII glyphs without shaping, since start.
static FAST_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs drawn as placed ASCII glyphs without shaping ([`AsciiFont`]).
pub fn fast_runs() -> u64 {
    FAST_RUNS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Of [`paint_nanos`], the time spent shaping rows the shape cache did
/// not hold, in nanoseconds.
pub fn shape_nanos() -> u64 {
    SHAPE_NANOS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Of [`paint_nanos`], the time spent turning new or changed images into
/// GPU-ready textures, in nanoseconds.
pub fn image_nanos() -> u64 {
    IMAGE_NANOS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Terminal elements painted since the process started. A static view
/// must not move it (zero render wakeups).
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
    /// [`AsciiFont`]s by font, size, and cell width (`None`: the font has
    /// no usable ASCII glyphs of its own).
    #[allow(clippy::type_complexity)]
    static ASCII: std::cell::RefCell<std::collections::HashMap<(Font, Pixels, Pixels), Option<AsciiFont>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
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
        let started = std::time::Instant::now();
        let t = rgba_image(img.width, img.height, img.rgba.to_vec());
        IMAGE_NANOS.fetch_add(
            started.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let t = t?;
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
    body: Body,
    wide: bool,
    decorated: bool,
}

/// How a piece is drawn: a line from the text system's shaper, or ASCII
/// glyphs placed one per cell ([`AsciiFont`]).
enum Body {
    Line(Box<gpui::ShapedLine>),
    Glyphs(PlacedGlyphs),
}

/// Glyphs at cell offsets from the piece's first column, in one font and
/// color, painted at the baseline the shaped line would use.
struct PlacedGlyphs {
    font_id: gpui::FontId,
    font_size: Pixels,
    ascent: Pixels,
    descent: Pixels,
    color: Hsla,
    glyphs: Vec<(Pixels, gpui::GlyphId)>,
}

/// `DREAMER_RENDER_ASCII`: unset or `off` shapes every run; `learned`
/// lets runs skip shaping where that was learned to be exact
/// ([`AsciiFont`]); `all` places every run of the font's own glyphs, to
/// compare the two paths pixel for pixel. Off by default: on Linux
/// (Wayland, 16 panes with 15 busy) placing glyphs made paint p95 about
/// 7 times slower than shaping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsciiMode {
    Learned,
    Off,
    All,
}

fn ascii_mode() -> AsciiMode {
    static MODE: std::sync::OnceLock<AsciiMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("DREAMER_RENDER_ASCII").as_deref() {
        Ok("learned") => AsciiMode::Learned,
        Ok("all") => AsciiMode::All,
        _ => AsciiMode::Off,
    })
}

/// Printable ASCII, the only characters the fast path draws.
const ASCII_FIRST: u8 = 0x20;
const ASCII_COUNT: usize = 95;
/// A run's start or end, as one more symbol after the characters.
const EDGE: u64 = ASCII_COUNT as u64;
/// Symbols in a window ([`AsciiFont`]).
const WINDOW: usize = 5;
/// Windows remembered per font; past this nothing new is learned, so
/// endless varied text (a base64 dump) cannot grow the set without bound.
const WINDOWS_MAX: usize = 1 << 18;

/// A font's glyphs for printable ASCII, and the stretches of text it has
/// been seen to draw as exactly those glyphs, one per cell.
///
/// Shaping a run of narrow cells through the text system is most of a
/// busy frame's cost, yet most terminal text is ASCII that a monospace
/// font draws one glyph per character. A run may skip shaping when every
/// character has a glyph in the font itself (no fallback) and every window
/// of [`WINDOW`] consecutive symbols in it, counting the run's start and
/// end as symbols, has been seen in a run the shaper drew as the
/// characters' own glyphs at their cells. Windows are learned from runs
/// shaped normally, so text a font joins or changes never qualifies: a
/// ligature (`->`), or a contextual alternate.
///
/// Pairs are not enough. A contextual alternate can depend on both
/// neighbours: JetBrains Mono raises the colon in `12:30`, yet draws `2:`
/// and `:3` plain. A three-character ligature can depend on the
/// characters before and after it (`===` in JetBrains Mono, `***` in Fira
/// Code), so a window is five symbols. Windows of five found no case
/// drawn differently in the shipped ligature fonts (JetBrains Mono, Fira
/// Code, Cascadia Code), where windows of four did. A font whose rules
/// reach further could still be drawn differently here; the fast path is
/// off unless `DREAMER_RENDER_ASCII` asks for it.
struct AsciiFont {
    font_id: gpui::FontId,
    ascent: Pixels,
    descent: Pixels,
    glyphs: [Option<gpui::GlyphId>; ASCII_COUNT],
    /// Where the shaper puts the k-th glyph of a run of same-advance
    /// glyphs forced to the cell width. Not always `k` cells: the text
    /// system keeps the font's own position while it is within a pixel of
    /// the cell, so these are taken from the shaper itself.
    positions: Vec<Pixels>,
    /// Windows seen drawn apart, each as a base-96 number.
    seen: std::collections::HashSet<u64, std::hash::BuildHasherDefault<WindowHasher>>,
}

/// Mixes a window's number so that every bit reaches the low bits a hash
/// table indexes by (they would otherwise hold only the last symbol).
#[derive(Default)]
struct WindowHasher(u64);

impl std::hash::Hasher for WindowHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u64(&mut self, v: u64) {
        let x = (self.0 ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 = x ^ (x >> 29);
    }
}

/// The longest run drawn without shaping.
const ASCII_RUN_MAX: usize = 1024;

impl AsciiFont {
    /// Each printable character shaped on its own; `None` when the font
    /// draws none of them itself.
    fn learn(
        font: &Font,
        font_size: Pixels,
        width: Pixels,
        text_system: &gpui::WindowTextSystem,
    ) -> Option<Self> {
        let mut alone = [None; ASCII_COUNT];
        let mut primary = None;
        let mut metrics = None;
        for (i, slot) in alone.iter_mut().enumerate() {
            let ch = (ASCII_FIRST + i as u8) as char;
            let text: SharedString = ch.to_string().into();
            let run = TextRun {
                len: 1,
                font: font.clone(),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = text_system.shape_line(text, font_size, &[run], Some(width));
            let [run] = line.runs.as_slice() else {
                continue;
            };
            let [glyph] = run.glyphs.as_slice() else {
                continue;
            };
            // The font the run asked for is the one most characters use;
            // a character drawn from a fallback is left to the shaper.
            if ch == 'M' {
                primary = Some(run.font_id);
                metrics = Some((line.ascent, line.descent));
            }
            *slot = Some((run.font_id, glyph.id, glyph.position));
        }
        let font_id = primary?;
        let (ascent, descent) = metrics?;
        // Positions in a run, from the shaper: `M` repeated, which no font
        // joins, with every glyph at the baseline.
        let text: SharedString = "M".repeat(ASCII_RUN_MAX).into();
        let run = TextRun {
            len: ASCII_RUN_MAX,
            font: font.clone(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = text_system.shape_line(text, font_size, &[run], Some(width));
        let placed: Vec<&gpui::ShapedGlyph> = line.runs.iter().flat_map(|r| &r.glyphs).collect();
        if placed.len() != ASCII_RUN_MAX
            || line.runs.iter().any(|r| r.font_id != font_id)
            || placed.iter().any(|g| g.position.y != px(0.0))
        {
            return None;
        }
        let positions: Vec<Pixels> = placed.iter().map(|g| g.position.x).collect();
        // Only characters as wide as `M` sit where `M` does.
        let m_advance = text_system.advance(font_id, font_size, 'M').ok()?.width;
        let glyphs = std::array::from_fn(|i| {
            let ch = (ASCII_FIRST + i as u8) as char;
            match alone[i] {
                Some((f, id, _))
                    if f == font_id
                        && text_system
                            .advance(font_id, font_size, ch)
                            .is_ok_and(|a| a.width == m_advance) =>
                {
                    Some(id)
                }
                _ => None,
            }
        });
        let mut f = AsciiFont {
            font_id,
            ascent,
            descent,
            glyphs,
            positions,
            seen: Default::default(),
        };
        // A run of one character, drawn alone above.
        for (i, a) in alone.iter().enumerate() {
            if let Some((_, _, at)) = a
                && f.glyphs[i].is_some()
                && *at == point(f.positions[0], px(0.0))
            {
                Self::windows(&[ASCII_FIRST + i as u8], |w| {
                    f.seen.insert(w);
                    true
                });
            }
        }
        Some(f)
    }

    fn index(ch: u8) -> Option<usize> {
        let i = ch.wrapping_sub(ASCII_FIRST) as usize;
        (i < ASCII_COUNT).then_some(i)
    }

    /// Calls `f` with each window of [`WINDOW`] symbols over `bytes` and
    /// its two edges (a shorter run is one window, padded with edges)
    /// until `f` returns false. False when it did, or a byte is not
    /// printable ASCII.
    fn windows(bytes: &[u8], mut f: impl FnMut(u64) -> bool) -> bool {
        let symbol = |k: usize| match k.checked_sub(1).and_then(|k| bytes.get(k)) {
            Some(&b) => Self::index(b).map(|i| i as u64),
            None => Some(EDGE),
        };
        let symbols = (bytes.len() + 2).max(WINDOW);
        (0..=symbols - WINDOW).all(|start| {
            (start..start + WINDOW)
                .try_fold(0u64, |key, k| Some(key * (EDGE + 1) + symbol(k)?))
                .is_some_and(&mut f)
        })
    }

    /// Glyphs for `text` at successive cells, if it may skip shaping.
    fn place(&self, text: &str, mode: AsciiMode) -> Option<Vec<(Pixels, gpui::GlyphId)>> {
        let bytes = text.as_bytes();
        if mode == AsciiMode::Off
            || bytes.len() > ASCII_RUN_MAX
            || (mode == AsciiMode::Learned && !Self::windows(bytes, |w| self.seen.contains(&w)))
        {
            return None;
        }
        let mut out = Vec::with_capacity(bytes.len());
        for (col, &b) in bytes.iter().enumerate() {
            let glyph = self.glyphs[Self::index(b)?]?;
            // A space takes its cell and draws nothing.
            if b != b' ' {
                out.push((self.positions[col], glyph));
            }
        }
        Some(out)
    }

    /// After `text` was shaped into `line`: if the shaper drew exactly the
    /// characters' own glyphs, one per cell, every window in it is safe
    /// from now on.
    fn observe(&mut self, text: &str, line: &gpui::ShapedLine) {
        let bytes = text.as_bytes();
        if bytes.len() > ASCII_RUN_MAX || self.seen.len() >= WINDOWS_MAX {
            return;
        }
        let mut glyphs = line.runs.iter().flat_map(|r| {
            r.glyphs
                .iter()
                .map(move |g| (r.font_id, g.id, g.index, g.position.x, g.position.y))
        });
        for (at, &b) in bytes.iter().enumerate() {
            let Some(i) = Self::index(b) else { return };
            let Some(own) = self.glyphs[i] else { return };
            match glyphs.next() {
                Some((f, id, index, x, y))
                    if f == self.font_id
                        && id == own
                        && index == at
                        && self.positions.get(at) == Some(&x)
                        && y == px(0.0) => {}
                _ => return,
            }
        }
        if glyphs.next().is_some() {
            return;
        }
        Self::windows(bytes, |w| {
            self.seen.insert(w);
            true
        });
    }
}

/// Shaped rows by shared row identity (unchanged rows are shared
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
                        body: Body::Line(Box::new(line)),
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
                    text.push_str(if t.is_empty() || is_image_placeholder(t) {
                        " "
                    } else {
                        t
                    });
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
                let key = (run.font.clone(), el.font_size, m.width);
                let font = run.font.clone();
                let fast = !decorated && ascii_mode() != AsciiMode::Off;
                if fast
                    && let Some(placed) = ASCII.with(|a| {
                        let mut a = a.borrow_mut();
                        let entry = a.entry(key.clone()).or_insert_with(|| {
                            AsciiFont::learn(&font, el.font_size, m.width, window.text_system())
                        });
                        let f = entry.as_ref()?;
                        Some(PlacedGlyphs {
                            font_id: f.font_id,
                            font_size: el.font_size,
                            ascent: f.ascent,
                            descent: f.descent,
                            color: fg,
                            glyphs: f.place(&text, ascii_mode())?,
                        })
                    })
                {
                    FAST_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    out.push(Shaped {
                        col: start,
                        body: Body::Glyphs(placed),
                        wide: false,
                        decorated,
                    });
                    continue;
                }
                let line = window.text_system().shape_line(
                    text.clone().into(),
                    el.font_size,
                    &[run],
                    Some(m.width),
                );
                if fast && text.is_ascii() {
                    ASCII.with(|a| {
                        if let Some(Some(f)) = a.borrow_mut().get_mut(&key) {
                            f.observe(&text, &line);
                        }
                    });
                }
                out.push(Shaped {
                    col: start,
                    body: Body::Line(Box::new(line)),
                    wide: false,
                    decorated,
                });
            }
        }
    }
    out
}

/// A Kitty Unicode placeholder cell (U+10EEEE and its diacritics): the
/// engine places the image under it, so the cell paints no glyph.
fn is_image_placeholder(text: &str) -> bool {
    text.starts_with('\u{10EEEE}')
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

    // Layer order: default background, images below cell
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
                let started = std::time::Instant::now();
                let lines = std::rc::Rc::new(shape_row(row, el, m, colors, window));
                SHAPE_NANOS.fetch_add(
                    started.elapsed().as_nanos() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                stats.shaped_lines += lines.len();
                SHAPES.with(|c| c.borrow_mut().insert(row, colors, key, lines.clone()));
                lines
            }
        };
        for s in lines.iter() {
            let at = point(origin.x + m.width * s.col as f32, top);
            match &s.body {
                Body::Line(line) => {
                    let _ = line.paint(at, m.height, TextAlign::Left, None, window, cx);
                }
                Body::Glyphs(g) => {
                    // The baseline a shaped line of this font would use.
                    let baseline = top + (m.height - g.ascent - g.descent) / 2.0 + g.ascent;
                    for (x, glyph) in &g.glyphs {
                        let _ = window.paint_glyph(
                            point(at.x + *x, baseline),
                            g.font_id,
                            *glyph,
                            g.font_size,
                            g.color,
                        );
                    }
                }
            }
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

/// Decode a PNG or JPEG file's bytes into a GPUI image. Slow for a large
/// image: call it off the UI thread.
pub fn decode_image(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg) {
        return Err("not a PNG or JPEG".into());
    }
    let decoded = image::load_from_memory_with_format(bytes, format).map_err(|e| e.to_string())?;
    let rgba = decoded.into_rgba8();
    let (w, h) = rgba.dimensions();
    rgba_image(w, h, rgba.into_raw()).ok_or_else(|| "an image of no size".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A font whose every printable character has glyph id = its code, at
    /// 8 px a cell.
    fn ascii_font() -> AsciiFont {
        AsciiFont {
            font_id: gpui::FontId(0),
            ascent: px(10.0),
            descent: px(3.0),
            glyphs: std::array::from_fn(|i| Some(gpui::GlyphId(u32::from(ASCII_FIRST) + i as u32))),
            positions: (0..ASCII_RUN_MAX).map(|k| px(8.0 * k as f32)).collect(),
            seen: Default::default(),
        }
    }

    fn seen_apart(f: &mut AsciiFont, text: &str) {
        AsciiFont::windows(text.as_bytes(), |w| {
            f.seen.insert(w);
            true
        });
    }

    #[test]
    fn ascii_runs_skip_shaping_only_through_windows_seen_shaped_apart() {
        use AsciiMode::{All, Learned, Off};
        let mut f = ascii_font();
        assert!(f.place("a", Learned).is_none());
        assert!(f.place("a", All).is_some());
        seen_apart(&mut f, "ab b");
        let placed = f.place("ab b", Learned).expect("every window seen");
        // Spaces take a cell but draw nothing.
        assert_eq!(
            placed,
            vec![
                (px(0.0), gpui::GlyphId(u32::from(b'a'))),
                (px(8.0), gpui::GlyphId(u32::from(b'b'))),
                (px(24.0), gpui::GlyphId(u32::from(b'b'))),
            ]
        );
        assert!(f.place("ab b", Off).is_none());
        // The same characters at a run's edge are other windows.
        assert!(f.place("ab", Learned).is_none());
        // Every pair in `12:30` seen apart is not enough: a font may
        // change the colon only between digits.
        for t in ["12 ", "2: ", " :3", "30"] {
            seen_apart(&mut f, t);
        }
        assert!(f.place("12:30", Learned).is_none());
        // Outside printable ASCII, or a character the font lacks: shaped.
        assert!(f.place("a\u{e9}", All).is_none());
        f.glyphs[AsciiFont::index(b'z').expect("ascii")] = None;
        assert!(f.place("z", All).is_none());
    }

    /// Fonts Dreamer ships, with ligatures and contextual alternates:
    /// family, regular file.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const SHIPPED: [(&str, &str); 2] = [
        (
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMonoNerdFontMono-Regular.ttf",
        ),
        (
            "FiraCode Nerd Font Mono",
            "FiraCodeNerdFontMono-Regular.ttf",
        ),
    ];

    /// A shipped font file, from `DREAMER_FONT_DIR` or where `cargo xtask
    /// deps prepare` puts it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn shipped_font(file: &str) -> Vec<u8> {
        use std::path::PathBuf;
        let mut dirs: Vec<PathBuf> = std::env::var_os("DREAMER_FONT_DIR")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cache = if cfg!(target_os = "macos") {
            home.map(|h| h.join("Library/Caches"))
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".cache")))
        };
        if let Some(Ok(entries)) =
            cache.map(|c| std::fs::read_dir(c.join("dreamer/deps/fonts/nerdfonts")))
        {
            let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            found.sort();
            dirs.extend(found.into_iter().rev());
        }
        dirs.iter()
            .find_map(|d| std::fs::read(d.join(file)).ok())
            .unwrap_or_else(|| {
                panic!("{file} not found: run `cargo xtask deps prepare` or set DREAMER_FONT_DIR")
            })
    }

    /// The platform's own shaper, as the desktop uses it, with the shipped
    /// fonts. No window or display is needed.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn platform_text_system() -> gpui::WindowTextSystem {
        #[cfg(target_os = "linux")]
        let platform: Arc<dyn gpui::PlatformTextSystem> = Arc::new(
            gpui_wgpu::CosmicTextSystem::new_without_system_fonts(SHIPPED[0].0),
        );
        #[cfg(target_os = "macos")]
        let platform: Arc<dyn gpui::PlatformTextSystem> =
            Arc::new(gpui_apple::AppleTextSystem::new());
        let text_system = Arc::new(gpui::TextSystem::new(platform));
        text_system
            .add_fonts(
                SHIPPED
                    .iter()
                    .map(|(_, file)| shipped_font(file).into())
                    .collect(),
            )
            .expect("the shipped fonts load");
        gpui::WindowTextSystem::new(text_system)
    }

    /// Pixel-identical by construction: a placed run issues the same
    /// glyph paints a shaped line does (font, glyph, size, color, and
    /// origin; a space's glyph draws nothing), so it is enough that every
    /// run the fast path takes gets exactly the shaper's glyphs, positions,
    /// and baseline. Text is full of what fonts join or change: operators,
    /// times, hex numbers, and runs of punctuation.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn placed_ascii_runs_are_what_the_shaper_draws() {
        let text_system = platform_text_system();
        let size = px(14.0);
        // Symbols the shipped fonts join or change in some context.
        let alphabet = b"0123x:/\\|-=<>!_.* w";
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut line = |len: usize| -> String {
            (0..len)
                .map(|_| alphabet[random(alphabet.len())] as char)
                .collect()
        };
        let train: Vec<String> = (0..12000).map(|k| line(1 + k % 8)).collect();
        let mut test: Vec<String> = (0..4000).map(|k| line(2 + k % 9)).collect();
        test.extend(
            [
                "12:30", " /\\ ", "www", "0x1F", "a->b", "x != y", "===", "<!--", "1:2", "||",
            ]
            .map(String::from),
        );
        for (k, (family, _)) in SHIPPED.into_iter().enumerate() {
            // Letter spacing moves the cells off the font's advance.
            let spacings: &[f32] = if k == 0 { &[0.0, 1.0] } else { &[0.0] };
            for &spacing in spacings {
                let font = gpui::font(family);
                let id = text_system.resolve_font(&font);
                let width =
                    text_system.advance(id, size, 'M').expect("advance").width + px(spacing);
                let shape = |text: &str| {
                    let run = TextRun {
                        len: text.len(),
                        font: font.clone(),
                        color: Hsla::default(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    text_system.shape_line(text.to_owned().into(), size, &[run], Some(width))
                };
                let mut f = AsciiFont::learn(&font, size, width, &text_system)
                    .expect("the font draws ASCII itself");
                for t in &train {
                    f.observe(t, &shape(t));
                }
                let mut placed = 0;
                for t in &test {
                    let Some(glyphs) = f.place(t, AsciiMode::Learned) else {
                        continue;
                    };
                    placed += 1;
                    let line = shape(t);
                    let drawn: Vec<_> = line
                        .runs
                        .iter()
                        .flat_map(|r| r.glyphs.iter().map(move |g| (r.font_id, g)))
                        .filter(|(_, g)| t.as_bytes().get(g.index) != Some(&b' '))
                        .map(|(font_id, g)| (font_id, g.position, g.id))
                        .collect();
                    let ours: Vec<_> = glyphs
                        .iter()
                        .map(|&(x, glyph)| (f.font_id, point(x, px(0.0)), glyph))
                        .collect();
                    assert_eq!(
                        ours, drawn,
                        "{family} +{spacing} px: `{t}` placed unlike the shaper"
                    );
                    assert_eq!((f.ascent, f.descent), (line.ascent, line.descent));
                }
                eprintln!(
                    "{family} +{spacing} px: {placed} of {} runs placed",
                    test.len()
                );
                // Enough runs skipped shaping for the comparison to mean something.
                assert!(
                    placed > test.len() / 20,
                    "{family}: only {placed} runs placed"
                );
            }
        }
    }

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

    #[test]
    fn a_png_decodes_and_other_formats_do_not() {
        let mut bytes = Vec::new();
        image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255])
            .expect("pixels")
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode");
        let decoded = decode_image(&bytes).expect("a PNG");
        assert_eq!(decoded.size(0).width.0, 2);
        assert!(decode_image(b"GIF89a....").is_err());
        assert!(decode_image(&bytes[..20]).is_err(), "a cut file");
    }
}
