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

pub use features::{Features, features};
pub use terminal_core as core;

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
        let mut engine = Engine {
            term: ptr::null_mut(),
            render: ptr::null_mut(),
            row_iter: ptr::null_mut(),
            row_cells: ptr::null_mut(),
            callbacks: Box::new(CallbackState::new(config.capture_replies)),
            size,
            revision: 0,
            last: None,
            scratch: frame::Scratch::default(),
            _not_send: PhantomData,
        };
        // Each handle is stored as soon as it exists, so `Drop` frees exactly
        // what was created if a later step fails.
        // SAFETY: valid out-pointers; NULL allocator selects the default.
        unsafe {
            check(
                "ghostty_terminal_new",
                ghostty_terminal_new(ptr::null(), &mut engine.term, size.cols, size.rows),
            )?;
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
        if let Some(lines) = config.scrollback_lines {
            // SAFETY: the option takes a pointer to a size_t read during the call.
            check("set scrollback lines", unsafe {
                ghostty_terminal_set(
                    engine.term,
                    GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                    (&raw const lines).cast(),
                )
            })?;
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
