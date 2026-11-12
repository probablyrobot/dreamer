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
    Attrs, Cell, CellWidth, Color, Colors, Cursor, CursorShape, FrameSnapshot, GridSize, Rgb, Row,
    SelectedCells, Style, Underline,
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
        let previous = e.last.as_ref().filter(|f| f.size == size);
        if dirty == GHOSTTY_RENDER_STATE_DIRTY_FALSE
            && let Some(prev) = previous
            && prev.render_hold == held
            && prev.view == view
            && e.selection_rev == e.framed_selection_rev
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
