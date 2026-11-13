//! Copy Ghostty render state into owned, shared frame data.
//!
//! Dirty rows are rebuilt; clean rows are taken from the previous frame by
//! `Arc`. Engine dirty markers are cleared only after the whole frame has
//! been copied, so a failure leaves the damage for the next attempt.

use std::sync::Arc;

use ghostty_sys::GhosttyRenderStateData::*;
use ghostty_sys::GhosttyRenderStateDirty::*;
use ghostty_sys::GhosttyRenderStateRowCellsData::*;
use ghostty_sys::GhosttyRenderStateRowData::*;
use ghostty_sys::*;
use terminal_core::{
    Attrs, Cell, CellWidth, Color, Colors, Cursor, CursorShape, FrameSnapshot, GridSize,
    ImageLayer, ImagePixels, ImagePlacement, Rgb, Row, SelectedCells, Style, Underline,
};

use crate::{Engine, EngineError, check};

/// Reusable buffers for frame extraction.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    codepoints: Vec<u32>,
}

const UNDERLINE_SHIFT: u16 = 8;
const UNDERLINE_MASK: u16 = 0x7 << UNDERLINE_SHIFT;

pub(crate) fn build(e: &mut Engine) -> Result<Arc<FrameSnapshot>, EngineError> {
    // SAFETY (whole function): all handles are live and owned by `e`; every
    // out-pointer refers to a correctly typed, initialized local; borrowed
    // data (codepoints) is copied before the next library call.
    unsafe {
        check(
            "ghostty_render_state_update",
            ghostty_render_state_update(e.render, e.term),
        )?;

        let mut dirty: GhosttyRenderStateDirty::Type = 0;
        get(e.render, GHOSTTY_RENDER_STATE_DATA_DIRTY, &mut dirty)?;
        let mut cols: u16 = 0;
        let mut rows: u16 = 0;
        get(e.render, GHOSTTY_RENDER_STATE_DATA_COLS, &mut cols)?;
        get(e.render, GHOSTTY_RENDER_STATE_DATA_ROWS, &mut rows)?;
        let size = GridSize { cols, rows };
        let held = e.callbacks.held;

        let view = e.view_state()?;
        let mut placements_gen: u64 = 0;
        get(
            e.render,
            GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION,
            &mut placements_gen,
        )?;
        let images_changed = e.images.generation != Some(placements_gen);
        let previous = e.last.as_ref().filter(|f| f.size == size);
        if dirty == GHOSTTY_RENDER_STATE_DIRTY_FALSE
            && let Some(prev) = previous
            && prev.render_hold == held
            && prev.view == view
            && e.selection_rev == e.framed_selection_rev
            && !images_changed
        {
            return Ok(Arc::clone(prev));
        }
        let full = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL || previous.is_none();

        let mut raw_colors = GhosttyRenderStateColors {
            size: size_of::<GhosttyRenderStateColors>(),
            ..Default::default()
        };
        get(e.render, GHOSTTY_RENDER_STATE_DATA_COLORS, &mut raw_colors)?;
        let colors = convert_colors(&raw_colors);
        let colors = match previous {
            Some(prev) if *prev.colors == colors => Arc::clone(&prev.colors),
            _ => Arc::new(colors),
        };

        let mut raw_cursor = GhosttyRenderStateCursor {
            size: size_of::<GhosttyRenderStateCursor>(),
            ..Default::default()
        };
        get(e.render, GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut raw_cursor)?;
        let cursor = convert_cursor(&raw_cursor);

        get(
            e.render,
            GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
            &mut e.row_iter,
        )?;
        let mut out_rows = Vec::with_capacity(usize::from(rows));
        let mut damaged = Vec::new();
        let mut selection = Vec::new();
        let mut y: u16 = 0;
        while ghostty_render_state_row_iterator_next(e.row_iter) {
            let mut sel = GhosttyRenderStateRowSelection {
                size: size_of::<GhosttyRenderStateRowSelection>(),
                ..Default::default()
            };
            let code = ghostty_render_state_row_get(
                e.row_iter,
                GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                (&raw mut sel).cast(),
            );
            if code == GhosttyResult::GHOSTTY_SUCCESS {
                selection.push(SelectedCells {
                    row: y,
                    start: sel.start_x,
                    end: sel.end_x,
                });
            } else if code != GhosttyResult::GHOSTTY_NO_VALUE {
                check("ghostty_render_state_row_get(selection)", code)?;
            }
            let mut row_dirty = false;
            row_get(
                e.row_iter,
                GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                &mut row_dirty,
            )?;
            let reused = if full || row_dirty {
                None
            } else {
                previous.and_then(|p| p.rows.get(usize::from(y))).cloned()
            };
            let row = match reused {
                Some(row) => row,
                None => {
                    damaged.push(y);
                    row_get(
                        e.row_iter,
                        GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                        &mut e.row_cells,
                    )?;
                    Arc::new(read_row(e.row_cells, &mut e.scratch)?)
                }
            };
            out_rows.push(row);
            y = y.saturating_add(1);
        }

        check(
            "ghostty_render_state_clean",
            ghostty_render_state_clean(e.render),
        )?;
        let images = if images_changed {
            read_images(e, placements_gen)?
        } else {
            Arc::clone(&e.images.placements)
        };
        e.revision += 1;
        e.framed_selection_rev = e.selection_rev;
        Ok(Arc::new(FrameSnapshot {
            revision: e.revision,
            size,
            rows: out_rows,
            cursor,
            colors,
            damaged_rows: damaged,
            render_hold: held,
            view,
            selection,
            images,
        }))
    }
}

unsafe fn get<T>(
    rs: GhosttyRenderState,
    data: GhosttyRenderStateData::Type,
    out: &mut T,
) -> Result<(), EngineError> {
    check("ghostty_render_state_get", unsafe {
        ghostty_render_state_get(rs, data, (out as *mut T).cast())
    })
}

unsafe fn row_get<T>(
    it: GhosttyRenderStateRowIterator,
    data: GhosttyRenderStateRowData::Type,
    out: &mut T,
) -> Result<(), EngineError> {
    check("ghostty_render_state_row_get", unsafe {
        ghostty_render_state_row_get(it, data, (out as *mut T).cast())
    })
}

unsafe fn cell_get<T>(
    cells: GhosttyRenderStateRowCells,
    data: GhosttyRenderStateRowCellsData::Type,
    out: &mut T,
) -> Result<(), EngineError> {
    check("ghostty_render_state_row_cells_get", unsafe {
        ghostty_render_state_row_cells_get(cells, data, (out as *mut T).cast())
    })
}

unsafe fn read_row(
    cells: GhosttyRenderStateRowCells,
    scratch: &mut Scratch,
) -> Result<Row, EngineError> {
    let mut out = Vec::new();
    // SAFETY: see `build`.
    unsafe {
        while ghostty_render_state_row_cells_next(cells) {
            let mut len: u32 = 0;
            cell_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                &mut len,
            )?;
            let text: Box<str> = if len == 0 {
                Box::from("")
            } else {
                let want = len as usize;
                if scratch.codepoints.len() < want {
                    scratch.codepoints.resize(want, 0);
                }
                let mut buf = GhosttyCodepointBuffer {
                    ptr: scratch.codepoints.as_mut_ptr(),
                    cap: scratch.codepoints.len(),
                    len: 0,
                };
                cell_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                    &mut buf,
                )?;
                scratch.codepoints[..buf.len.min(buf.cap)]
                    .iter()
                    .map(|&cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect::<String>()
                    .into_boxed_str()
            };

            let mut style = GhosttyStyle::default();
            cell_get(cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &mut style)?;

            let mut raw: GhosttyCell = 0;
            cell_get(cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &mut raw)?;
            let mut wide: GhosttyCellWide::Type = 0;
            check("ghostty_cell_get(wide)", {
                ghostty_cell_get(
                    raw,
                    GhosttyCellData::GHOSTTY_CELL_DATA_WIDE,
                    (&raw mut wide).cast(),
                )
            })?;

            out.push(Cell {
                text,
                width: convert_width(wide),
                style: convert_style(&style),
            });
        }
    }
    Ok(Row { cells: out.into() })
}

fn convert_width(wide: GhosttyCellWide::Type) -> CellWidth {
    use ghostty_sys::GhosttyCellWide::*;
    match wide {
        GHOSTTY_CELL_WIDE_WIDE => CellWidth::Wide,
        GHOSTTY_CELL_WIDE_SPACER_TAIL => CellWidth::SpacerTail,
        GHOSTTY_CELL_WIDE_SPACER_HEAD => CellWidth::SpacerHead,
        _ => CellWidth::Narrow,
    }
}

fn convert_color(c: &GhosttyStyleColor) -> Color {
    use ghostty_sys::GhosttyStyleColorTag::*;
    // SAFETY: the tag selects the initialized union member.
    unsafe {
        match u32::from(c.tag) {
            GHOSTTY_STYLE_COLOR_PALETTE => Color::Palette(c.value.palette),
            GHOSTTY_STYLE_COLOR_RGB => {
                let v = c.value.rgb;
                Color::Rgb(Rgb {
                    r: v.r,
                    g: v.g,
                    b: v.b,
                })
            }
            _ => Color::Default,
        }
    }
}

fn convert_style(s: &GhosttyStyle) -> Style {
    let underline = match (s.flags & UNDERLINE_MASK) >> UNDERLINE_SHIFT {
        0 => Underline::None,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        5 => Underline::Dashed,
        _ => Underline::Single,
    };
    Style {
        fg: convert_color(&s.fg_color),
        bg: convert_color(&s.bg_color),
        underline_color: convert_color(&s.underline_color),
        // The low eight flag bits match `Attrs` bit for bit (style.h).
        attrs: Attrs((s.flags & 0xff) as u8),
        underline,
    }
}

fn rgb(c: GhosttyColorRgb) -> Rgb {
    Rgb {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

fn convert_colors(c: &GhosttyRenderStateColors) -> Colors {
    let mut palette = Box::new([Rgb::default(); 256]);
    for (dst, src) in palette.iter_mut().zip(c.palette.iter()) {
        *dst = rgb(*src);
    }
    Colors {
        foreground: rgb(c.foreground),
        background: rgb(c.background),
        cursor: c.cursor_has_value.then(|| rgb(c.cursor)),
        palette,
    }
}

fn convert_cursor(c: &GhosttyRenderStateCursor) -> Cursor {
    use ghostty_sys::GhosttyCursorStyle::*;
    Cursor {
        visible: c.visible && c.viewport_has_value,
        x: c.viewport_x,
        y: c.viewport_y,
        shape: match c.visual_style {
            GHOSTTY_CURSOR_STYLE_BAR => CursorShape::Bar,
            GHOSTTY_CURSOR_STYLE_UNDERLINE => CursorShape::Underline,
            GHOSTTY_CURSOR_STYLE_BLOCK_HOLLOW => CursorShape::BlockHollow,
            _ => CursorShape::Block,
        },
        blinking: c.blinking,
        on_wide_tail: c.wide_tail,
    }
}

/// Owned image pixels by (image id, generation), the placements last read,
/// and the render-state placements generation they came from.
#[derive(Default)]
pub(crate) struct Images {
    generation: Option<u64>,
    placements: Arc<[ImagePlacement]>,
    pixels: std::collections::HashMap<(u32, u64), Arc<ImagePixels>>,
    /// Placements skipped because their image data was missing or invalid.
    pub(crate) skipped: u64,
}

/// Largest image accepted, in pixels (64 Mi pixels, 256 MiB as RGBA).
const MAX_IMAGE_PIXELS: u64 = 64 << 20;

unsafe fn image_get<T>(
    it: GhosttyRenderStateImageIterator,
    data: GhosttyRenderStateImageData::Type,
    out: &mut T,
) -> Result<(), EngineError> {
    check("ghostty_render_state_image_get", unsafe {
        ghostty_render_state_image_get(it, data, (out as *mut T).cast())
    })
}

/// Read the placements of the last render-state update; pixels are copied
/// once per (image id, generation) and shared with earlier frames.
unsafe fn read_images(
    e: &mut Engine,
    generation: u64,
) -> Result<Arc<[ImagePlacement]>, EngineError> {
    use GhosttyRenderStateImageData::*;
    // SAFETY (whole function): the iterator and graphics handles belong to
    // `e` and are read before any terminal mutation; pixel data is copied
    // out before returning.
    unsafe {
        get(
            e.render,
            GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR,
            &mut e.image_iter,
        )?;
        let mut graphics: GhosttyKittyGraphics = std::ptr::null_mut();
        let code = ghostty_terminal_get(
            e.term,
            GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
            (&raw mut graphics).cast(),
        );
        if code != GhosttyResult::GHOSTTY_SUCCESS {
            graphics = std::ptr::null_mut();
        }
        let mut out = Vec::new();
        let mut keep = std::collections::HashMap::new();
        while ghostty_render_state_image_iterator_next(e.image_iter) {
            let it = e.image_iter;
            let mut protocol: GhosttyImageProtocol::Type = 0;
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_PROTOCOL, &mut protocol)?;
            if protocol != GhosttyImageProtocol::GHOSTTY_IMAGE_PROTOCOL_KITTY || graphics.is_null()
            {
                e.images.skipped += 1;
                continue;
            }
            let (mut id, mut image_gen) = (0u32, 0u64);
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID, &mut id)?;
            image_get(
                it,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_GENERATION,
                &mut image_gen,
            )?;
            let pixels = match keep
                .get(&(id, image_gen))
                .or_else(|| e.images.pixels.get(&(id, image_gen)))
            {
                Some(p) => Arc::clone(p),
                None => match copy_pixels(graphics, e.instance, id, image_gen) {
                    Some(p) => Arc::new(p),
                    None => {
                        e.images.skipped += 1;
                        continue;
                    }
                },
            };
            keep.insert((id, image_gen), Arc::clone(&pixels));
            let mut layer: GhosttyImageLayer::Type = 0;
            let (mut z, mut col, mut row) = (0i32, 0i32, 0i32);
            let mut v = [0u32; 8];
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_LAYER, &mut layer)?;
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_Z, &mut z)?;
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL, &mut col)?;
            image_get(it, GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW, &mut row)?;
            for (slot, key) in v.iter_mut().zip([
                GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH,
                GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT,
            ]) {
                image_get(it, key, slot)?;
            }
            let layer = match layer {
                GhosttyImageLayer::GHOSTTY_IMAGE_LAYER_BELOW_BACKGROUND => {
                    ImageLayer::BelowBackground
                }
                GhosttyImageLayer::GHOSTTY_IMAGE_LAYER_BELOW_TEXT => ImageLayer::BelowText,
                _ => ImageLayer::AboveText,
            };
            out.push(ImagePlacement {
                image: pixels,
                layer,
                z,
                col,
                row,
                offset_x: v[0],
                offset_y: v[1],
                width: v[2],
                height: v[3],
                source_x: v[4],
                source_y: v[5],
                source_width: v[6],
                source_height: v[7],
            });
        }
        // Draw order: by layer, then z; equal z keeps the library's order.
        out.sort_by_key(|p| (p.layer as u8, p.z));
        let placements: Arc<[ImagePlacement]> = out.into();
        e.images.pixels = keep;
        e.images.generation = Some(generation);
        e.images.placements = Arc::clone(&placements);
        Ok(placements)
    }
}

/// Copy an image's current pixels as RGBA, or `None` if they are missing,
/// too large, or inconsistent with the reported size and format.
unsafe fn copy_pixels(
    graphics: GhosttyKittyGraphics,
    engine: u64,
    id: u32,
    generation: u64,
) -> Option<ImagePixels> {
    use GhosttyKittyGraphicsImageData::*;
    use GhosttyKittyImageFormat::*;
    // SAFETY: `graphics` is live; the image handle and the borrowed pixel
    // string are used before any terminal mutation and copied here.
    unsafe {
        let image = ghostty_kitty_graphics_image(graphics, id);
        if image.is_null() {
            return None;
        }
        let read = |key, out: *mut std::ffi::c_void| {
            ghostty_kitty_graphics_image_get(image, key, out) == GhosttyResult::GHOSTTY_SUCCESS
        };
        let (mut width, mut height) = (0u32, 0u32);
        let mut format: GhosttyKittyImageFormat::Type = 0;
        let mut data = GhosttyString::default();
        if !(read(
            GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_WIDTH,
            (&raw mut width).cast(),
        ) && read(
            GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_HEIGHT,
            (&raw mut height).cast(),
        ) && read(
            GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_FORMAT,
            (&raw mut format).cast(),
        ) && read(
            GHOSTTY_KITTY_GRAPHICS_IMAGE_DATA_PIXELS,
            (&raw mut data).cast(),
        )) {
            return None;
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > MAX_IMAGE_PIXELS {
            return None;
        }
        let bpp = match format {
            GHOSTTY_KITTY_IMAGE_FORMAT_RGBA => 4,
            GHOSTTY_KITTY_IMAGE_FORMAT_RGB => 3,
            GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA => 2,
            GHOSTTY_KITTY_IMAGE_FORMAT_GRAY => 1,
            _ => return None,
        };
        let pixels = pixels as usize;
        if data.ptr.is_null() || data.len != pixels * bpp {
            return None;
        }
        let src = std::slice::from_raw_parts(data.ptr, data.len);
        let mut rgba = Vec::with_capacity(pixels * 4);
        match bpp {
            4 => rgba.extend_from_slice(src),
            3 => src
                .as_chunks::<3>()
                .0
                .iter()
                .for_each(|[r, g, b]| rgba.extend_from_slice(&[*r, *g, *b, 255])),
            2 => src
                .as_chunks::<2>()
                .0
                .iter()
                .for_each(|[g, a]| rgba.extend_from_slice(&[*g, *g, *g, *a])),
            _ => src
                .iter()
                .for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
        }
        Some(ImagePixels {
            engine,
            id,
            generation,
            width,
            height,
            rgba: rgba.into_boxed_slice(),
        })
    }
}
