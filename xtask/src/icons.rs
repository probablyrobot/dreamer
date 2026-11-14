//! `cargo xtask icons [--check]`: render the application icon into the
//! raster files the packages install. With `--check`, fail if the
//! committed rasters are not what the design renders to.
//!
//! The design is `packaging/icons/dreamer.svg`, a halftone hatch moon on
//! a dark rounded tile. It is the source and is installed as it is (the
//! Linux `scalable` icon); this command never writes it. From it come:
//!
//! - `packaging/icons/linux/<n>.png`: hicolor sizes for Linux;
//! - `packaging/icons/macos/Dreamer.icns`: the macOS bundle icon.
//!
//! The check compares pixels, not bytes, and allows each channel to differ
//! by a few levels: the rasteriser's SIMD paths round differently on
//! x86-64 and arm64, and a level is not a different icon.

use std::path::{Path, PathBuf};

use resvg::{tiny_skia, usvg};

use crate::util::repo_root;

const SOURCE: &str = "packaging/icons/dreamer.svg";

/// Linux icons fill most of the square; macOS icons sit on Apple's grid
/// (an 824 tile on a 1024 canvas).
const LINUX_TILE: f32 = 15.0 / 16.0;
const MACOS_TILE: f32 = 824.0 / 1024.0;

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

/// Largest per-channel difference `--check` accepts.
const MAX_DELTA: u8 = 3;

fn design(root: &Path) -> Result<usvg::Tree, String> {
    let data = std::fs::read(root.join(SOURCE)).map_err(|e| format!("{SOURCE}: {e}"))?;
    usvg::Tree::from_data(&data, &usvg::Options::default()).map_err(|e| format!("{SOURCE}: {e}"))
}

/// The design on a `size`-pixel canvas with its tile covering `tile` of
/// it, as straight (not premultiplied) RGBA.
fn raster(tree: &usvg::Tree, size: u32, tile: f32) -> Result<Vec<u8>, String> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("empty icon size")?;
    let side = size as f32 * tile;
    let margin = (size as f32 - side) / 2.0;
    let scale = side / tree.size().width().max(tree.size().height());
    let at = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, margin, margin);
    resvg::render(tree, at, &mut pixmap.as_mut());
    Ok(pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect())
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

/// A PNG's width and straight RGBA pixels.
fn decode_png(bytes: &[u8]) -> Result<(u32, Vec<u8>), String> {
    let dec = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut r = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; r.output_buffer_size().ok_or("png too large")?];
    let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("not 8-bit RGBA".into());
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, buf))
}

/// One raster file.
enum Raster {
    Png(u32),
    Icns,
}

/// The images a file holds, in order: pixel size and tile fraction.
fn entries(kind: &Raster) -> Vec<(u32, f32)> {
    match kind {
        Raster::Png(size) => vec![(*size, LINUX_TILE)],
        Raster::Icns => ICNS.iter().map(|(_, s)| (*s, MACOS_TILE)).collect(),
    }
}

fn files(root: &Path) -> Vec<(PathBuf, Raster)> {
    let dir = root.join("packaging/icons");
    let mut out: Vec<_> = LINUX_SIZES
        .iter()
        .map(|&s| (dir.join("linux").join(format!("{s}.png")), Raster::Png(s)))
        .collect();
    out.push((dir.join("macos/Dreamer.icns"), Raster::Icns));
    out
}

/// An `icns` file of PNG entries.
fn icns(pngs: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for ((kind, _), data) in ICNS.iter().zip(pngs) {
        body.extend_from_slice(*kind);
        body.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(data);
    }
    let mut out = b"icns".to_vec();
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// The entries of an `icns` file, in order.
fn icns_entries(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut out = Vec::new();
    let mut at = 8;
    while at + 8 <= bytes.len() {
        let mut len = [0; 4];
        len.copy_from_slice(&bytes[at + 4..at + 8]);
        let len = u32::from_be_bytes(len) as usize;
        if len < 8 || at + len > bytes.len() {
            return Err("truncated icns".into());
        }
        out.push(&bytes[at + 8..at + len]);
        at += len;
    }
    Ok(out)
}

/// Whether a committed file holds what the design renders to.
fn current(tree: &usvg::Tree, kind: &Raster, bytes: &[u8]) -> Result<bool, String> {
    let pngs = match kind {
        Raster::Png(_) => vec![bytes],
        Raster::Icns => icns_entries(bytes)?,
    };
    let want = entries(kind);
    if pngs.len() != want.len() {
        return Ok(false);
    }
    for (png, (size, tile)) in pngs.into_iter().zip(want) {
        let (width, got) = decode_png(png)?;
        let fresh = raster(tree, size, tile)?;
        if width != size
            || got.len() != fresh.len()
            || got
                .iter()
                .zip(&fresh)
                .any(|(a, b)| a.abs_diff(*b) > MAX_DELTA)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask icons [--check]".into()),
    };
    let root = repo_root();
    let tree = design(&root)?;
    let mut stale = Vec::new();
    for (path, kind) in files(&root) {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if check {
            let ok = std::fs::read(&path)
                .ok()
                .is_some_and(|bytes| current(&tree, &kind, &bytes).unwrap_or(false));
            if !ok {
                stale.push(rel);
            }
            continue;
        }
        let pngs = entries(&kind)
            .into_iter()
            .map(|(size, tile)| encode_png(size, &raster(&tree, size, tile)?))
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = match kind {
            Raster::Png(_) => pngs.into_iter().next().unwrap_or_default(),
            Raster::Icns => icns(&pngs),
        };
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
    fn a_written_icns_reads_back_entry_by_entry() {
        let file = icns(&[b"one".to_vec(), b"second".to_vec()]);
        let back = icns_entries(&file).expect("icns parses");
        assert_eq!(back, vec![&b"one"[..], &b"second"[..]]);
    }
}
