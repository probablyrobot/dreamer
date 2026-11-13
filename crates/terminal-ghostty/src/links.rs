//! OSC 8 hyperlink hit testing (spec 9.3, 15): which link, if any, is under
//! a viewport cell, and which cells it covers. Hit testing only; opening a
//! link is always the user's explicit action, decided elsewhere.

use std::ptr;

use ghostty_sys::*;
use terminal_core::SelectedCells;

use crate::{Engine, EngineError, check};

/// A hyperlink under the pointer and the viewport cells it covers (the
/// contiguous run with the same URI, across soft-wrapped rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub uri: String,
    pub cells: Vec<SelectedCells>,
}

/// Longest URI accepted; longer ones are treated as no link.
const MAX_URI: usize = 8 << 10;

impl Engine {
    fn viewport_ref(&self, col: u16, row: u16) -> Result<GhosttyGridRef, EngineError> {
        let mut r = GhosttyGridRef::default();
        // SAFETY: `term` is live; the point is passed by value and the ref
        // is a snapshot used before the next terminal mutation.
        check("ghostty_terminal_grid_ref", unsafe {
            ghostty_terminal_grid_ref(
                self.term,
                GhosttyPoint {
                    tag: GhosttyPointTag::GHOSTTY_POINT_TAG_VIEWPORT,
                    value: GhosttyPointValue {
                        coordinate: GhosttyPointCoordinate {
                            x: col,
                            y: u32::from(row),
                        },
                    },
                },
                &mut r,
            )
        })?;
        Ok(r)
    }

    /// The URI of the link at a viewport cell, if it has one.
    pub fn uri_at(&self, col: u16, row: u16) -> Result<Option<String>, EngineError> {
        let r = self.viewport_ref(col, row)?;
        let mut len = 0usize;
        // SAFETY: `r` is a fresh snapshot; the buffer is sized by the first
        // call and the bytes are copied before returning.
        unsafe {
            let code = ghostty_grid_ref_hyperlink_uri(&r, ptr::null_mut(), 0, &mut len);
            if code == GhosttyResult::GHOSTTY_SUCCESS || len == 0 {
                return Ok(None);
            }
            if code != GhosttyResult::GHOSTTY_OUT_OF_SPACE || len > MAX_URI {
                return Ok(None);
            }
            let mut buf = vec![0u8; len];
            check(
                "ghostty_grid_ref_hyperlink_uri",
                ghostty_grid_ref_hyperlink_uri(&r, buf.as_mut_ptr(), buf.len(), &mut len),
            )?;
            buf.truncate(len);
            Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
        }
    }

    /// Whether a viewport row continues on the next row (soft wrap).
    fn row_wraps(&self, row: u16) -> Result<bool, EngineError> {
        let r = self.viewport_ref(0, row)?;
        let mut raw: GhosttyRow = 0;
        let mut wrap = false;
        // SAFETY: `r` is a fresh snapshot; each call writes its documented
        // output type.
        unsafe {
            check("ghostty_grid_ref_row", ghostty_grid_ref_row(&r, &mut raw))?;
            check(
                "ghostty_row_get(wrap)",
                ghostty_row_get(
                    raw,
                    GhosttyRowData::GHOSTTY_ROW_DATA_WRAP,
                    (&raw mut wrap).cast(),
                ),
            )?;
        }
        Ok(wrap)
    }

    /// The link under a viewport cell and every viewport cell it covers.
    pub fn link_at(&self, col: u16, row: u16) -> Result<Option<Link>, EngineError> {
        let size = self.size();
        if col >= size.cols || row >= size.rows {
            return Ok(None);
        }
        let Some(uri) = self.uri_at(col, row)? else {
            return Ok(None);
        };
        let cols = usize::from(size.cols);
        let at = |i: usize| ((i % cols) as u16, (i / cols) as u16);
        let same = |i: usize| -> Result<bool, EngineError> {
            let (c, r) = at(i);
            Ok(self.uri_at(c, r)?.as_deref() == Some(uri.as_str()))
        };
        // Walk left and right; crossing a row boundary only along a wrap.
        let here = usize::from(row) * cols + usize::from(col);
        let mut first = here;
        while first > 0 {
            let prev = first - 1;
            if first % cols == 0 && !self.row_wraps(at(prev).1)? {
                break;
            }
            if !same(prev)? {
                break;
            }
            first = prev;
        }
        let total = cols * usize::from(size.rows);
        let mut last = here;
        while last + 1 < total {
            let next = last + 1;
            if next % cols == 0 && !self.row_wraps(at(last).1)? {
                break;
            }
            if !same(next)? {
                break;
            }
            last = next;
        }
        let mut cells: Vec<SelectedCells> = Vec::new();
        for i in first..=last {
            let (c, r) = at(i);
            match cells.last_mut() {
                Some(s) if s.row == r => s.end = c,
                _ => cells.push(SelectedCells {
                    row: r,
                    start: c,
                    end: c,
                }),
            }
        }
        Ok(Some(Link { uri, cells }))
    }
}
