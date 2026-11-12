//! Cold attach: reconstruct one pane's terminal state from tmux and continue
//! from exactly where that state ends. See `docs/tmux-bootstrap.md`.
//!
//! Two round trips on the pane's canonical control client:
//!
//! 1. `refresh-client -A '%N:pause'`, then wait for `%pause %N`. tmux discards
//!    the pane's unwritten output when pausing, and queues none while paused,
//!    so every `%output %N` that follows `%pause %N` in the stream was produced
//!    after the pause. Earlier `%output %N` is dropped: its effect is in the
//!    grid that is about to be captured.
//! 2. One command line, executed by tmux without yielding: pane state, the
//!    history, the visible screen, the saved normal screen (when the alternate
//!    screen is active), the pending incomplete escape bytes, and
//!    `refresh-client -A '%N:continue'`. `continue` resumes delivery from the
//!    parser position the captures reflect, so all `%output %N` after the
//!    pause is the exact continuation of the captured state, even when it
//!    reaches us before the capture responses.
//!
//! Restoration feeds a fresh engine with synthesized VT bytes; generated
//! replies are never captured during restore and its effects are discarded.

use std::time::{Duration, Instant};

use terminal_ghostty::core::GridSize;
use terminal_ghostty::{Engine, EngineConfig, EngineError};
use tmux_protocol::{Event, Notification, PaneId, Response, unescape};

use crate::control::{ControlClient, ControlError};
use crate::quote;

/// Pane state fields read with one `display-message`.
const STATE_FORMAT: &str = "#{pane_width}|#{pane_height}|#{history_size}|#{alternate_on}|\
#{alternate_saved_x}|#{alternate_saved_y}|#{cursor_x}|#{cursor_y}|#{cursor_flag}|\
#{scroll_region_upper}|#{scroll_region_lower}|#{insert_flag}|#{keypad_cursor_flag}|\
#{keypad_flag}|#{wrap_flag}|#{origin_flag}|#{mouse_standard_flag}|#{mouse_button_flag}|\
#{mouse_any_flag}|#{mouse_sgr_flag}|#{bracket_paste_flag}|#{cursor_shape}|\
#{cursor_blinking}|#{pane_tabs}|#{pane_output_generation}|#{history_limit}";

const STATE_FIELDS: usize = 26;

/// Pane state reported by tmux at capture time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneState {
    pub width: u16,
    pub height: u16,
    pub history_size: u32,
    pub alternate_on: bool,
    pub alternate_saved: (u16, u16),
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
    pub scroll_region: (u16, u16),
    pub insert: bool,
    pub cursor_keys_app: bool,
    pub keypad_app: bool,
    pub wrap: bool,
    pub origin: bool,
    pub mouse_standard: bool,
    pub mouse_button: bool,
    pub mouse_any: bool,
    pub mouse_sgr: bool,
    pub bracketed_paste: bool,
    pub cursor_shape: String,
    pub cursor_blinking: bool,
    /// Tab stop columns; `None` when tmux reports the default.
    pub tabs: Option<Vec<u16>>,
    pub output_generation: u64,
    /// The pane's scrollback limit in lines (`history-limit`).
    pub history_limit: u32,
}

impl PaneState {
    pub fn parse(line: &str) -> Result<Self, String> {
        let f: Vec<&str> = line.split('|').collect();
        if f.len() != STATE_FIELDS {
            return Err(format!(
                "expected {STATE_FIELDS} state fields, got {}: {line}",
                f.len()
            ));
        }
        let n = |i: usize| -> Result<u64, String> {
            f[i].parse::<u64>()
                .map_err(|_| format!("state field {i} is not a number: `{}`", f[i]))
        };
        let u16f = |i: usize| -> Result<u16, String> {
            u16::try_from(n(i)?).map_err(|_| format!("state field {i} out of range"))
        };
        let b = |i: usize| -> Result<bool, String> { Ok(n(i)? != 0) };
        // tmux reports unset coordinates (no saved alternate cursor) as
        // UINT_MAX; they are only meaningful when the alternate screen is on.
        let coord = |i: usize| -> Result<u16, String> { Ok(u16::try_from(n(i)?).unwrap_or(0)) };
        let tabs = if f[23].is_empty() {
            Some(Vec::new())
        } else {
            Some(
                f[23]
                    .split(',')
                    .map(|t| t.parse::<u16>().map_err(|_| format!("bad tab stop `{t}`")))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let width = u16f(0)?;
        let tabs = tabs.filter(|t| *t != default_tabs(width));
        Ok(Self {
            width,
            height: u16f(1)?,
            history_size: u32::try_from(n(2)?).map_err(|_| "history too large".to_owned())?,
            alternate_on: b(3)?,
            alternate_saved: (coord(4)?, coord(5)?),
            cursor: (u16f(6)?, u16f(7)?),
            cursor_visible: b(8)?,
            scroll_region: (u16f(9)?, u16f(10)?),
            insert: b(11)?,
            cursor_keys_app: b(12)?,
            keypad_app: b(13)?,
            wrap: b(14)?,
            origin: b(15)?,
            mouse_standard: b(16)?,
            mouse_button: b(17)?,
            mouse_any: b(18)?,
            mouse_sgr: b(19)?,
            bracketed_paste: b(20)?,
            cursor_shape: f[21].to_owned(),
            cursor_blinking: b(22)?,
            tabs,
            output_generation: n(24)?,
            history_limit: u32::try_from(n(25)?).unwrap_or(u32::MAX),
        })
    }

    pub fn size(&self) -> GridSize {
        GridSize {
            cols: self.width,
            rows: self.height,
        }
    }
}

fn default_tabs(width: u16) -> Vec<u16> {
    (8..width).step_by(8).collect()
}

/// Raw captures returned by tmux.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captures {
    /// Scrollback lines (wrapped lines joined), oldest first.
    pub history: Vec<Vec<u8>>,
    /// The visible screen: the alternate screen when it is active.
    pub visible: Vec<Vec<u8>>,
    /// The saved normal screen when the alternate screen is active.
    pub saved_normal: Vec<Vec<u8>>,
    /// Bytes of an incomplete escape sequence still in tmux's parser.
    pub pending: Vec<u8>,
}

#[derive(Debug)]
pub enum BootstrapError {
    Control(ControlError),
    Engine(EngineError),
    State(String),
    PaneNotFound(String),
    /// Phase 2 failed; the pane was resumed with a separate command.
    CaptureFailed(String),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Control(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "{e}"),
            Self::State(e) => write!(f, "pane state: {e}"),
            Self::PaneNotFound(p) => write!(f, "pane {p} not found"),
            Self::CaptureFailed(e) => write!(f, "capture failed: {e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

impl From<ControlError> for BootstrapError {
    fn from(e: ControlError) -> Self {
        Self::Control(e)
    }
}

impl From<EngineError> for BootstrapError {
    fn from(e: EngineError) -> Self {
        Self::Engine(e)
    }
}

/// A restored, live-ready mirror and the evidence behind it.
#[derive(Debug)]
pub struct Bootstrapped {
    pub engine: Engine,
    pub state: PaneState,
    pub captures: Captures,
    /// `%output` bytes dropped before the pause (already in the capture).
    pub dropped_before_pause: usize,
    /// Continuation bytes received during phase 2 and already applied.
    pub continuation: usize,
    /// Other events seen meanwhile, in order, for the caller to process.
    pub passthrough: Vec<Event>,
}

/// Bootstrap `pane` through `client`. The client must be the pane's
/// canonical output source and must not use `pause-after`.
pub fn bootstrap_pane(
    client: &mut ControlClient,
    pane: PaneId,
    timeout: Duration,
) -> Result<Bootstrapped, BootstrapError> {
    let deadline = Instant::now() + timeout;
    let left = || deadline.saturating_duration_since(Instant::now());
    let target = pane.to_string();
    let mut passthrough = Vec::new();

    // Phase 1: pause and wait until the pause is visible in the stream.
    // `refresh-client -A` silently ignores unknown panes, so the target is
    // validated first; a failure there skips the pause.
    client.send(&format!(
        "display-message -p -t {} '#{{pane_id}}' ; refresh-client -A {}",
        quote(&target),
        quote(&format!("{target}:pause"))
    ))?;
    let mut dropped_before_pause = 0usize;
    let (mut answered, mut paused, mut missing) = (0usize, false, false);
    // Both responses are always consumed so the stream stays aligned for the
    // next command, even when the pane turns out not to exist.
    while answered < 2 || !(paused || missing) {
        match client.recv(left())? {
            Event::Response(r) if r.ok => {
                answered += 1;
                // tmux prints an empty line, not an error, for an unknown
                // pane; the pause that follows is then a no-op.
                if answered == 1 && r.body.first().map(Vec::as_slice) != Some(target.as_bytes()) {
                    missing = true;
                }
            }
            Event::Response(r) => return Err(ControlError::Command(r).into()),
            Event::Notification(Notification::Output { pane: p, data }) if p == pane => {
                dropped_before_pause += data.len();
            }
            Event::Notification(Notification::Pause { pane: p }) if p == pane => paused = true,
            other => passthrough.push(other),
        }
    }
    if missing {
        return Err(BootstrapError::PaneNotFound(target));
    }

    // Phase 2: state, captures, and continue, as one non-yielding line.
    let t = quote(&target);
    let line = [
        format!("display-message -p -t {t} {}", quote(STATE_FORMAT)),
        format!("capture-pane -p -e -J -N -S - -E -1 -t {t}"),
        format!("capture-pane -p -e -J -N -S 0 -E - -t {t}"),
        format!("capture-pane -p -e -J -N -a -q -t {t}"),
        format!("capture-pane -p -P -C -t {t}"),
        format!("refresh-client -A {}", quote(&format!("{target}:continue"))),
    ]
    .join(" ; ");
    const EXPECTED: usize = 6;
    client.send(&line)?;
    let mut responses: Vec<Response> = Vec::with_capacity(EXPECTED);
    let mut continuation: Vec<u8> = Vec::new();
    let mut continued = false;
    let mut failure: Option<Response> = None;
    while failure.is_none() && !(responses.len() == EXPECTED && continued) {
        match client.recv(left())? {
            Event::Response(r) if r.ok => responses.push(r),
            Event::Response(r) => failure = Some(r),
            Event::Notification(Notification::Output { pane: p, data }) if p == pane => {
                continuation.extend_from_slice(&data);
            }
            Event::Notification(Notification::Continue { pane: p }) if p == pane => {
                continued = true
            }
            other => passthrough.push(other),
        }
    }
    if let Some(failed) = failure {
        // Commands after the failing one did not run: resume in a fresh
        // command so the pane is not left paused for this client.
        let resume = format!("refresh-client -A {}", quote(&format!("{target}:continue")));
        let _ = client.command(&resume, left(), |e| passthrough.push(e));
        return Err(BootstrapError::CaptureFailed(
            failed
                .body
                .iter()
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect::<Vec<_>>()
                .join(" / "),
        ));
    }

    let state_line = responses[0]
        .body
        .first()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .unwrap_or_default();
    let state = PaneState::parse(&state_line).map_err(BootstrapError::State)?;
    let mut pending = Vec::new();
    for line in &responses[4].body {
        unescape(line, &mut pending)
            .map_err(|e| BootstrapError::State(format!("pending bytes: {e}")))?;
    }
    let captures = Captures {
        history: if state.history_size == 0 {
            Vec::new()
        } else {
            responses[1].body.clone()
        },
        visible: responses[2].body.clone(),
        saved_normal: if state.alternate_on {
            responses[3].body.clone()
        } else {
            Vec::new()
        },
        pending,
    };

    // The mirror keeps as much history as tmux does. Memory budgets for
    // mirrors are a later policy (spec 16.4); M0 measures fidelity.
    let mut config = EngineConfig::new(state.size());
    config.scrollback_lines = Some(state.history_limit as usize);
    config.scrollback_bytes = Some(usize::MAX);
    let mut engine = Engine::new(config)?;
    // Restore under suppression: no reply capture, effects discarded.
    let _ = engine.feed(&restore_bytes(&state, &captures))?;
    let _ = engine.feed(&continuation)?;
    Ok(Bootstrapped {
        engine,
        state,
        captures,
        dropped_before_pause,
        continuation: continuation.len(),
        passthrough,
    })
}

/// Synthesize the VT byte stream that recreates the captured state in a
/// fresh engine of the pane's size.
pub fn restore_bytes(state: &PaneState, captures: &Captures) -> Vec<u8> {
    let mut out = Vec::new();
    // Tab stops first: captured lines can contain literal tabs, which tmux
    // keeps in its grid and which must land on the same stops.
    if let Some(tabs) = &state.tabs {
        out.extend_from_slice(b"\x1b[3g");
        for &col in tabs {
            out.extend_from_slice(format!("\x1b[{}G\x1bH", col + 1).as_bytes());
        }
        out.extend_from_slice(b"\x1b[H");
    }
    let mut first = true;
    let lines = |out: &mut Vec<u8>, lines: &[Vec<u8>], first: &mut bool| {
        for line in lines {
            if !*first {
                out.extend_from_slice(b"\r\n");
            }
            *first = false;
            out.extend_from_slice(line);
            out.extend_from_slice(b"\x1b[0m");
        }
    };

    if state.alternate_on {
        lines(&mut out, &captures.history, &mut first);
        lines(&mut out, &captures.saved_normal, &mut first);
        let (x, y) = state.alternate_saved;
        push_cup(&mut out, x, y);
        // Saves this cursor for the eventual 1049l, switches, and clears.
        out.extend_from_slice(b"\x1b[?1049h\x1b[H");
        let mut first_alt = true;
        lines(&mut out, &captures.visible, &mut first_alt);
    } else {
        lines(&mut out, &captures.history, &mut first);
        lines(&mut out, &captures.visible, &mut first);
    }

    let (top, bottom) = state.scroll_region;
    if (top, bottom) != (0, state.height.saturating_sub(1)) {
        out.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
    }
    let modes: [(bool, &[u8]); 10] = [
        (state.insert, b"\x1b[4h"),
        (state.cursor_keys_app, b"\x1b[?1h"),
        (state.keypad_app, b"\x1b="),
        (!state.wrap, b"\x1b[?7l"),
        (state.mouse_standard, b"\x1b[?1000h"),
        (state.mouse_button, b"\x1b[?1002h"),
        (state.mouse_any, b"\x1b[?1003h"),
        (state.mouse_sgr, b"\x1b[?1006h"),
        (state.bracketed_paste, b"\x1b[?2004h"),
        (!state.cursor_visible, b"\x1b[?25l"),
    ];
    for (on, seq) in modes {
        if on {
            out.extend_from_slice(seq);
        }
    }
    let shape = match (state.cursor_shape.as_str(), state.cursor_blinking) {
        ("block", true) => Some(1),
        ("block", false) => Some(2),
        ("underline", true) => Some(3),
        ("underline", false) => Some(4),
        ("bar", true) => Some(5),
        ("bar", false) => Some(6),
        _ => None,
    };
    if let Some(n) = shape {
        out.extend_from_slice(format!("\x1b[{n} q").as_bytes());
    }
    // Origin mode homes the cursor and makes CUP relative to the region.
    let (x, y) = state.cursor;
    if state.origin {
        out.extend_from_slice(b"\x1b[?6h");
        push_cup(&mut out, x, y.saturating_sub(top));
    } else {
        push_cup(&mut out, x, y);
    }
    out.extend_from_slice(&captures.pending);
    out
}

fn push_cup(out: &mut Vec<u8>, x: u16, y: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_line(overrides: &[(usize, &str)]) -> String {
        let mut f: Vec<String> = [
            "20", "4", "0", "0", "0", "0", "3", "1", "1", "0", "3", "0", "0", "0", "1", "0", "0",
            "0", "0", "0", "0", "block", "0", "8,16", "42", "2000",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for (i, v) in overrides {
            f[*i] = (*v).to_owned();
        }
        f.join("|")
    }

    #[test]
    fn parses_state_and_ignores_default_tabs() {
        let s = PaneState::parse(&state_line(&[])).expect("parse");
        assert_eq!(s.size(), GridSize { cols: 20, rows: 4 });
        assert_eq!(s.cursor, (3, 1));
        assert_eq!(s.tabs, None);
        assert_eq!(s.output_generation, 42);
        let s = PaneState::parse(&state_line(&[(23, "4,9")])).expect("parse");
        assert_eq!(s.tabs, Some(vec![4, 9]));
        assert!(PaneState::parse("1|2|3").is_err());
        assert!(PaneState::parse(&state_line(&[(6, "x")])).is_err());
    }

    #[test]
    fn restore_rebuilds_text_cursor_and_modes() {
        let state = PaneState::parse(&state_line(&[(12, "1"), (20, "1"), (8, "0"), (21, "bar")]))
            .expect("parse");
        let caps = Captures {
            visible: vec![b"\x1b[31mred\x1b[39m text".to_vec(), b"second".to_vec()],
            pending: b"\x1b[1".to_vec(),
            ..Captures::default()
        };
        let mut e = Engine::new(EngineConfig::new(state.size())).expect("engine");
        e.feed(&restore_bytes(&state, &caps)).expect("feed");
        e.feed(b";32mgreen").expect("continue");
        let frame = e.frame().expect("frame");
        assert_eq!(frame.rows[0].text(), "red text");
        // The cursor was restored to (3, 1), so the continuation overwrites
        // "second" from column 3.
        assert_eq!(frame.rows[1].text(), "secgreen");
        assert!(e.mode(1, false).expect("decckm"));
        assert!(e.mode(2004, false).expect("paste"));
        assert!(!e.mode(25, false).expect("visible"));
        // The pending CSI completed with the continuation.
        let style = frame.style_dump();
        assert!(style.contains("Palette(2)"), "{style}");
    }
}
