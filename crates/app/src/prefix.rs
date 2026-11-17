//! The user's tmux prefix in Dreamer. Keys reach panes as bytes, so tmux's
//! own key tables never see them; this layer watches for the prefix the
//! tmux server has (`prefix`, `prefix2`) and handles the key after it:
//! `[` enters Dreamer's copy mode, `]` pastes tmux's newest buffer, `=`
//! chooses a buffer to paste, and any other key goes to tmux, which runs
//! the user's binding for it.
//!
//! The options are read from tmux (never changed): when the window's tmux
//! buffers connection starts, when a window becomes active, and after each
//! prefix key.

use std::sync::Mutex;

use replica::copy_mode::Table;
use replica::keys::Key;
use terminal_ghostty::core::{Colors, Rgb};

/// tmux's key options as last read; `None` until read, and the layer is
/// off until then.
static OPTIONS: Mutex<Option<api::KeyOptions>> = Mutex::new(None);

/// Read tmux's key options again, off the UI thread.
pub(crate) fn refresh(cx: &gpui::App) {
    if crate::TMUX_BUFFERS.get().is_none() {
        return;
    }
    cx.background_executor()
        .spawn(async move {
            if let Some(b) = crate::TMUX_BUFFERS.get()
                && let Some(o) = b.key_options().await
                && let Ok(mut opts) = OPTIONS.lock()
            {
                *opts = Some(o);
            }
        })
        .detach();
}

fn with<T>(f: impl FnOnce(&api::KeyOptions) -> T) -> Option<T> {
    OPTIONS.lock().ok()?.as_ref().map(f)
}

/// Whether `key` is tmux's `prefix` or `prefix2`.
pub(crate) fn is_prefix(key: &Key) -> bool {
    with(|o| {
        [&o.prefix, &o.prefix2]
            .into_iter()
            .filter_map(|name| replica::keys::parse(name))
            .any(|p| p == *key)
    })
    .unwrap_or(false)
}

/// The copy-mode key table tmux's `mode-keys` names.
pub(crate) fn table() -> Table {
    with(|o| Table::for_mode_keys(&o.mode_keys)).unwrap_or(Table::Emacs)
}

/// tmux's `mode-style` foreground and background, resolved against the
/// pane's colors; `None` where the style leaves the color to the
/// terminal.
pub(crate) fn mode_colors(colors: &Colors) -> (Option<Rgb>, Option<Rgb>) {
    with(|o| style_colors(&o.mode_style, colors)).unwrap_or((None, None))
}

/// The `fg=` and `bg=` of a tmux style.
pub(crate) fn style_colors(style: &str, colors: &Colors) -> (Option<Rgb>, Option<Rgb>) {
    let mut fg = None;
    let mut bg = None;
    for part in style.split([',', ' ']) {
        if let Some(v) = part.strip_prefix("fg=") {
            fg = color(v, colors);
        } else if let Some(v) = part.strip_prefix("bg=") {
            bg = color(v, colors);
        }
    }
    (fg, bg)
}

/// A tmux color: `#rrggbb`, `colourN`, a name (`red`, `brightred`, or
/// tmux 3.8's `themered`, which follows the terminal's palette).
fn color(v: &str, colors: &Colors) -> Option<Rgb> {
    if let Some(hex) = v.strip_prefix('#')
        && hex.len() == 6
        && let Ok(n) = u32::from_str_radix(hex, 16)
    {
        return Some(Rgb {
            r: (n >> 16) as u8,
            g: (n >> 8) as u8,
            b: n as u8,
        });
    }
    if let Some(n) = v
        .strip_prefix("colour")
        .or_else(|| v.strip_prefix("color"))
        .and_then(|n| n.parse::<u8>().ok())
    {
        return Some(colors.palette[usize::from(n)]);
    }
    let name = v.strip_prefix("theme").unwrap_or(v);
    let (bright, name) = match name.strip_prefix("bright") {
        Some(n) => (8, n),
        None => (0, name),
    };
    let base = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ]
    .iter()
    .position(|n| *n == name)?;
    Some(colors.palette[base + bright])
}

/// The buffer chooser (prefix `=`): tmux's buffers, newest first.
#[derive(Debug, Clone, Default)]
pub(crate) struct Chooser {
    pub buffers: Vec<api::TmuxBufferInfo>,
    pub at: usize,
}

/// What a key does in the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Choice {
    Stay,
    Close,
    /// Paste the buffer with this name.
    Paste(String),
}

impl Chooser {
    pub(crate) fn key(&mut self, k: &Key) -> Choice {
        use replica::keys::Code;
        let n = self.buffers.len();
        match (k.code, k.ctrl, k.alt) {
            (Code::Up | Code::Char('k'), false, false) | (Code::Char('p'), true, false) => {
                self.at = self.at.saturating_sub(1);
            }
            (Code::Down | Code::Char('j'), false, false) | (Code::Char('n'), true, false) => {
                self.at = (self.at + 1).min(n.saturating_sub(1));
            }
            (Code::Enter, false, false) => {
                return self
                    .buffers
                    .get(self.at)
                    .map_or(Choice::Close, |b| Choice::Paste(b.name.clone()));
            }
            (Code::Escape | Code::Char('q'), false, false)
            | (Code::Char('c' | 'g'), true, false) => {
                return Choice::Close;
            }
            _ => {}
        }
        Choice::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use replica::keys::parse;

    #[test]
    fn mode_style_colors_resolve_against_the_pane_palette() {
        let mut colors = Colors::default();
        colors.palette[3] = Rgb { r: 1, g: 2, b: 3 };
        colors.palette[9] = Rgb { r: 9, g: 9, b: 9 };
        colors.palette[200] = Rgb { r: 200, g: 0, b: 0 };
        assert_eq!(
            style_colors("noattr,bg=themeyellow,fg=themeblack", &colors),
            (Some(colors.palette[0]), Some(colors.palette[3]))
        );
        assert_eq!(
            style_colors("fg=brightred bg=#102030", &colors),
            (
                Some(colors.palette[9]),
                Some(Rgb {
                    r: 0x10,
                    g: 0x20,
                    b: 0x30
                })
            )
        );
        assert_eq!(
            style_colors("bg=colour200,fg=default", &colors),
            (None, Some(colors.palette[200]))
        );
    }

    #[test]
    fn the_chooser_moves_and_picks_a_buffer_by_name() {
        let b = |name: &str| api::TmuxBufferInfo {
            name: name.into(),
            ..api::TmuxBufferInfo::default()
        };
        let mut c = Chooser {
            buffers: vec![b("buffer2"), b("buffer1")],
            at: 0,
        };
        let key = |n: &str| parse(n).expect("key");
        assert_eq!(c.key(&key("k")), Choice::Stay);
        assert_eq!(c.at, 0);
        c.key(&key("Down"));
        c.key(&key("j"));
        assert_eq!(c.at, 1);
        assert_eq!(c.key(&key("Enter")), Choice::Paste("buffer1".into()));
        assert_eq!(c.key(&key("Escape")), Choice::Close);
        assert_eq!(Chooser::default().key(&key("Enter")), Choice::Close);
    }
}
