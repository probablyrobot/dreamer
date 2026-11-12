//! Local text selection (spec 9.3, 11.5): Ghostty's selection gestures on
//! the engine, the selected text, and nothing sent anywhere. A selection
//! is view state; it never changes what the terminal holds.
//!
//! Positions are viewport cells. Drags also carry a position in canonical
//! cell units (fractions allowed, outside the grid allowed) so Ghostty can
//! tell which side of a cell the pointer is on and when to autoscroll.

use std::ptr;

use ghostty_sys::*;
use terminal_core::family::CANONICAL_CELL_PX;

use crate::{Engine, EngineError, borrowed_str, check};

/// One pointer event of a selection gesture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectEvent {
    /// Button down at a cell. `time_ns` is a monotonic time used to count
    /// repeat clicks (double-click word, triple-click line).
    Press { at: CellPos, time_ns: u64 },
    /// Pointer moved with the button down.
    Drag { at: CellPos },
    /// Button up.
    Release { at: CellPos },
    /// Continue a drag held past the top or bottom edge: scrolls one row.
    AutoscrollTick { at: CellPos },
}

/// A pointer position in viewport cells: `x`, `y` may be fractional and
/// lie outside the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellPos {
    pub x: f32,
    pub y: f32,
}

/// Which way a drag past an edge wants the viewport to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autoscroll {
    None,
    Up,
    Down,
}

/// Repeat clicks closer than this count as double and triple clicks.
const REPEAT_INTERVAL_NS: u64 = 500_000_000;

impl Engine {
    /// Apply a selection gesture event and install the result as the
    /// terminal's selection (a press that selects nothing clears it).
    pub fn select(&mut self, event: SelectEvent) -> Result<(), EngineError> {
        use GhosttySelectionGestureEventOption::*;
        use GhosttySelectionGestureEventType::*;
        let (kind, at, time) = match event {
            SelectEvent::Press { at, time_ns } => (
                GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS,
                at,
                Some(time_ns),
            ),
            SelectEvent::Drag { at } => (GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG, at, None),
            SelectEvent::Release { at } => (GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE, at, None),
            SelectEvent::AutoscrollTick { at } => (
                GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK,
                at,
                None,
            ),
        };
        let (cw, ch) = CANONICAL_CELL_PX;
        let size = self.size();
        let cell = GhosttyPointCoordinate {
            x: (at.x.max(0.0) as u16).min(size.cols.saturating_sub(1)),
            y: u32::from((at.y.max(0.0) as u16).min(size.rows.saturating_sub(1))),
        };
        let mut grid_ref = GhosttyGridRef::default();
        let position = GhosttySurfacePosition {
            x: f64::from(at.x) * f64::from(cw),
            y: f64::from(at.y) * f64::from(ch),
        };
        let geometry = GhosttySelectionGestureGeometry {
            columns: u32::from(size.cols),
            cell_width: cw,
            padding_left: 0,
            screen_height: u32::from(size.rows) * ch,
        };
        let repeat_distance = f64::from(cw);
        let repeat_interval = REPEAT_INTERVAL_NS;
        let mut ev: GhosttySelectionGestureEvent = ptr::null_mut();
        let mut out = GhosttySelection::default();
        self.selection_rev += 1;
        // SAFETY: `term` is live; the gesture and event are created here or
        // owned by the engine, the event is freed on every path, and option
        // values point at locals that outlive the calls reading them.
        unsafe {
            if self.gesture.is_null() {
                check(
                    "ghostty_selection_gesture_new",
                    ghostty_selection_gesture_new(ptr::null(), &mut self.gesture),
                )?;
            }
            check(
                "ghostty_terminal_grid_ref",
                ghostty_terminal_grid_ref(
                    self.term,
                    GhosttyPoint {
                        tag: GhosttyPointTag::GHOSTTY_POINT_TAG_VIEWPORT,
                        value: GhosttyPointValue { coordinate: cell },
                    },
                    &mut grid_ref,
                ),
            )?;
            check(
                "ghostty_selection_gesture_event_new",
                ghostty_selection_gesture_event_new(ptr::null(), &mut ev, kind),
            )?;
            let set = |opt, value: *const std::ffi::c_void| {
                check(
                    "ghostty_selection_gesture_event_set",
                    ghostty_selection_gesture_event_set(ev, opt, value),
                )
            };
            let result = (|| {
                match event {
                    SelectEvent::Press { .. } => {
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                            (&raw const grid_ref).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                            (&raw const position).cast(),
                        )?;
                        if let Some(t) = &time {
                            set(
                                GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                                (t as *const u64).cast(),
                            )?;
                        }
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                            (&raw const repeat_interval).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                            (&raw const repeat_distance).cast(),
                        )?;
                    }
                    SelectEvent::Drag { .. } => {
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                            (&raw const grid_ref).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                            (&raw const position).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                            (&raw const geometry).cast(),
                        )?;
                    }
                    SelectEvent::Release { .. } => {
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                            (&raw const grid_ref).cast(),
                        )?;
                    }
                    SelectEvent::AutoscrollTick { .. } => {
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_VIEWPORT,
                            (&raw const cell).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                            (&raw const position).cast(),
                        )?;
                        set(
                            GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                            (&raw const geometry).cast(),
                        )?;
                    }
                }
                Ok(ghostty_selection_gesture_event(
                    self.gesture,
                    self.term,
                    ev,
                    &mut out,
                ))
            })();
            ghostty_selection_gesture_event_free(ev);
            let code = result?;
            if code == GhosttyResult::GHOSTTY_SUCCESS {
                check(
                    "ghostty_terminal_set(selection)",
                    ghostty_terminal_set(
                        self.term,
                        GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SELECTION,
                        (&raw const out).cast(),
                    ),
                )
            } else if code == GhosttyResult::GHOSTTY_NO_VALUE {
                if matches!(event, SelectEvent::Press { .. }) {
                    self.clear_selection()?;
                }
                Ok(())
            } else {
                check("ghostty_selection_gesture_event", code)
            }
        }
    }

    /// Whether the current drag wants the viewport to scroll.
    pub fn selection_autoscroll(&self) -> Result<Autoscroll, EngineError> {
        if self.gesture.is_null() {
            return Ok(Autoscroll::None);
        }
        let mut dir: GhosttySelectionGestureAutoscroll::Type = 0;
        // SAFETY: gesture and term are live; the key writes one enum value.
        check("ghostty_selection_gesture_get(autoscroll)", unsafe {
            ghostty_selection_gesture_get(
                self.gesture,
                self.term,
                GhosttySelectionGestureData::GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL,
                (&raw mut dir).cast(),
            )
        })?;
        Ok(match dir {
            GhosttySelectionGestureAutoscroll::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_UP => {
                Autoscroll::Up
            }
            GhosttySelectionGestureAutoscroll::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_DOWN => {
                Autoscroll::Down
            }
            _ => Autoscroll::None,
        })
    }

    pub fn clear_selection(&mut self) -> Result<(), EngineError> {
        self.selection_rev += 1;
        // SAFETY: `term` is live; NULL clears the selection.
        check("ghostty_terminal_set(selection)", unsafe {
            ghostty_terminal_set(
                self.term,
                GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SELECTION,
                ptr::null(),
            )
        })
    }

    /// The selected text: wrapped lines joined, trailing blanks trimmed.
    /// `None` without a selection.
    pub fn selection_text(&self) -> Result<Option<String>, EngineError> {
        let mut sel = GhosttySelection::default();
        // SAFETY: `term` is live; the selection snapshot is used before any
        // terminal mutation; the formatter is freed on every path and its
        // output is copied and freed with the default allocator.
        unsafe {
            let code = ghostty_terminal_get(
                self.term,
                GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SELECTION,
                (&raw mut sel).cast(),
            );
            if code == GhosttyResult::GHOSTTY_NO_VALUE {
                return Ok(None);
            }
            check("ghostty_terminal_get(selection)", code)?;
            let mut fmt: GhosttyFormatter = ptr::null_mut();
            check(
                "ghostty_formatter_terminal_new",
                ghostty_formatter_terminal_new(ptr::null(), &mut fmt, self.term),
            )?;
            let unwrap = true;
            let trim = true;
            let result = (|| {
                use GhosttyFormatterOption::*;
                check(
                    "ghostty_formatter_set(unwrap)",
                    ghostty_formatter_set(
                        fmt,
                        GHOSTTY_FORMATTER_OPT_UNWRAP,
                        (&raw const unwrap).cast(),
                    ),
                )?;
                check(
                    "ghostty_formatter_set(trim)",
                    ghostty_formatter_set(
                        fmt,
                        GHOSTTY_FORMATTER_OPT_TRIM,
                        (&raw const trim).cast(),
                    ),
                )?;
                check(
                    "ghostty_formatter_set(selection)",
                    ghostty_formatter_set(
                        fmt,
                        GHOSTTY_FORMATTER_OPT_SELECTION,
                        (&raw const sel).cast(),
                    ),
                )?;
                let mut out: *mut u8 = ptr::null_mut();
                let mut len = 0usize;
                let rc = ghostty_formatter_format_alloc(fmt, ptr::null(), &mut out, &mut len);
                let text = borrowed_str(out, len);
                if !out.is_null() {
                    ghostty_free(ptr::null(), out, len);
                }
                check("ghostty_formatter_format_alloc", rc)?;
                Ok(text)
            })();
            ghostty_formatter_free(fmt);
            result.map(Some)
        }
    }
}
