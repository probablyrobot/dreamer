//! What an `image` node may show: a PNG or JPEG file of at most
//! [`MAX_BYTES`], from the app's package. The app host reads only a file's
//! header ([`sniff`]) for its size; the desktop decodes it, off its UI
//! thread.

/// The largest image file shown.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The largest width or height, in pixels.
pub const MAX_SIDE: u32 = 8192;
/// How much of a file [`sniff`] needs at most: a JPEG's size is in a
/// segment after its metadata.
pub const HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

/// The format and size (width, height) of the PNG or JPEG that starts
/// with `head`; `None` for anything else.
pub fn sniff(head: &[u8]) -> Option<(Format, u32, u32)> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        // The first chunk is IHDR: length, type, width, height.
        if head.get(12..16)? != b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes(head.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(head.get(20..24)?.try_into().ok()?);
        return Some((Format::Png, w, h));
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        // Segments until a start of frame, which holds the size.
        let mut i = 2;
        loop {
            while *head.get(i)? == 0xFF && *head.get(i + 1)? == 0xFF {
                i += 1;
            }
            if *head.get(i)? != 0xFF {
                return None;
            }
            let marker = *head.get(i + 1)?;
            let len = usize::from(u16::from_be_bytes([*head.get(i + 2)?, *head.get(i + 3)?]));
            let sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if sof {
                let h = u16::from_be_bytes([*head.get(i + 5)?, *head.get(i + 6)?]);
                let w = u16::from_be_bytes([*head.get(i + 7)?, *head.get(i + 8)?]);
                return Some((Format::Jpeg, u32::from(w), u32::from(h)));
            }
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

/// Why a path given as an image's `src` is not a plain path inside the
/// package: absolute, empty, or with a `..`.
pub fn src_problem(src: &str) -> Option<&'static str> {
    let p = std::path::Path::new(src);
    if src.is_empty() {
        return Some("an image needs `src`, a file in the app's package");
    }
    if p.is_absolute() || src.starts_with('~') {
        return Some("an image's `src` is a path in the app's package, not an absolute path");
    }
    if p.components().any(|c| {
        !matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Some("an image's `src` may not leave the app's package (`..`)");
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg")) {
        return Some("an image is a .png, .jpg, or .jpeg file");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_and_jpeg_sizes_come_from_their_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        assert_eq!(sniff(&png), Some((Format::Png, 640, 480)));
        // SOI, an APP0 segment, then SOF0 with height 20 and width 30.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 17, 8, 0, 20, 0, 30, 3,
        ];
        assert_eq!(sniff(&jpeg), Some((Format::Jpeg, 30, 20)));
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(sniff(&png[..20]), None);
    }

    #[test]
    fn a_src_stays_in_the_package() {
        assert_eq!(src_problem("img/logo.png"), None);
        assert_eq!(src_problem("./a.JPG"), None);
        assert!(src_problem("../secret.png").is_some());
        assert!(src_problem("img/../../x.png").is_some());
        assert!(src_problem("/etc/x.png").is_some());
        assert!(src_problem("~/x.png").is_some());
        assert!(src_problem("notes.txt").is_some());
        assert!(src_problem("").is_some());
    }
}
