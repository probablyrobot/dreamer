//! Renderer fixture matrix: each fixture in
//! `crates/terminal-render/fixtures` must paint with the statistics listed
//! in `expected.txt` (derived from the fixture content). Used by the macOS
//! gate and by `gate linux-desktop`.

use std::path::{Path, PathBuf};

pub const FIXTURES: &str = "crates/terminal-render/fixtures";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub name: String,
    pub text_runs: usize,
    pub wide_cells: usize,
    pub decorated_runs: usize,
    pub background_quads: usize,
    /// `none` when the cursor is hidden.
    pub cursor: String,
    /// Image placements drawn, and how many of them the grid edge cut.
    pub images: usize,
    pub images_clipped: usize,
}

pub fn expectations(root: &Path) -> Result<Vec<(PathBuf, Expected)>, String> {
    let dir = root.join(FIXTURES);
    let text = std::fs::read_to_string(dir.join("expected.txt"))
        .map_err(|e| format!("expected.txt: {e}"))?;
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| -> Result<usize, String> {
                f.get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or(format!("expected.txt: bad line `{l}`"))
            };
            let e = Expected {
                name: f.first().copied().unwrap_or_default().to_owned(),
                text_runs: n(1)?,
                wide_cells: n(2)?,
                decorated_runs: n(3)?,
                background_quads: n(4)?,
                cursor: f.get(5).copied().unwrap_or("none").to_owned(),
                images: if f.len() > 6 { n(6)? } else { 0 },
                images_clipped: if f.len() > 7 { n(7)? } else { 0 },
            };
            Ok((dir.join(&e.name), e))
        })
        .collect()
}

fn field(stats: &str, name: &str) -> Option<usize> {
    let at = stats.find(&format!("{name}: "))? + name.len() + 2;
    stats[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Compare a `smoke: painted PaintStats { … }` line with the expectation.
pub fn check(stdout: &str, e: &Expected) -> Result<(), String> {
    let got = painted(stdout, &e.name)?;
    if &got == e {
        Ok(())
    } else {
        Err(format!("{}: expected {e:?}, painted {got:?}", e.name))
    }
}

/// What a `smoke: painted PaintStats { … }` line says was drawn: the
/// statistics that follow from the frame's content, not from caches.
pub fn painted(stdout: &str, name: &str) -> Result<Expected, String> {
    let stats = stdout
        .lines()
        .find_map(|l| l.strip_prefix("smoke: painted "))
        .ok_or(format!("{name}: no paint statistics in the output"))?;
    let cursor = match stats.find("cursor: Some(\"") {
        Some(at) => stats[at + 14..]
            .split('"')
            .next()
            .unwrap_or_default()
            .to_owned(),
        None => "none".to_owned(),
    };
    Ok(Expected {
        name: name.to_owned(),
        text_runs: field(stats, "text_runs").unwrap_or(usize::MAX),
        wide_cells: field(stats, "wide_cells").unwrap_or(usize::MAX),
        decorated_runs: field(stats, "decorated_runs").unwrap_or(usize::MAX),
        background_quads: field(stats, "background_quads").unwrap_or(usize::MAX),
        cursor,
        images: field(stats, "images").unwrap_or(usize::MAX),
        images_clipped: field(stats, "images_clipped").unwrap_or(usize::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_lines_are_compared_field_by_field() {
        let e = Expected {
            name: "x.vt".into(),
            text_runs: 8,
            wide_cells: 0,
            decorated_runs: 6,
            background_quads: 1,
            cursor: "block".into(),
            images: 0,
            images_clipped: 0,
        };
        let line = "smoke: painted PaintStats { background_quads: 1, text_runs: 8, wide_cells: 0, images: 0, image_uploads: 0, images_clipped: 0, shaped_lines: 8, reused_lines: 0, decorated_runs: 6, cursor: Some(\"block\") }";
        assert!(check(line, &e).is_ok());
        assert!(check(&line.replace("text_runs: 8", "text_runs: 7"), &e).is_err());
        let hidden = Expected {
            cursor: "none".into(),
            ..e.clone()
        };
        assert!(check(&line.replace("Some(\"block\")", "None"), &hidden).is_ok());
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let all = expectations(&root).expect("the checked-in expectations parse");
        assert!(all.len() >= 6 && all.iter().all(|(p, _)| p.is_file()));
    }
}
