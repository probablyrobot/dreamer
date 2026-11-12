//! Write this architecture's snapshot fixtures.
//!
//! usage: snapshot-fixtures [OUT_DIR]
//! Default: tests/fixtures/snapshots/<this target>/

use std::process::ExitCode;

use terminal_ghostty::snapshot_fixtures::{fixture_root, generate, this_target};

fn main() -> ExitCode {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| fixture_root().join("snapshots").join(this_target()));
    match generate(&dir) {
        Ok(paths) => {
            for p in paths {
                println!("{}", p.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("snapshot-fixtures: {e}");
            ExitCode::FAILURE
        }
    }
}
