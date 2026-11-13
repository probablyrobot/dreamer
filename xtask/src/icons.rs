//! `cargo xtask icons [--check]`: draw the application icon, a halftone
//! moon, into the files the packages install. With `--check`, fail if the
//! committed files are not what this code draws.
//!
//! The icon is a dark rounded tile holding a disc of 45° hatch lines. On
//! the lit side the lines are white and thicken toward the bright limb
//! until they merge; past the terminator they are thin and gray. Every
//! output comes from the one geometry below:
//!
//! - `packaging/icons/dreamer.svg`: the scalable icon (Linux `scalable`);
//! - `packaging/icons/linux/<n>.png`: hicolor sizes for Linux;
//! - `packaging/icons/macos/Dreamer.icns`: the macOS bundle icon.
//!
//! Small sizes keep the hatch visible by holding a minimum line period in
//! pixels, so a 16 px icon has fewer, coarser lines than a 1024 px one.

use std::path::{Path, PathBuf};

use crate::util::repo_root;

/// All lengths are fractions of the tile's side.
const CORNER: f64 = 0.2;
const MOON_X: f64 = 0.5;
const MOON_Y: f64 = 0.5;
const MOON_R: f64 = 0.316;
/// Distance between neighbouring hatch lines, across the lines.
const PERIOD: f64 = 0.02;
/// Direction the light comes from, in degrees, screen coordinates (y down).
const LIGHT_DEG: f64 = 199.0;
/// Where the terminator crosses the light axis, as a fraction of the
/// radius toward the light (0 is a half moon, 1 is new moon).
const TERMINATOR: f64 = 0.37;
/// Width of a dark-side line, as a fraction of the period.
const DARK_WIDTH: f64 = 0.17;
/// Line centres sit at `(j + PHASE) * PERIOD` across the disc.
const PHASE: f64 = 0.25;
/// No line starts nearer the upper-left limb than this many radii from
/// the centre, which flattens that edge of the moon.
const FIRST_LINE: f64 = -0.75;

const TILE: [u8; 3] = [0x0b, 0x0b, 0x0c];
const LIT: [u8; 3] = [0xf2, 0xf2, 0xf0];
const DARK: [u8; 3] = [0x5e, 0x5e, 0x60];

/// Smallest line period and dark-line width a raster keeps, in pixels.
const MIN_PERIOD_PX: f64 = 2.5;
const MIN_DARK_PX: f64 = 0.9;

/// Linux icons fill most of the square; macOS icons sit on Apple's grid
/// (an 824 tile on a 1024 canvas).
const LINUX_TILE: f64 = 15.0 / 16.0;
const MACOS_TILE: f64 = 824.0 / 1024.0;

const LINUX_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];
/// `icns` entry types and the pixel size each holds.
const ICNS: [(&[u8; 4], u32); 10] = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
];

/// Width of the lit line whose centre sits `s` radii across the disc
/// (negative toward the upper left), as a fraction of the period. Lines
/// near the bright edge are widest; the last ones before the terminator
/// are thin.
fn lit_width(s: f64) -> f64 {
    (0.8 - 1.67 * (s + 0.7)).clamp(0.18, 0.85)
}

/// `(cos, sin)` of `LIGHT_DEG`, written out so the drawing does not depend
/// on the platform's trigonometry and `--check` matches on every host.
fn light() -> (f64, f64) {
    (-0.945_518_575_599_316_7, -0.325_568_154_457_156_76)
}

/// Whether a point `(dx, dy)` from the moon's centre, inside the disc, is
/// on the lit side of the terminator.
fn lit(dx: f64, dy: f64) -> bool {
    let (lx, ly) = light();
    let u = dx * lx + dy * ly;
    let v = -dx * ly + dy * lx;
    u > TERMINATOR * (MOON_R * MOON_R - v * v).max(0.0).sqrt()
}

/// Inside the rounded tile `[0, 1]²`.
fn in_tile(x: f64, y: f64) -> bool {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return false;
    }
    let cx = x.clamp(CORNER, 1.0 - CORNER);
    let cy = y.clamp(CORNER, 1.0 - CORNER);
    (x - cx).powi(2) + (y - cy).powi(2) <= CORNER * CORNER
}

/// The color at tile point `(x, y)`, or `None` outside the tile. `period`
/// and `dark` are the line period and dark-line width in tile units.
fn shade(x: f64, y: f64, period: f64, dark: f64) -> Option<[u8; 3]> {
    if !in_tile(x, y) {
        return None;
    }
    let (dx, dy) = (x - MOON_X, y - MOON_Y);
    if dx * dx + dy * dy > MOON_R * MOON_R {
        return Some(TILE);
    }
    // Distance across the "/" lines, and the nearest line.
    let across = (dx + dy) / std::f64::consts::SQRT_2;
    let centre = ((across / period - PHASE).round() + PHASE) * period;
    if centre / MOON_R < FIRST_LINE {
        return Some(TILE);
    }
    let off = (across - centre).abs();
    if lit(dx, dy) {
        if off <= lit_width(centre / MOON_R) * period / 2.0 {
            return Some(LIT);
        }
    } else if off <= dark / 2.0 {
        return Some(DARK);
    }
    Some(TILE)
}

/// Draw the icon on a `size`-pixel canvas with the tile covering `tile` of
/// it, as straight (not premultiplied) RGBA.
fn raster(size: u32, tile: f64) -> Vec<u8> {
    let side = f64::from(size) * tile;
    let margin = (f64::from(size) - side) / 2.0;
    let period = PERIOD.max(MIN_PERIOD_PX / side);
    let dark = (DARK_WIDTH * period).max(MIN_DARK_PX / side);
    let n = if size >= 256 { 4 } else { 8 };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut sum, mut hits) = ([0.0f64; 3], 0u32);
            for sy in 0..n {
                for sx in 0..n {
                    let x = (f64::from(px) + (f64::from(sx) + 0.5) / f64::from(n) - margin) / side;
                    let y = (f64::from(py) + (f64::from(sy) + 0.5) / f64::from(n) - margin) / side;
                    if let Some(c) = shade(x, y, period, dark) {
                        for (s, c) in sum.iter_mut().zip(c) {
                            *s += f64::from(c);
                        }
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                for s in sum {
                    out.push((s / f64::from(hits)).round() as u8);
                }
                out.push((f64::from(hits) * 255.0 / f64::from(n * n)).round() as u8);
            }
        }
    }
    out
}

fn encode_png(size: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut enc = png::Encoder::new(&mut bytes, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn png(size: u32, tile: f64) -> Result<Vec<u8>, String> {
    encode_png(size, &raster(size, tile))
}

/// An `icns` file of PNG entries.
fn icns() -> Result<Vec<u8>, String> {
    let mut cache: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut body = Vec::new();
    for (kind, size) in ICNS {
        let data = match cache.iter().find(|(s, _)| *s == size) {
            Some((_, d)) => d.clone(),
            None => {
                let d = png(size, MACOS_TILE)?;
                cache.push((size, d.clone()));
                d
            }
        };
        body.extend_from_slice(kind);
        body.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(&data);
    }
    let mut out = b"icns".to_vec();
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// The scalable icon, on a 1024 canvas with the Linux margin.
fn svg() -> String {
    let side = 1024.0 * LINUX_TILE;
    let m = (1024.0 - side) / 2.0;
    let at = |v: f64| m + v * side;
    let len = |v: f64| v * side;
    let (cx, cy, r) = (at(MOON_X), at(MOON_Y), len(MOON_R));
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let k = TERMINATOR * r;
    // Both halves, drawn in a frame turned so the light is along +x.
    let lit = format!(
        "M{cx:.2} {:.2}A{r:.2} {r:.2} 0 0 1 {cx:.2} {:.2}A{k:.2} {r:.2} 0 0 0 {cx:.2} {:.2}Z",
        cy - r,
        cy + r,
        cy - r
    );
    let dark = format!(
        "M{cx:.2} {:.2}A{r:.2} {r:.2} 0 0 0 {cx:.2} {:.2}A{k:.2} {r:.2} 0 0 0 {cx:.2} {:.2}Z",
        cy - r,
        cy + r,
        cy - r
    );
    let turn = format!("rotate({LIGHT_DEG} {cx:.2} {cy:.2})");
    let p = len(PERIOD);
    let mut lit_lines = String::new();
    let mut dark_lines = String::new();
    let steps = (r / p).ceil() as i64;
    for j in -steps - 1..=steps {
        let across = (j as f64 + PHASE) * p;
        if across / r < FIRST_LINE || across > r {
            continue;
        }
        // Centre of line j, and its ends one radius either way along "/".
        let (x, y) = (
            cx + across / std::f64::consts::SQRT_2,
            cy + across / std::f64::consts::SQRT_2,
        );
        let d = r / std::f64::consts::SQRT_2 * 1.01;
        let seg = format!("M{:.2} {:.2}L{:.2} {:.2}", x - d, y + d, x + d, y - d);
        let w = lit_width(across / r) * p;
        lit_lines.push_str(&format!("<path d=\"{seg}\" stroke-width=\"{w:.2}\"/>"));
        dark_lines.push_str(&format!("<path d=\"{seg}\"/>"));
    }
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
<title>Dreamer</title>
<defs>
<clipPath id="lit"><path transform="{turn}" d="{lit}"/></clipPath>
<clipPath id="dark"><path transform="{turn}" d="{dark}"/></clipPath>
</defs>
<rect x="{m:.2}" y="{m:.2}" width="{side:.2}" height="{side:.2}" rx="{rx:.2}" fill="{tile}"/>
<g clip-path="url(#dark)" fill="none" stroke="{dark_c}" stroke-width="{dw:.2}">{dark_lines}</g>
<g clip-path="url(#lit)" fill="none" stroke="{lit_c}">{lit_lines}</g>
</svg>
"#,
        rx = len(CORNER),
        tile = hex(TILE),
        dark_c = hex(DARK),
        lit_c = hex(LIT),
        dw = DARK_WIDTH * p,
    )
}

/// Every file this command owns, with its contents.
fn outputs(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let dir = root.join("packaging/icons");
    let mut out = vec![(dir.join("dreamer.svg"), svg().into_bytes())];
    for size in LINUX_SIZES {
        out.push((
            dir.join("linux").join(format!("{size}.png")),
            png(size, LINUX_TILE)?,
        ));
    }
    out.push((dir.join("macos/Dreamer.icns"), icns()?));
    Ok(out)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask icons [--check]".into()),
    };
    let root = repo_root();
    let mut stale = Vec::new();
    for (path, bytes) in outputs(&root)? {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if check {
            if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                stale.push(rel);
            }
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, &bytes).map_err(|e| format!("{rel}: {e}"))?;
        println!("wrote {rel}");
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "icons are out of date; run `cargo xtask icons`: {}",
            stale.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_light_constants_match_the_angle() {
        let a = LIGHT_DEG.to_radians();
        let (c, s) = light();
        assert!((c - a.cos()).abs() < 1e-12 && (s - a.sin()).abs() < 1e-12);
    }

    #[test]
    fn the_moon_is_lit_toward_the_light_and_dark_away_from_it() {
        let (lx, ly) = light();
        let r = MOON_R * 0.9;
        assert!(lit(lx * r, ly * r));
        assert!(!lit(-lx * r, -ly * r));
        // The terminator sits past the centre, so the centre is dark.
        assert!(!lit(0.0, 0.0));
    }

    #[test]
    fn corners_outside_the_rounding_are_transparent() {
        assert!(shade(0.01, 0.01, PERIOD, DARK_WIDTH * PERIOD).is_none());
        assert_eq!(shade(0.5, 0.05, PERIOD, DARK_WIDTH * PERIOD), Some(TILE));
    }
}
