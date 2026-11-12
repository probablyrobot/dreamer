//! Snapshot interop across architectures (spec sections 11.5 and 18.2).
//!
//! Every architecture's checked-in snapshots must decode on this one into a
//! terminal that matches a native replay of the same stream, both before and
//! after a continuation stream that completes unfinished input. This
//! architecture's own snapshots must also regenerate byte for byte.
//! Regenerate with `cargo run -p terminal-ghostty --bin snapshot-fixtures`.

use terminal_ghostty::Engine;
use terminal_ghostty::snapshot_fixtures::{
    TARGETS, config, fixture_root, replay, semantic_state, source_bytes, sources, this_target,
};

#[test]
fn every_architectures_snapshots_restore_exactly() {
    let names = sources().expect("sources");
    assert!(!names.is_empty());
    let mut checked = 0;
    for target in TARGETS {
        let dir = fixture_root().join("snapshots").join(target);
        assert!(
            dir.is_dir(),
            "missing snapshots for {target}; generate them on that architecture"
        );
        for name in &names {
            let path = dir.join(format!("{name}.snap"));
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let mut restored = Engine::from_snapshot(&bytes, &config())
                .unwrap_or_else(|e| panic!("{target}/{name}: decode failed: {e}"));
            let mut native = replay(name).expect("replay");
            assert_eq!(
                semantic_state(&mut restored).expect("state"),
                semantic_state(&mut native).expect("state"),
                "{target}/{name}: restored state differs from native replay"
            );
            if let Some(cont) = source_bytes(name, "cont").expect("cont") {
                restored.feed(&cont).expect("feed restored");
                native.feed(&cont).expect("feed native");
                assert_eq!(
                    semantic_state(&mut restored).expect("state"),
                    semantic_state(&mut native).expect("state"),
                    "{target}/{name}: state differs after continuation"
                );
            }
            checked += 1;
        }
    }
    eprintln!(
        "checked {checked} snapshots from {} architectures",
        TARGETS.len()
    );
}

#[test]
fn this_architectures_snapshots_regenerate_identically() {
    let dir = fixture_root().join("snapshots").join(this_target());
    let fresh = std::env::temp_dir().join(format!("dreamer-snapshots-{}", std::process::id()));
    let written = terminal_ghostty::snapshot_fixtures::generate(&fresh).expect("generate");
    for path in written {
        let name = path.file_name().expect("name");
        let checked_in = std::fs::read(dir.join(name)).unwrap_or_default();
        let now = std::fs::read(&path).expect("read");
        assert!(
            checked_in == now,
            "{}/{} changed; regenerate fixtures if the pin changed",
            this_target(),
            name.to_string_lossy()
        );
    }
    let _ = std::fs::remove_dir_all(&fresh);
}

#[test]
fn corrupted_and_truncated_snapshots_are_rejected() {
    let mut e = replay("basic").expect("replay");
    let snap = e.snapshot().expect("snapshot");
    let _ = e.frame();
    assert!(Engine::from_snapshot(&snap[..snap.len() / 2], &config()).is_err());
    assert!(Engine::from_snapshot(b"not a snapshot", &config()).is_err());
    assert!(Engine::from_snapshot(&[], &config()).is_err());
}
