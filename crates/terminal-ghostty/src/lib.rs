//! Safe terminal engine over the pinned `libghostty-vt`.
//!
//! [`Engine`] owns a Ghostty terminal and its render state. It ingests VT
//! bytes, reports side effects as owned [`Effects`], and produces immutable
//! [`FrameSnapshot`]s that share unchanged rows with the previous frame. No
//! foreign pointer escapes this crate.
//!
//! An engine is deliberately `!Send` and `!Sync`: Ghostty handles stay on the
//! worker that created them. Frames are plain owned data and may go anywhere.
//!
//! Effects are opt-in in Ghostty: a query or side effect with no registered
//! callback is ignored. Generated replies are therefore suppressed unless
//! [`EngineConfig::capture_replies`] is set, which is the default for a tmux
//! mirror (tmux answers the application itself).

// The only crate besides `ghostty-sys` allowed to use `unsafe`.
#![allow(unsafe_code)]

mod effects;
mod features;
mod frame;
pub mod input;
pub mod links;
pub mod selection;
pub mod snapshot_fixtures;

pub use features::{Features, features};
pub use terminal_core as core;

/// Where to move the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Top,
    Bottom,
    /// Rows; negative scrolls up into history.
    Delta(isize),
}

use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;

use ghostty_sys::GhosttyResult::{self, GHOSTTY_SUCCESS};
use ghostty_sys::*;
use terminal_core::{Effects, FrameSnapshot, GridSize};

use crate::effects::CallbackState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// A library call failed with the given Ghostty result code.
    Ghostty {
        call: &'static str,
        code: i32,
    },
    InvalidSize(GridSize),
    /// A Rust callback panicked inside the engine; the engine must be
    /// discarded because its state after the interrupted write is unknown.
    CallbackPanicked,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ghostty { call, code } => write!(f, "{call} failed with Ghostty result {code}"),
            Self::InvalidSize(s) => write!(f, "invalid terminal size {}x{}", s.cols, s.rows),
            Self::CallbackPanicked => write!(f, "a terminal callback panicked; engine discarded"),
        }
    }
}

impl std::error::Error for EngineError {}

pub(crate) fn check(call: &'static str, code: GhosttyResult::Type) -> Result<(), EngineError> {
    if code == GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(EngineError::Ghostty { call, code })
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub size: GridSize,
    /// Maximum scrollback lines; `None` keeps the library default.
    pub scrollback_lines: Option<usize>,
    /// Maximum scrollback memory in bytes; `None` keeps the library default,
    /// `Some(usize::MAX)` removes the byte limit.
    pub scrollback_bytes: Option<usize>,
    /// Track unfinished input (a partial escape or UTF-8 sequence) up to
    /// this many bytes so snapshots can carry it. Must be set before any
    /// input; `None` leaves tracking off.
    pub continuation_max_bytes: Option<usize>,
    /// Deliver engine-generated replies in [`Effects::replies`]. Off by
    /// default: in tmux mirror mode tmux owns query responses.
    pub capture_replies: bool,
}

impl EngineConfig {
    pub fn new(size: GridSize) -> Self {
        Self {
            size,
            scrollback_lines: None,
            scrollback_bytes: None,
            continuation_max_bytes: None,
            capture_replies: false,
        }
    }
}

/// A terminal engine instance. See the crate documentation.
///
/// Handles never leave the thread that created them:
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<terminal_ghostty::Engine>();
/// ```
pub struct Engine {
    term: GhosttyTerminal,
    render: GhosttyRenderState,
    row_iter: GhosttyRenderStateRowIterator,
    row_cells: GhosttyRenderStateRowCells,
    /// Boxed so its address, registered as callback userdata, never moves.
    callbacks: Box<CallbackState>,
    size: GridSize,
    revision: u64,
    last: Option<Arc<FrameSnapshot>>,
    scratch: frame::Scratch,
    /// Selection gesture state, created on first use.
    gesture: GhosttySelectionGesture,
    /// Bumped whenever the selection may have changed; the value the last
    /// frame was built at.
    selection_rev: u64,
    framed_selection_rev: u64,
    _not_send: PhantomData<*mut ()>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("size", &self.size)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let size = config.size;
        if size.cols == 0 || size.rows == 0 {
            return Err(EngineError::InvalidSize(size));
        }
        let mut term: GhosttyTerminal = ptr::null_mut();
        // SAFETY: valid out-pointer; NULL allocator selects the default.
        check("ghostty_terminal_new", unsafe {
            ghostty_terminal_new(ptr::null(), &mut term, size.cols, size.rows)
        })?;
        // SAFETY: `term` was just created and is owned by nobody else.
        unsafe { Self::adopt(term, &config) }
    }

    /// Restore an engine from a snapshot produced by [`Engine::snapshot`]
    /// with a compatible library build. Only the terminal state comes from
    /// the snapshot: callbacks, reply policy, and scrollback limits come from
    /// `config` (its `size` is ignored; the snapshot carries the grid).
    pub fn from_snapshot(bytes: &[u8], config: &EngineConfig) -> Result<Self, EngineError> {
        use ghostty_sys::GhosttySnapshotDecoderOption::*;
        let mut decoder: GhosttySnapshotDecoder = ptr::null_mut();
        let mut term: GhosttyTerminal = ptr::null_mut();
        // SAFETY: `bytes` outlives the decoder, which is freed before return;
        // options are set before decoding starts and point at locals.
        unsafe {
            check(
                "ghostty_snapshot_decoder_new_buf",
                ghostty_snapshot_decoder_new_buf(
                    ptr::null(),
                    &mut decoder,
                    bytes.as_ptr(),
                    bytes.len(),
                ),
            )?;
            let max = config.continuation_max_bytes.unwrap_or(0);
            let retain = max > 0;
            let set = |opt, value: *const std::ffi::c_void| {
                check(
                    "ghostty_snapshot_decoder_set",
                    ghostty_snapshot_decoder_set(decoder, opt, value),
                )
            };
            let result = set(
                GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                (&raw const max).cast(),
            )
            .and_then(|()| {
                set(
                    GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION,
                    (&raw const retain).cast(),
                )
            })
            .and_then(|()| {
                check(
                    "ghostty_snapshot_decoder_decode",
                    ghostty_snapshot_decoder_decode(decoder, &mut term),
                )
            });
            ghostty_snapshot_decoder_free(decoder);
            result?;
            Self::adopt(term, config)
        }
    }

    /// Encode the complete terminal state, including unfinished input when
    /// continuation tracking is enabled.
    pub fn snapshot(&self) -> Result<Vec<u8>, EngineError> {
        let mut out: *mut u8 = ptr::null_mut();
        let mut len = 0usize;
        // SAFETY: `term` is live and not mutated during the call; the buffer
        // is copied and freed with the same (default) allocator.
        unsafe {
            check(
                "ghostty_snapshot_encode_alloc",
                ghostty_snapshot_encode_alloc(self.term, ptr::null(), &mut out, &mut len),
            )?;
            let bytes = if out.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(out, len).to_vec()
            };
            ghostty_free(ptr::null(), out, len);
            Ok(bytes)
        }
    }

    /// Wrap a terminal handle: create its render state, apply options, and
    /// register callbacks.
    ///
    /// # Safety
    /// `term` must be a live terminal that nothing else owns; ownership moves
    /// into the returned engine (or it is freed on error).
    unsafe fn adopt(term: GhosttyTerminal, config: &EngineConfig) -> Result<Self, EngineError> {
        let mut engine = Engine {
            term,
            render: ptr::null_mut(),
            row_iter: ptr::null_mut(),
            row_cells: ptr::null_mut(),
            callbacks: Box::new(CallbackState::new(config.capture_replies)),
            size: config.size,
            revision: 0,
            last: None,
            scratch: frame::Scratch::default(),
            gesture: ptr::null_mut(),
            selection_rev: 0,
            framed_selection_rev: 0,
            _not_send: PhantomData,
        };
        // From here `Drop` frees whatever has been created.
        // SAFETY: valid out-pointers; NULL allocator selects the default.
        unsafe {
            check(
                "ghostty_render_state_new",
                ghostty_render_state_new(ptr::null(), &mut engine.render),
            )?;
            check(
                "ghostty_render_state_row_iterator_new",
                ghostty_render_state_row_iterator_new(ptr::null(), &mut engine.row_iter),
            )?;
            check(
                "ghostty_render_state_row_cells_new",
                ghostty_render_state_row_cells_new(ptr::null(), &mut engine.row_cells),
            )?;
        }
        let set_size = |opt, value: usize| {
            // SAFETY: size_t options read the pointed-to value during the call.
            check("set size option", unsafe {
                ghostty_terminal_set(engine.term, opt, (&raw const value).cast())
            })
        };
        if let Some(lines) = config.scrollback_lines {
            set_size(
                GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                lines,
            )?;
        }
        if let Some(bytes) = config.scrollback_bytes {
            set_size(
                GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                bytes,
            )?;
        }
        if let Some(bytes) = config.continuation_max_bytes {
            set_size(
                GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
                bytes,
            )?;
        }
        if let Some(bytes) = config.scrollback_bytes {
            // SAFETY: the option takes a pointer to a size_t read during the call.
            check("set scrollback bytes", unsafe {
                ghostty_terminal_set(
                    engine.term,
                    GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                    (&raw const bytes).cast(),
                )
            })?;
        }
        effects::register(engine.term, &mut engine.callbacks)?;
        let (mut cols, mut rows) = (0u16, 0u16);
        // SAFETY: `term` is live; each key writes one u16.
        unsafe {
            check(
                "ghostty_terminal_get(cols)",
                ghostty_terminal_get(
                    engine.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_COLS,
                    (&raw mut cols).cast(),
                ),
            )?;
            check(
                "ghostty_terminal_get(rows)",
                ghostty_terminal_get(
                    engine.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ROWS,
                    (&raw mut rows).cast(),
                ),
            )?;
        }
        engine.size = GridSize { cols, rows };
        Ok(engine)
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Ingest application output in order. Returns the side effects it
    /// produced, copied out of the engine.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Effects, EngineError> {
        if self.callbacks.panicked {
            return Err(EngineError::CallbackPanicked);
        }
        // SAFETY: `term` is live; the slice is valid for its length. Callbacks
        // only touch `callbacks`, which outlives this call.
        unsafe { ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
        if self.callbacks.panicked {
            return Err(EngineError::CallbackPanicked);
        }
        let mut fx = self.callbacks.take();
        if self.callbacks.title_changed {
            self.callbacks.title_changed = false;
            fx.title = Some(self.read_string(GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_TITLE)?);
        }
        if self.callbacks.pwd_changed {
            self.callbacks.pwd_changed = false;
            fx.pwd = Some(self.read_string(GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_PWD)?);
        }
        Ok(fx)
    }

    /// Resize the grid. `cell_px` is the canonical cell size in pixels, used
    /// for pixel-based reports; it never changes the grid.
    pub fn resize(&mut self, size: GridSize, cell_px: (u32, u32)) -> Result<(), EngineError> {
        if size.cols == 0 || size.rows == 0 {
            return Err(EngineError::InvalidSize(size));
        }
        // SAFETY: `term` is live.
        check("ghostty_terminal_resize", unsafe {
            ghostty_terminal_resize(self.term, size.cols, size.rows, cell_px.0, cell_px.1)
        })?;
        self.size = size;
        Ok(())
    }

    /// Produce the current frame. Unchanged rows are shared with the previous
    /// frame; if nothing changed, the previous frame itself is returned.
    pub fn frame(&mut self) -> Result<Arc<FrameSnapshot>, EngineError> {
        let frame = frame::build(self)?;
        self.last = Some(Arc::clone(&frame));
        Ok(frame)
    }

    /// Current value of a terminal mode: `ansi` modes are set with `CSI n h`,
    /// DEC private modes with `CSI ? n h`.
    pub fn mode(&self, number: u16, ansi: bool) -> Result<bool, EngineError> {
        let mut config = GhosttyTerminalModeConfig {
            mode: (number & 0x7fff) | (u16::from(ansi) << 15),
            value: false,
        };
        // SAFETY: `term` is live; the config is read and written in place.
        check("ghostty_terminal_get(mode)", unsafe {
            ghostty_terminal_get(
                self.term,
                GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MODE,
                (&raw mut config).cast(),
            )
        })?;
        Ok(config.value)
    }

    /// Cursor position in the active screen, zero-based `(x, y)`.
    pub fn cursor_position(&self) -> Result<(u16, u16), EngineError> {
        let (mut x, mut y) = (0u16, 0u16);
        // SAFETY: `term` is live; each key writes one u16.
        unsafe {
            check(
                "ghostty_terminal_get(cursor_x)",
                ghostty_terminal_get(
                    self.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_CURSOR_X,
                    (&raw mut x).cast(),
                ),
            )?;
            check(
                "ghostty_terminal_get(cursor_y)",
                ghostty_terminal_get(
                    self.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_CURSOR_Y,
                    (&raw mut y).cast(),
                ),
            )?;
        }
        Ok((x, y))
    }

    /// Plain text of the whole active screen including scrollback, one line
    /// per row (wrapped rows are not joined), trailing blanks trimmed.
    /// Intended for verification and diagnostics, not the render path.
    pub fn screen_text(&self) -> Result<String, EngineError> {
        let mut fmt: GhosttyFormatter = ptr::null_mut();
        // SAFETY: `term` outlives `fmt`, which is freed before returning; the
        // output buffer is copied and freed with the same (default) allocator.
        unsafe {
            check(
                "ghostty_formatter_terminal_new",
                ghostty_formatter_terminal_new(ptr::null(), &mut fmt, self.term),
            )?;
            let mut out: *mut u8 = ptr::null_mut();
            let mut len = 0usize;
            let rc = ghostty_formatter_format_alloc(fmt, ptr::null(), &mut out, &mut len);
            let text = borrowed_str(out, len);
            if !out.is_null() {
                ghostty_free(ptr::null(), out, len);
            }
            ghostty_formatter_free(fmt);
            check("ghostty_formatter_format_alloc", rc)?;
            Ok(text)
        }
    }

    /// Move the viewport through history. Local view state only: it never
    /// changes what the terminal holds, and new output keeps a scrolled
    /// viewport where it is.
    pub fn scroll_viewport(&mut self, to: Scroll) -> Result<(), EngineError> {
        use GhosttyTerminalScrollViewportTag::*;
        let mut value = GhosttyTerminalScrollViewportValue::default();
        let tag = match to {
            Scroll::Top => GHOSTTY_TERMINAL_SCROLL_VIEWPORT_TOP,
            Scroll::Bottom => GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM,
            Scroll::Delta(d) => {
                value.delta = d;
                GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA
            }
        };
        // SAFETY: `term` is live; the behavior is passed by value.
        check("ghostty_terminal_scroll_viewport", unsafe {
            ghostty_terminal_scroll_viewport(
                self.term,
                GhosttyTerminalScrollViewport { tag, value },
            )
        })
    }

    /// The viewport's position and the modes a view needs locally.
    pub fn view_state(&self) -> Result<terminal_core::ViewState, EngineError> {
        let mut bar = GhosttyTerminalScrollbar::default();
        let mut tracking = false;
        let mut screen: GhosttyTerminalScreen::Type = 0;
        // SAFETY: `term` is live; each key writes the documented type.
        unsafe {
            check(
                "ghostty_terminal_get(scrollbar)",
                ghostty_terminal_get(
                    self.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                    (&raw mut bar).cast(),
                ),
            )?;
            check(
                "ghostty_terminal_get(mouse_tracking)",
                ghostty_terminal_get(
                    self.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                    (&raw mut tracking).cast(),
                ),
            )?;
            check(
                "ghostty_terminal_get(active_screen)",
                ghostty_terminal_get(
                    self.term,
                    GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                    (&raw mut screen).cast(),
                ),
            )?;
        }
        Ok(terminal_core::ViewState {
            total_rows: bar.total,
            offset: bar.offset,
            len: bar.len,
            mouse_tracking: tracking,
            alt_screen: screen != GhosttyTerminalScreen::GHOSTTY_TERMINAL_SCREEN_PRIMARY,
        })
    }

    /// Lines of scrollback above the active screen.
    pub fn scrollback_rows(&self) -> Result<usize, EngineError> {
        let mut rows: usize = 0;
        // SAFETY: `term` is live; the key writes one size_t.
        check("ghostty_terminal_get(scrollback_rows)", unsafe {
            ghostty_terminal_get(
                self.term,
                GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
                (&raw mut rows).cast(),
            )
        })?;
        Ok(rows)
    }

    fn read_string(&self, data: GhosttyTerminalData::Type) -> Result<String, EngineError> {
        let mut s = GhosttyString::default();
        // SAFETY: `term` is live; the returned string is borrowed until the
        // next mutation and copied before returning.
        let code = unsafe { ghostty_terminal_get(self.term, data, (&raw mut s).cast()) };
        if code == GhosttyResult::GHOSTTY_NO_VALUE {
            return Ok(String::new());
        }
        check("ghostty_terminal_get(string)", code)?;
        Ok(borrowed_str(s.ptr, s.len))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: each handle is either null (accepted by the free functions)
        // or was created by the matching `_new` and is freed exactly once,
        // dependents before the terminal.
        unsafe {
            ghostty_selection_gesture_free(self.gesture, self.term);
            ghostty_render_state_row_cells_free(self.row_cells);
            ghostty_render_state_row_iterator_free(self.row_iter);
            ghostty_render_state_free(self.render);
            ghostty_terminal_free(self.term);
        }
    }
}

/// Copy a borrowed (ptr, len) UTF-8 string, replacing invalid sequences.
pub(crate) fn borrowed_str(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    // SAFETY: the library guarantees `len` readable bytes at `ptr` for the
    // duration of the borrow; they are copied immediately.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}
