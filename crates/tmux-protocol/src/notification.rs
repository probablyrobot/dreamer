//! Control-mode notifications (lines starting with `%` outside a response).
//!
//! Formats follow tmux 3.8 (`control.c`, `control-notify.c`, `client.c`,
//! `cfg.c`). Lines that carry pane data (`%output`, `%extended-output`) and
//! flow-control lines (`%pause`, `%continue`) must parse exactly; a malformed
//! one is a protocol error because pane data would be lost. Any other line
//! that does not match its known shape, and any unknown name, is returned as
//! [`Notification::Unknown`] so the caller can reconcile through queries.

use crate::escape::unescape;
use crate::ids::{PaneId, SessionId, WindowId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// Pane output, unescaped to the original bytes.
    Output {
        pane: PaneId,
        data: Vec<u8>,
    },
    /// Output with its age in milliseconds (clients using `pause-after`).
    ExtendedOutput {
        pane: PaneId,
        age_ms: u64,
        data: Vec<u8>,
    },
    /// tmux stopped sending output for this pane to this client.
    Pause {
        pane: PaneId,
    },
    /// Output resumed; anything produced while paused was not delivered.
    Continue {
        pane: PaneId,
    },
    /// The control client is exiting.
    Exit {
        reason: Option<String>,
    },
    SessionsChanged,
    SessionChanged {
        session: SessionId,
        name: String,
    },
    SessionRenamed {
        session: SessionId,
        name: String,
    },
    SessionWindowChanged {
        session: SessionId,
        window: WindowId,
    },
    ClientSessionChanged {
        client: String,
        session: SessionId,
        name: String,
    },
    ClientDetached {
        client: String,
    },
    WindowAdd {
        window: WindowId,
    },
    WindowClose {
        window: WindowId,
    },
    WindowRenamed {
        window: WindowId,
        name: String,
    },
    UnlinkedWindowAdd {
        window: WindowId,
    },
    UnlinkedWindowClose {
        window: WindowId,
    },
    UnlinkedWindowRenamed {
        window: WindowId,
        name: String,
    },
    WindowPaneChanged {
        window: WindowId,
        pane: PaneId,
    },
    LayoutChange {
        window: WindowId,
        layout: String,
        visible_layout: String,
        flags: String,
    },
    PaneModeChanged {
        pane: PaneId,
    },
    PasteBufferChanged {
        name: String,
    },
    PasteBufferDeleted {
        name: String,
    },
    SubscriptionChanged {
        name: String,
        session: SessionId,
        window: Option<WindowId>,
        window_index: Option<u32>,
        pane: Option<PaneId>,
        value: Vec<u8>,
    },
    ConfigError {
        message: String,
    },
    /// Unknown name, or a metadata notification whose shape was not
    /// recognized. Framing is preserved; reconcile state by querying.
    Unknown {
        name: String,
        rest: Vec<u8>,
    },
}

/// Parse one notification line (without its newline).
///
/// Returns `Err` only for malformed data or flow-control lines.
pub fn parse_notification(line: &[u8]) -> Result<Notification, String> {
    let body = line
        .strip_prefix(b"%")
        .ok_or_else(|| "notification must start with %".to_owned())?;
    let (name, rest) = split_once(body, b' ').unwrap_or((body, &[]));
    let has_args = body.len() > name.len();

    match name {
        b"output" => {
            let (pane, data) = split_once(rest, b' ').ok_or("missing output data")?;
            let pane = PaneId::parse(pane).ok_or("bad pane id")?;
            let mut out = Vec::new();
            unescape(data, &mut out).map_err(|e| e.to_string())?;
            return Ok(Notification::Output { pane, data: out });
        }
        b"extended-output" => {
            let (pane, rest) = split_once(rest, b' ').ok_or("missing age")?;
            let pane = PaneId::parse(pane).ok_or("bad pane id")?;
            let (age, rest) = split_once(rest, b' ').ok_or("missing separator")?;
            let age_ms = ascii_u64(age).ok_or("bad age")?;
            // tmux 3.8 writes exactly one ` : ` before the data; later
            // versions may add fields before the colon, which are skipped.
            let colon = find(rest, b": ").ok_or("missing ` : ` separator")?;
            let mut out = Vec::new();
            unescape(&rest[colon + 2..], &mut out).map_err(|e| e.to_string())?;
            return Ok(Notification::ExtendedOutput {
                pane,
                age_ms,
                data: out,
            });
        }
        b"pause" => {
            return PaneId::parse(rest)
                .map(|pane| Notification::Pause { pane })
                .ok_or_else(|| "bad pane id".to_owned());
        }
        b"continue" => {
            return PaneId::parse(rest)
                .map(|pane| Notification::Continue { pane })
                .ok_or_else(|| "bad pane id".to_owned());
        }
        _ => {}
    }

    let parsed = match name {
        b"exit" => Some(Notification::Exit {
            reason: has_args.then(|| text(rest)),
        }),
        b"sessions-changed" if !has_args => Some(Notification::SessionsChanged),
        b"session-changed" => id_and_name(rest, SessionId::parse)
            .map(|(session, name)| Notification::SessionChanged { session, name }),
        b"session-renamed" => id_and_name(rest, SessionId::parse)
            .map(|(session, name)| Notification::SessionRenamed { session, name }),
        b"session-window-changed" => two_ids(rest, SessionId::parse, WindowId::parse)
            .map(|(session, window)| Notification::SessionWindowChanged { session, window }),
        b"client-session-changed" => split_once(rest, b' ').and_then(|(client, rest)| {
            id_and_name(rest, SessionId::parse).map(|(session, name)| {
                Notification::ClientSessionChanged {
                    client: text(client),
                    session,
                    name,
                }
            })
        }),
        b"client-detached" if has_args => Some(Notification::ClientDetached { client: text(rest) }),
        b"window-add" => WindowId::parse(rest).map(|window| Notification::WindowAdd { window }),
        b"window-close" => WindowId::parse(rest).map(|window| Notification::WindowClose { window }),
        b"window-renamed" => id_and_name(rest, WindowId::parse)
            .map(|(window, name)| Notification::WindowRenamed { window, name }),
        b"unlinked-window-add" => {
            WindowId::parse(rest).map(|window| Notification::UnlinkedWindowAdd { window })
        }
        b"unlinked-window-close" => {
            WindowId::parse(rest).map(|window| Notification::UnlinkedWindowClose { window })
        }
        b"unlinked-window-renamed" => id_and_name(rest, WindowId::parse)
            .map(|(window, name)| Notification::UnlinkedWindowRenamed { window, name }),
        b"window-pane-changed" => two_ids(rest, WindowId::parse, PaneId::parse)
            .map(|(window, pane)| Notification::WindowPaneChanged { window, pane }),
        b"layout-change" => {
            let mut parts = rest.split(|&b| b == b' ');
            match (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) {
                (Some(w), Some(layout), Some(visible), flags, None) => {
                    WindowId::parse(w).map(|window| Notification::LayoutChange {
                        window,
                        layout: text(layout),
                        visible_layout: text(visible),
                        flags: flags.map(text).unwrap_or_default(),
                    })
                }
                _ => None,
            }
        }
        b"pane-mode-changed" => {
            PaneId::parse(rest).map(|pane| Notification::PaneModeChanged { pane })
        }
        b"paste-buffer-changed" if has_args => {
            Some(Notification::PasteBufferChanged { name: text(rest) })
        }
        b"paste-buffer-deleted" if has_args => {
            Some(Notification::PasteBufferDeleted { name: text(rest) })
        }
        b"subscription-changed" => parse_subscription(rest),
        b"config-error" => Some(Notification::ConfigError {
            message: text(rest),
        }),
        _ => None,
    };
    Ok(parsed.unwrap_or_else(|| Notification::Unknown {
        name: text(name),
        rest: rest.to_vec(),
    }))
}

/// `%subscription-changed <name> $s <@w|-> <idx|-> <%p|-> : <value>`
fn parse_subscription(rest: &[u8]) -> Option<Notification> {
    let colon = find(rest, b" : ")?;
    let value = rest[colon + 3..].to_vec();
    let mut parts = rest[..colon].split(|&b| b == b' ');
    let name = text(parts.next()?);
    let session = SessionId::parse(parts.next()?)?;
    let window = optional(parts.next()?, WindowId::parse)?;
    let window_index = optional(parts.next()?, ascii_u32)?;
    let pane = optional(parts.next()?, PaneId::parse)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Notification::SubscriptionChanged {
        name,
        session,
        window,
        window_index,
        pane,
        value,
    })
}

/// `-` means absent; anything else must parse. `None` = malformed.
fn optional<T>(token: &[u8], parse: impl Fn(&[u8]) -> Option<T>) -> Option<Option<T>> {
    if token == b"-" {
        Some(None)
    } else {
        parse(token).map(Some)
    }
}

fn id_and_name<T>(rest: &[u8], parse: impl Fn(&[u8]) -> Option<T>) -> Option<(T, String)> {
    let (id, name) = split_once(rest, b' ')?;
    Some((parse(id)?, text(name)))
}

fn two_ids<A, B>(
    rest: &[u8],
    a: impl Fn(&[u8]) -> Option<A>,
    b: impl Fn(&[u8]) -> Option<B>,
) -> Option<(A, B)> {
    let (x, y) = split_once(rest, b' ')?;
    Some((a(x)?, b(y)?))
}

fn split_once(s: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&b| b == sep)?;
    Some((&s[..i], &s[i + 1..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn ascii_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn ascii_u32(s: &[u8]) -> Option<u32> {
    ascii_u64(s).and_then(|v| u32::try_from(v).ok())
}

/// Names and messages are metadata, not terminal data; invalid UTF-8 is
/// replaced rather than rejected.
fn text(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(line: &[u8]) -> Notification {
        parse_notification(line).expect("parse")
    }

    #[test]
    fn output_preserves_spaces_and_unescapes() {
        assert_eq!(
            n(b"%output %3   two  spaces \\015\\012"),
            Notification::Output {
                pane: PaneId(3),
                data: b"  two  spaces \r\n".to_vec()
            }
        );
        assert_eq!(
            n(b"%output %3 "),
            Notification::Output {
                pane: PaneId(3),
                data: Vec::new()
            }
        );
    }

    #[test]
    fn extended_output_keeps_data_after_the_separator() {
        assert_eq!(
            n(b"%extended-output %1 250 : a : b\\012"),
            Notification::ExtendedOutput {
                pane: PaneId(1),
                age_ms: 250,
                data: b"a : b\n".to_vec()
            }
        );
    }

    #[test]
    fn malformed_data_lines_are_errors() {
        for bad in [
            &b"%output 3 x"[..],
            b"%output %3",
            b"%output %3 bad\\9",
            b"%extended-output %1 x : y",
            b"%extended-output %1 5 y",
            b"%pause 1",
            b"%continue",
        ] {
            assert!(
                parse_notification(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn metadata_notifications() {
        assert_eq!(
            n(b"%session-changed $1 my session"),
            Notification::SessionChanged {
                session: SessionId(1),
                name: "my session".into()
            }
        );
        assert_eq!(
            n(b"%client-session-changed /dev/ttys003 $2 work"),
            Notification::ClientSessionChanged {
                client: "/dev/ttys003".into(),
                session: SessionId(2),
                name: "work".into()
            }
        );
        assert_eq!(
            n(b"%layout-change @4 b25d,80x24,0,0,1 b25d,80x24,0,0,1 *"),
            Notification::LayoutChange {
                window: WindowId(4),
                layout: "b25d,80x24,0,0,1".into(),
                visible_layout: "b25d,80x24,0,0,1".into(),
                flags: "*".into()
            }
        );
        assert_eq!(
            n(b"%window-pane-changed @1 %7"),
            Notification::WindowPaneChanged {
                window: WindowId(1),
                pane: PaneId(7)
            }
        );
        assert_eq!(n(b"%exit"), Notification::Exit { reason: None });
        assert_eq!(
            n(b"%exit detached"),
            Notification::Exit {
                reason: Some("detached".into())
            }
        );
        assert_eq!(n(b"%sessions-changed"), Notification::SessionsChanged);
    }

    #[test]
    fn subscriptions_with_and_without_targets() {
        assert_eq!(
            n(b"%subscription-changed cwd $1 @2 0 %3 : /home/u x"),
            Notification::SubscriptionChanged {
                name: "cwd".into(),
                session: SessionId(1),
                window: Some(WindowId(2)),
                window_index: Some(0),
                pane: Some(PaneId(3)),
                value: b"/home/u x".to_vec()
            }
        );
        assert_eq!(
            n(b"%subscription-changed s $1 - - - : "),
            Notification::SubscriptionChanged {
                name: "s".into(),
                session: SessionId(1),
                window: None,
                window_index: None,
                pane: None,
                value: Vec::new()
            }
        );
    }

    #[test]
    fn unknown_and_misshapen_metadata_become_unknown() {
        assert_eq!(
            n(b"%future-thing a b"),
            Notification::Unknown {
                name: "future-thing".into(),
                rest: b"a b".to_vec()
            }
        );
        assert!(matches!(n(b"%window-add 4"), Notification::Unknown { .. }));
        assert!(matches!(
            n(b"%pane-mode-changed name"),
            Notification::Unknown { .. }
        ));
    }
}
