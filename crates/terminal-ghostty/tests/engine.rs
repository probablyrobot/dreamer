//! Engine behavior: golden frames for fixture streams, row sharing, effects,
//! reply ownership, and split input.
//!
//! Fixtures in `fixtures/*.vt` are synthetic byte streams written for these
//! tests. Each has a golden `*.dump` (styles included). Regenerate after an
//! intentional change with `UPDATE_FIXTURES=1 cargo test -p terminal-ghostty`
//! and review the diff.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use terminal_ghostty::core::{GridSize, ReplySource};
use terminal_ghostty::{Engine, EngineConfig, Scroll, features};

const SIZE: GridSize = GridSize { cols: 20, rows: 6 };

fn engine() -> Engine {
    Engine::new(EngineConfig::new(SIZE)).expect("engine")
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn dump_of(files: &[&str]) -> String {
    let mut e = engine();
    for f in files {
        let bytes = std::fs::read(fixtures().join(f)).expect("fixture");
        e.feed(&bytes).expect("feed");
    }
    e.frame().expect("frame").style_dump()
}

fn check_golden(name: &str, files: &[&str]) {
    let actual = dump_of(files);
    let golden = fixtures().join(format!("{name}.dump"));
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(&golden, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|_| panic!("missing {}; run with UPDATE_FIXTURES=1", golden.display()));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

#[test]
fn golden_frames() {
    for name in [
        "sgr",
        "unicode",
        "altscreen-enter",
        "altscreen-exit",
        "cursor",
        "wrap-scroll",
        // zsh in tmux: `ESC k name ST` names the window; nothing is printed.
        "tmux-window-name",
    ] {
        check_golden(name, &[&format!("{name}.vt")]);
    }
    check_golden(
        "incomplete",
        &["incomplete-a.vt", "incomplete-b.vt", "incomplete-c.vt"],
    );
}

#[test]
fn split_input_matches_whole_input_at_every_boundary() {
    let mut whole = Vec::new();
    for f in [
        "sgr.vt",
        "unicode.vt",
        "incomplete-a.vt",
        "incomplete-b.vt",
        "incomplete-c.vt",
    ] {
        whole.extend(std::fs::read(fixtures().join(f)).expect("fixture"));
    }
    let expected = {
        let mut e = engine();
        e.feed(&whole).expect("feed");
        e.frame().expect("frame").style_dump()
    };
    for cut in 0..=whole.len() {
        let mut e = engine();
        e.feed(&whole[..cut]).expect("feed a");
        e.feed(&whole[cut..]).expect("feed b");
        assert_eq!(
            e.frame().expect("frame").style_dump(),
            expected,
            "cut at {cut}"
        );
    }
}

#[test]
fn unchanged_rows_are_shared_and_damage_is_reported() {
    let mut e = engine();
    e.feed(b"row zero\r\nrow one\r\n").expect("feed");
    let first = e.frame().expect("frame");
    assert_eq!(first.damaged_rows.len(), usize::from(SIZE.rows));

    let same = e.frame().expect("frame");
    assert!(
        Arc::ptr_eq(&first, &same),
        "an unchanged engine returns the same frame"
    );

    e.feed(b"\x1b[4;1Hrow three").expect("feed");
    let second = e.frame().expect("frame");
    assert_eq!(second.revision, first.revision + 1);
    assert!(second.damaged_rows.contains(&3));
    assert!(!second.damaged_rows.contains(&0));
    assert!(Arc::ptr_eq(&first.rows[0], &second.rows[0]));
    assert!(Arc::ptr_eq(&first.colors, &second.colors));
    assert_eq!(second.rows[3].text(), "row three");
}

#[test]
fn replies_are_suppressed_by_default() {
    let mut e = engine();
    let fx = e.feed(b"\x1b[c\x1b[6n").expect("feed");
    assert!(fx.replies.is_empty(), "{fx:?}");
}

#[test]
fn replies_are_delivered_when_captured() {
    let mut config = EngineConfig::new(SIZE);
    config.capture_replies = true;
    let mut e = Engine::new(config).expect("engine");
    let fx = e.feed(b"\x1b[3;5H\x1b[6n").expect("feed");
    assert_eq!(fx.replies, [(ReplySource::Protocol, b"\x1b[3;5R".to_vec())]);
}

#[test]
fn effects_are_reported_once() {
    let mut e = engine();
    let fx = e
        .feed(b"\x07\x07\x1b]2;title one\x07\x1b]7;file://host/tmp/x\x07\x1b[?2026h")
        .expect("feed");
    assert_eq!(fx.bells, 2);
    assert_eq!(fx.title.as_deref(), Some("title one"));
    assert_eq!(fx.pwd.as_deref(), Some("file://host/tmp/x"));
    assert_eq!(fx.render_hold, [true]);
    assert!(e.frame().expect("frame").render_hold);

    let fx = e.feed(b"plain\x1b[?2026l").expect("feed");
    assert_eq!(fx.bells, 0);
    assert_eq!(fx.title, None);
    assert_eq!(fx.render_hold, [false]);
    assert!(!e.frame().expect("frame").render_hold);
}

#[test]
fn shell_integration_reports_command_steps_and_exit_status() {
    use terminal_ghostty::core::CommandMark;
    let mut e = engine();
    let fx = e
        .feed(b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;3\x07")
        .expect("feed");
    assert_eq!(
        fx.commands,
        [
            CommandMark::PromptStart,
            CommandMark::InputStart,
            CommandMark::OutputStart { command: None },
            CommandMark::End { exit_code: Some(3) },
        ]
    );
    let fx = e.feed(b"\x1b]133;D\x07").expect("feed");
    assert_eq!(fx.commands, [CommandMark::End { exit_code: None }]);
}

#[test]
fn resize_changes_the_frame_grid() {
    let mut e = engine();
    e.feed(b"hello").expect("feed");
    e.resize(GridSize { cols: 8, rows: 3 }, (9, 18))
        .expect("resize");
    let f = e.frame().expect("frame");
    assert_eq!(f.size, GridSize { cols: 8, rows: 3 });
    assert_eq!(f.rows.len(), 3);
    assert!(f.rows.iter().all(|r| r.cells.len() == 8));
    assert!(e.resize(GridSize { cols: 0, rows: 3 }, (9, 18)).is_err());
    assert!(Engine::new(EngineConfig::new(GridSize { cols: 4, rows: 0 })).is_err());
}

#[test]
fn every_required_feature_is_compiled_in() {
    let f = features();
    assert_eq!(f.missing_required(), Vec::<&str>::new(), "{f:?}");
    assert!(
        f.version
            .starts_with(terminal_core::family::GHOSTTY_VERSION)
    );
}

/// The build family peers compare (terminal_core::family) must describe
/// the library actually linked and the pins in deps/baseline.toml.
#[test]
fn build_family_matches_the_linked_library_and_the_pins() {
    use terminal_core::family::{GHOSTTY_COMMIT, GHOSTTY_VERSION, REQUIRED_FEATURES};
    let baseline = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deps/baseline.toml"),
    )
    .expect("baseline");
    let ghostty: Vec<&str> = baseline
        .split("[ghostty]")
        .nth(1)
        .and_then(|s| s.split("\n[").next())
        .expect("[ghostty] section")
        .lines()
        .collect();
    assert!(ghostty.contains(&format!("commit = \"{GHOSTTY_COMMIT}\"").as_str()));
    assert!(ghostty.contains(&format!("version = \"{GHOSTTY_VERSION}\"").as_str()));
    let f = features();
    assert!(f.version.starts_with(GHOSTTY_VERSION), "{}", f.version);
    // Every feature the engine requires is part of the family, and back.
    let required: Vec<&str> = {
        let mut none = f.clone();
        none.kitty_graphics = false;
        none.snapshot = false;
        none.formatter = false;
        none.selection = false;
        none.search = false;
        none.render_state = false;
        none.input_encode = false;
        none.color = false;
        none.grid_introspection = false;
        none.missing_required()
    };
    assert_eq!(required, REQUIRED_FEATURES);
}

#[test]
fn modes_cursor_and_scrollback_are_queryable() {
    let mut e = engine();
    assert!(!e.mode(1, false).expect("decckm"));
    assert!(e.mode(7, false).expect("decawm"));
    e.feed(b"\x1b[?1h\x1b[?2004h\x1b[4h\x1b[?25l\x1b[3;4H")
        .expect("feed");
    assert!(e.mode(1, false).expect("decckm"));
    assert!(e.mode(2004, false).expect("bracketed paste"));
    assert!(e.mode(4, true).expect("insert"));
    assert!(!e.mode(25, false).expect("cursor visible"));
    assert_eq!(e.cursor_position().expect("cursor"), (3, 2));
    for i in 0..10 {
        e.feed(format!("\r\nline {i}").as_bytes()).expect("feed");
    }
    assert!(e.scrollback_rows().expect("scrollback") >= 6);
}

#[test]
fn input_modes_follow_what_the_application_set() {
    let mut e = engine();
    let m = e.input_modes().expect("modes");
    assert_eq!(
        (m.cursor_keys, m.bracketed_paste, m.alt_screen, m.mouse_sgr),
        (false, false, false, false)
    );
    assert_eq!(m.kitty_keyboard, 0);
    // DECCKM, DECNKM, any-event mouse in SGR, focus, bracketed paste, the
    // alternate screen, LNM, and kitty keyboard flags 1|4.
    e.feed(
        b"\x1b[?1h\x1b[?66h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1049h\x1b[20h\x1b[>5u",
    )
    .expect("feed");
    let m = e.input_modes().expect("modes");
    assert!(m.cursor_keys && m.keypad_keys && m.mouse_any && m.mouse_sgr);
    assert!(m.focus_events && m.bracketed_paste && m.alt_screen && m.linefeed);
    assert!(!m.mouse_normal && !m.mouse_utf8 && !m.keyboard_locked);
    assert_eq!(m.kitty_keyboard, 5);
    e.feed(b"\x1b[<u\x1b[?1049l\x1b[?1l").expect("feed");
    let m = e.input_modes().expect("modes");
    assert!(!m.cursor_keys && !m.alt_screen);
    assert_eq!(m.kitty_keyboard, 0);
}

#[test]
fn screen_text_includes_scrollback() {
    let mut e = engine();
    for i in 0..30 {
        e.feed(format!("line {i}\r\n").as_bytes()).expect("feed");
    }
    let text = e.screen_text().expect("text");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.first(), Some(&"line 0"), "{text}");
    assert!(lines.contains(&"line 29"));
}

#[test]
fn the_viewport_scrolls_locally_and_stays_put_under_new_output() {
    let mut e = engine();
    let rows = u64::from(SIZE.rows);
    for i in 0..100 {
        e.feed(format!("line {i}\r\n").as_bytes()).expect("feed");
    }
    let top = |e: &mut Engine| {
        e.frame().expect("frame").rows[0]
            .text()
            .trim_end()
            .to_owned()
    };
    let following = e.frame().expect("frame").view;
    assert_eq!(following.rows_back(), 0);
    assert_eq!(following.len, rows);
    let bottom_top = top(&mut e);

    e.scroll_viewport(Scroll::Delta(-5)).expect("scroll");
    let f = e.frame().expect("frame");
    assert_eq!(f.view.rows_back(), 5);
    let scrolled_top = top(&mut e);
    assert_ne!(scrolled_top, bottom_top);

    // New output does not move a scrolled viewport.
    e.feed(b"more\r\nand more\r\n").expect("feed");
    assert_eq!(top(&mut e), scrolled_top);
    assert_eq!(e.frame().expect("frame").view.rows_back(), 7);

    e.scroll_viewport(Scroll::Top).expect("top");
    assert_eq!(e.frame().expect("frame").view.offset, 0);
    e.scroll_viewport(Scroll::Bottom).expect("bottom");
    assert_eq!(e.frame().expect("frame").view.rows_back(), 0);

    let v = e.frame().expect("frame").view;
    assert!(!v.mouse_tracking && !v.alt_screen);
    e.feed(b"\x1b[?1000h\x1b[?1049h").expect("feed");
    let v = e.frame().expect("frame").view;
    assert!(v.mouse_tracking && v.alt_screen);
}

mod selection {
    use super::*;
    use terminal_ghostty::selection::{CellPos, SelectEvent};

    fn at(x: f32, y: f32) -> CellPos {
        CellPos { x, y }
    }

    /// Press at `from`, drag to `to`, release; the selected text.
    fn drag(e: &mut Engine, from: (u16, u16), to: (u16, u16), t: u64) -> Option<String> {
        // Forward drags: press on the left half of the first cell and end
        // on the right half of the last, so both are included.
        let c = |(x, y): (u16, u16), dx: f32| at(f32::from(x) + dx, f32::from(y) + 0.5);
        e.select(SelectEvent::Press {
            at: c(from, 0.1),
            time_ns: t,
        })
        .expect("press");
        e.select(SelectEvent::Drag { at: c(to, 0.9) })
            .expect("drag");
        e.select(SelectEvent::Release { at: c(to, 0.9) })
            .expect("release");
        e.selection_text().expect("text")
    }

    fn click(e: &mut Engine, x: u16, y: u16, t: u64) {
        let p = at(f32::from(x) + 0.5, f32::from(y) + 0.5);
        e.select(SelectEvent::Press { at: p, time_ns: t })
            .expect("press");
        e.select(SelectEvent::Release { at: p }).expect("release");
    }

    #[test]
    fn drag_word_and_line_select_the_expected_text_without_changing_cells() {
        let mut e = engine();
        // Row 0: "hello world"; row 1: wide characters; rows 2-3: one
        // line wrapped across two rows (20 columns).
        e.feed("hello world\r\n日本語 ok\r\nabcdefghijklmnopqrstuvwxyz\r\n".as_bytes())
            .expect("feed");
        let before = e.frame().expect("frame").text_dump();

        assert_eq!(drag(&mut e, (0, 0), (4, 0), 1).as_deref(), Some("hello"));
        // Wide cells: columns 0..5 hold three double-width characters.
        assert_eq!(
            drag(&mut e, (0, 1), (5, 1), 10_000_000_000).as_deref(),
            Some("日本語")
        );
        // Across the wrap: one line, no newline inserted.
        // Row 2 holds a..t, row 3 holds u..z: p to w crosses the wrap.
        assert_eq!(
            drag(&mut e, (15, 2), (2, 3), 20_000_000_000).as_deref(),
            Some("pqrstuvw")
        );
        let f = e.frame().expect("frame");
        assert!(
            f.selection
                .iter()
                .any(|s| s.row == 2 && s.start == 15 && s.end == 19),
            "{:?}",
            f.selection
        );

        // Double click: the word; triple click: the whole line.
        let t = 30_000_000_000;
        click(&mut e, 8, 0, t);
        click(&mut e, 8, 0, t + 100_000_000);
        assert_eq!(e.selection_text().expect("text").as_deref(), Some("world"));
        e.select(SelectEvent::Press {
            at: at(8.5, 0.5),
            time_ns: t + 200_000_000,
        })
        .expect("third");
        assert_eq!(
            e.selection_text().expect("text").as_deref(),
            Some("hello world")
        );

        // A later single click clears it; the cells never changed.
        click(&mut e, 2, 4, t + 5_000_000_000);
        assert_eq!(e.selection_text().expect("text"), None);
        assert!(e.frame().expect("frame").selection.is_empty());
        assert_eq!(e.frame().expect("frame").text_dump(), before);
    }
}

#[test]
fn osc8_links_are_hit_tested_over_their_cells_and_wraps() {
    use terminal_ghostty::core::SelectedCells;
    let mut e = engine();
    let link = |uri: &str, text: &str| format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\");
    // Row 0: "see " + link "docs" + " here"; row 1: a wide-character link;
    // rows 2-3: a link that soft-wraps (20 columns).
    let input = format!(
        "see {} here\r\n{} x\r\n0123456789abcd{}\r\n",
        link("https://example.com/docs", "docs"),
        link("https://example.com/日本", "日本"),
        link("https://example.com/wrap", "WRAPPED-LINK"),
    );
    e.feed(input.as_bytes()).expect("feed");

    assert_eq!(e.link_at(0, 0).expect("hit"), None, "plain text");
    let docs = e.link_at(5, 0).expect("hit").expect("a link");
    assert_eq!(docs.uri, "https://example.com/docs");
    assert_eq!(
        docs.cells,
        vec![SelectedCells {
            row: 0,
            start: 4,
            end: 7
        }]
    );
    assert_eq!(e.link_at(8, 0).expect("hit"), None, "the space after it");

    // Both halves of a wide character hit the link.
    let wide = e.link_at(3, 1).expect("hit").expect("wide link");
    assert_eq!(wide.uri, "https://example.com/日本");
    assert_eq!(
        wide.cells,
        vec![SelectedCells {
            row: 1,
            start: 0,
            end: 3
        }]
    );

    // Hitting the wrapped part finds the whole run on both rows.
    let wrapped = e.link_at(2, 3).expect("hit").expect("wrapped link");
    assert_eq!(wrapped.uri, "https://example.com/wrap");
    assert_eq!(
        wrapped.cells,
        vec![
            SelectedCells {
                row: 2,
                start: 14,
                end: 19
            },
            SelectedCells {
                row: 3,
                start: 0,
                end: 5
            },
        ]
    );
}

#[test]
fn clipboard_writes_are_captured_only_when_asked_and_never_cut() {
    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                out.push(if i <= c.len() {
                    T[(n >> (18 - 6 * i)) as usize & 63] as char
                } else {
                    '='
                });
            }
        }
        out
    }
    let osc52 = |text: &[u8]| format!("\x1b]52;c;{}\x07", b64(text)).into_bytes();

    // Off by default: a replica never acts on clipboard writes.
    let mut off = engine();
    assert!(
        off.feed(&osc52(b"secret"))
            .expect("feed")
            .clipboard
            .is_empty()
    );

    let mut config = EngineConfig::new(SIZE);
    config.capture_clipboard = true;
    let mut e = Engine::new(config).expect("engine");
    let fx = e.feed(&osc52("copied ✓".as_bytes())).expect("feed");
    assert_eq!(fx.clipboard, vec!["copied ✓".as_bytes().to_vec()]);
    // Too large: refused whole.
    let big = vec![b'x'; terminal_ghostty::effects_limits::MAX_CLIPBOARD_BYTES + 1];
    let fx = e.feed(&osc52(&big)).expect("feed");
    assert!(fx.clipboard.is_empty());
    assert_eq!(fx.clipboard_refused, 1);
}

#[test]
fn a_color_scheme_and_default_cursor_travel_in_the_snapshot() {
    let scheme = terminal_ghostty::core::schemes::find("catppuccin-mocha").expect("scheme");
    let mut canonical = engine();
    canonical.set_scheme(scheme).expect("scheme");
    canonical
        .set_default_cursor(terminal_ghostty::core::CursorShape::Bar, false)
        .expect("cursor");
    canonical
        .feed(b"hello \x1b[31mred\x1b[0m\r\n")
        .expect("feed");
    let colors = canonical.frame().expect("frame").colors.clone();
    assert_eq!(colors.background, scheme.background);
    assert_eq!(colors.foreground, scheme.foreground);
    assert_eq!(colors.palette[1], scheme.ansi[1]);
    assert_eq!(colors.palette[100], scheme.palette()[100]);
    let shape = canonical.frame().expect("frame").cursor.shape;
    assert_eq!(shape, terminal_ghostty::core::CursorShape::Bar);

    // A replica restored from the checkpoint, with no scheme of its own,
    // shows the same colors and cursor.
    let snapshot = canonical.snapshot().expect("snapshot");
    let replica = Engine::from_snapshot(&snapshot, &EngineConfig::new(SIZE)).expect("restore");
    let mut replica = replica;
    let restored = replica.frame().expect("frame");
    assert_eq!(*restored.colors, *colors, "colors differ after restore");
    assert_eq!(restored.cursor.shape, shape);
}

#[test]
fn a_programs_own_background_survives_a_scheme_change() {
    let schemes = terminal_ghostty::core::schemes::find;
    let (first, second) = (
        schemes("nord").expect("nord"),
        schemes("gruvbox-dark").expect("gruvbox"),
    );
    let mut canonical = engine();
    canonical.set_scheme(first).expect("scheme");
    // The program sets its own background and palette color 1.
    canonical
        .feed(b"\x1b]11;rgb:12/34/56\x1b\\\x1b]4;1;rgb:ab/cd/ef\x1b\\x")
        .expect("feed");
    canonical.set_scheme(second).expect("scheme");
    let own = terminal_ghostty::core::Rgb {
        r: 0x12,
        g: 0x34,
        b: 0x56,
    };
    let colors = canonical.frame().expect("frame").colors.clone();
    assert_eq!(colors.background, own, "the program's background was lost");
    assert_eq!(
        colors.palette[1],
        terminal_ghostty::core::Rgb {
            r: 0xab,
            g: 0xcd,
            b: 0xef
        }
    );
    assert_eq!(colors.foreground, second.foreground, "the new default");
    assert_eq!(colors.palette[2], second.ansi[2]);
    let snapshot = canonical.snapshot().expect("snapshot");
    let mut replica = Engine::from_snapshot(&snapshot, &EngineConfig::new(SIZE)).expect("restore");
    assert_eq!(*replica.frame().expect("frame").colors, *colors);
    // Resetting it (OSC 111) brings back the scheme's.
    // With nothing printed after it: a color change alone reaches the frame.
    canonical.feed(b"\x1b]111\x07").expect("feed");
    assert_eq!(
        canonical.frame().expect("frame").colors.background,
        second.background
    );
}

#[test]
fn the_tail_text_is_the_end_of_the_screen_text() {
    let tail_of = |e: &Engine, n: usize| {
        let all = e.screen_text().expect("text");
        let lines: Vec<&str> = all.split('\n').collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    };
    let mut cfg = EngineConfig::new(SIZE);
    cfg.scrollback_lines = Some(500);
    let mut e = Engine::new(cfg).expect("engine");
    // Scrollback, a wrapped row, blank rows, wide characters and styles.
    for i in 0..120 {
        e.feed(format!("line {i}\r\n").as_bytes()).expect("feed");
    }
    e.feed(b"a row long enough to wrap past twenty columns\r\n\r\n\r\n")
        .expect("feed");
    e.feed("\x1b[1mbold\x1b[0m 日本語 end\r\nlast".as_bytes())
        .expect("feed");
    for n in [1, 3, 6, 7, 20, 50, 200, 10_000] {
        assert_eq!(e.tail_text(n).expect("tail"), tail_of(&e, n), "{n} lines");
    }
    // The alternate screen has no scrollback.
    e.feed(b"\x1b[?1049halt one\r\nalt two").expect("feed");
    for n in [1, 6, 30] {
        assert_eq!(
            e.tail_text(n).expect("tail"),
            tail_of(&e, n),
            "alt, {n} lines"
        );
    }
}
