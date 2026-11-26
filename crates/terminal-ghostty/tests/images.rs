//! Kitty images in frames: placements from the public render
//! state, owned pixels, inline transport only, and the same result after a
//! snapshot restore.

use terminal_ghostty::core::{GridSize, ImageLayer, ImagePlacement};
use terminal_ghostty::{Engine, EngineConfig};

const SIZE: GridSize = GridSize { cols: 20, rows: 6 };
const CELL_PX: (u32, u32) = terminal_ghostty::core::family::CANONICAL_CELL_PX;

fn engine() -> Engine {
    let mut e = Engine::new(EngineConfig::new(SIZE)).expect("engine");
    e.resize(SIZE, CELL_PX).expect("cell size");
    e
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An APC Kitty graphics command with a base64 payload.
fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{control};{}\x1b\\", base64(payload)).into_bytes()
}

/// `w` x `h` RGBA pixels, all `rgba`.
fn solid(w: usize, h: usize, rgba: [u8; 4]) -> Vec<u8> {
    rgba.repeat(w * h)
}

fn placements(e: &mut Engine) -> Vec<ImagePlacement> {
    e.frame().expect("frame").images.to_vec()
}

#[test]
fn a_placed_image_carries_its_pixels_position_and_size() {
    let mut e = engine();
    e.feed(b"\r\n  ").expect("move to row 1, column 2");
    // Transmit and place a 4x2 red image (RGBA) with id 1, q=2 for quiet.
    e.feed(&kitty(
        "a=T,f=32,s=4,v=2,i=1,q=2",
        &solid(4, 2, [255, 0, 0, 255]),
    ))
    .expect("image");
    let p = placements(&mut e);
    assert_eq!(p.len(), 1, "{p:?}");
    let p = &p[0];
    assert_eq!((p.col, p.row), (2, 1));
    assert_eq!((p.image.id, p.image.width, p.image.height), (1, 4, 2));
    assert_eq!(&*p.image.rgba, &solid(4, 2, [255, 0, 0, 255])[..]);
    assert_eq!((p.width, p.height), (4, 2), "drawn at its own size");
    assert_eq!(
        (p.source_x, p.source_y, p.source_width, p.source_height),
        (0, 0, 4, 2)
    );
    assert_eq!(p.layer, ImageLayer::AboveText);

    // An unchanged frame shares the placement list and the pixels.
    let a = e.frame().expect("frame");
    e.feed(b"x").expect("text");
    let b = e.frame().expect("frame");
    assert!(std::sync::Arc::ptr_eq(&a.images, &b.images));
}

#[test]
fn rgb_and_gray_images_become_rgba() {
    let mut e = engine();
    e.feed(&kitty("a=T,f=24,s=1,v=1,i=2,q=2", &[10, 20, 30]))
        .expect("rgb");
    let p = placements(&mut e);
    assert_eq!(&*p[0].image.rgba, &[10, 20, 30, 255]);
}

#[test]
fn crop_z_order_delete_and_replace() {
    let mut e = engine();
    // Image 1 transmitted once, placed twice: cropped below text, and
    // whole above it.
    e.feed(&kitty(
        "a=t,f=32,s=4,v=2,i=1,q=2",
        &solid(4, 2, [0, 255, 0, 255]),
    ))
    .expect("transmit");
    e.feed(b"\x1b_Ga=p,i=1,p=1,x=1,y=0,w=2,h=1,z=-1,q=2\x1b\\")
        .expect("cropped placement");
    e.feed(b"\x1b_Ga=p,i=1,p=2,z=3,q=2\x1b\\")
        .expect("whole placement");
    let p = placements(&mut e);
    assert_eq!(p.len(), 2, "{p:?}");
    assert_eq!((p[0].layer, p[0].z), (ImageLayer::BelowText, -1));
    assert_eq!(
        (
            p[0].source_x,
            p[0].source_y,
            p[0].source_width,
            p[0].source_height
        ),
        (1, 0, 2, 1)
    );
    assert_eq!((p[1].layer, p[1].z), (ImageLayer::AboveText, 3));
    assert!(
        std::sync::Arc::ptr_eq(&p[0].image, &p[1].image),
        "one copy of the pixels for both placements"
    );

    // Replacing the image's data gives a new generation and new pixels.
    let old = p[0].image.generation;
    e.feed(&kitty(
        "a=t,f=32,s=4,v=2,i=1,q=2",
        &solid(4, 2, [0, 0, 255, 255]),
    ))
    .expect("replace");
    e.feed(b"\x1b_Ga=p,i=1,p=2,q=2\x1b\\").expect("place again");
    let p = placements(&mut e);
    let replaced = p
        .iter()
        .find(|x| x.image.generation != old)
        .expect("new generation");
    assert_eq!(&replaced.image.rgba[..4], &[0, 0, 255, 255]);

    // Deleting the image removes every placement.
    e.feed(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\").expect("delete");
    assert!(placements(&mut e).is_empty());
}

/// Unicode placeholder cells for a virtual placement: U+10EEEE with
/// diacritics for the image row and column, the image id in the
/// foreground color. A cell without diacritics continues its left
/// neighbour's row at the next column.
fn placeholder_row(image: u8, row_diacritic: char, cells: usize) -> Vec<u8> {
    let mut s = format!("\x1b[38;5;{image}m\u{10EEEE}{row_diacritic}\u{0305}");
    for _ in 1..cells {
        s.push('\u{10EEEE}');
    }
    s.push_str("\x1b[39m");
    s.into_bytes()
}

#[test]
fn a_virtual_placement_is_drawn_where_its_placeholder_cells_are() {
    // Unicode placeholder diacritics for rows 0 and 1.
    const ROW0: char = '\u{0305}';
    const ROW1: char = '\u{030D}';
    let mut e = engine();
    // Image 7, 20x40 pixels: two by two canonical cells.
    e.feed(&kitty(
        "a=t,f=32,s=20,v=40,i=7,q=2",
        &solid(20, 40, [9, 8, 7, 255]),
    ))
    .expect("transmit");
    e.feed(b"\x1b_Ga=p,U=1,i=7,c=2,r=2,q=2\x1b\\")
        .expect("virtual placement");
    assert!(
        placements(&mut e).is_empty(),
        "a virtual placement shows nothing until its placeholders are written"
    );

    // The placeholders at row 1, column 3: each row of cells shows its
    // row of the image.
    e.feed(b"\x1b[2;4H").expect("move");
    e.feed(&placeholder_row(7, ROW0, 2)).expect("row 0");
    e.feed(b"\x1b[3;4H").expect("move");
    e.feed(&placeholder_row(7, ROW1, 2)).expect("row 1");
    let p = placements(&mut e);
    let shown = |p: &[ImagePlacement]| {
        p.iter()
            .map(|x| {
                assert_eq!((x.image.id, x.image.width, x.image.height), (7, 20, 40));
                (x.col, x.row, x.width, x.height, x.source_y, x.source_height)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        shown(&p),
        [(3, 1, 20, 20, 0, 20), (3, 2, 20, 20, 20, 20)],
        "{p:?}"
    );
    assert!(p.iter().all(|x| (x.source_x, x.source_width) == (0, 20)));
    assert!(p.iter().all(|x| x.layer == ImageLayer::BelowText), "{p:?}");

    // The placeholders are text: the image scrolls with them. Two lines
    // up, the top row of cells is in history and the bottom row is first.
    e.feed(b"\x1b[6;1H\n\n").expect("scroll two lines");
    assert_eq!(shown(&placements(&mut e)), [(3, 0, 20, 20, 20, 20)]);

    // Erasing the placeholder text removes the image from the screen;
    // the placement itself stays for new placeholders.
    e.feed(b"\x1b[H\x1b[2J").expect("erase");
    assert!(placements(&mut e).is_empty());
    e.feed(&placeholder_row(7, ROW1, 2))
        .expect("bottom row only");
    assert_eq!(shown(&placements(&mut e)), [(0, 0, 20, 20, 20, 20)]);
}

#[test]
fn file_and_shared_memory_transmissions_are_refused() {
    let mut e = engine();
    let path = std::env::temp_dir().join(format!("dreamer-kitty-{}.rgba", std::process::id()));
    std::fs::write(&path, solid(1, 1, [1, 2, 3, 255])).expect("file");
    let name = path.to_string_lossy().into_owned();
    e.feed(&kitty("a=T,t=f,f=32,s=1,v=1,i=5,q=2", name.as_bytes()))
        .expect("file medium");
    e.feed(&kitty("a=T,t=t,f=32,s=1,v=1,i=6,q=2", name.as_bytes()))
        .expect("temp-file medium");
    e.feed(&kitty("a=T,t=s,f=32,s=1,v=1,i=7,q=2", b"/dreamer-shm"))
        .expect("shared-memory medium");
    let _ = std::fs::remove_file(&path);
    assert!(
        placements(&mut e).is_empty(),
        "a non-inline image was loaded"
    );
}

#[test]
fn a_restored_snapshot_shows_the_same_images() {
    let mut config = EngineConfig::new(SIZE);
    config.continuation_max_bytes = Some(64 << 10);
    let mut live = Engine::new(config.clone()).expect("engine");
    live.resize(SIZE, CELL_PX).expect("cell size");
    live.feed(b"\r\n\r\n ").expect("move");
    live.feed(&kitty(
        "a=T,f=32,s=3,v=2,i=9,z=2,q=2",
        &solid(3, 2, [7, 8, 9, 255]),
    ))
    .expect("image");
    let want = placements(&mut live);
    assert_eq!(want.len(), 1);
    let mut restored =
        Engine::from_snapshot(&live.snapshot().expect("snapshot"), &config).expect("restore");
    // The engine instance and image generations are local to the engine,
    // and checkpoint equality excludes them (spec: "excluding ...
    // process-local generations"); everything else must match.
    let got = placements(&mut restored);
    assert_ne!(
        got[0].image.engine, want[0].image.engine,
        "a replacement engine must not share texture-cache keys with the old one"
    );
    let semantic = |p: Vec<ImagePlacement>| {
        p.into_iter()
            .map(|mut x| {
                x.image = std::sync::Arc::new(terminal_ghostty::core::ImagePixels {
                    engine: 0,
                    id: x.image.id,
                    generation: 0,
                    width: x.image.width,
                    height: x.image.height,
                    rgba: x.image.rgba.clone(),
                });
                x
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        semantic(got),
        semantic(want),
        "placements differ after restore"
    );
}

mod png_images {
    use super::*;

    fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("header");
        writer.write_image_data(rgba).expect("data");
        writer.finish().expect("finish");
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    /// A PNG signature and IHDR claiming `w` x `h`, then an empty IDAT:
    /// the header parses, and the pixel data is missing.
    fn header_only(w: u32, h: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(&ihdr);
        out.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"IDAT");
        out.extend_from_slice(&crc32(b"IDAT").to_be_bytes());
        out
    }

    #[test]
    fn a_png_image_decodes_to_its_pixels() {
        let mut e = engine();
        let pixels: Vec<u8> = (0..3 * 2)
            .flat_map(|i| [i as u8 * 40, 7, 200, 255])
            .collect();
        e.feed(&kitty("a=T,f=100,i=3,q=2", &encode_png(3, 2, &pixels)))
            .expect("png");
        let p = placements(&mut e);
        assert_eq!(p.len(), 1, "the PNG was not decoded");
        assert_eq!((p[0].image.width, p[0].image.height), (3, 2));
        assert_eq!(&*p[0].image.rgba, &pixels[..]);
    }

    #[test]
    fn malformed_truncated_and_oversized_pngs_are_rejected() {
        let mut e = engine();
        let good = encode_png(4, 4, &solid(4, 4, [1, 2, 3, 255]));
        e.feed(&kitty("a=T,f=100,i=4,q=2", b"this is not a png"))
            .expect("garbage");
        e.feed(&kitty("a=T,f=100,i=5,q=2", &good[..good.len() / 2]))
            .expect("truncated");
        e.feed(&kitty("a=T,f=100,i=6,q=2", &header_only(20_000, 20_000)))
            .expect("oversized");
        assert!(placements(&mut e).is_empty(), "a bad PNG produced an image");
        // Refused by size before any pixel buffer is allocated.
        let err = terminal_ghostty::png_decode::decode(&header_only(20_000, 20_000))
            .expect_err("oversized");
        assert!(err.contains("size limit"), "{err}");
        // The engine still works afterwards.
        e.feed(&kitty("a=T,f=100,i=7,q=2", &good)).expect("good");
        assert_eq!(placements(&mut e).len(), 1);
    }
}
