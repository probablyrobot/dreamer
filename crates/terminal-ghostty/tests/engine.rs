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
use terminal_ghostty::{Engine, EngineConfig, features};

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
    assert!(f.version.starts_with(ghostty_version_pin()));
}

fn ghostty_version_pin() -> &'static str {
    // Keep in step with deps/baseline.toml.
    "1.0.1"
}
