//! PNG decoding for Kitty images: the decode callback Ghostty
//! calls when a program sends `f=100` data.
//!
//! - Registered once per process, before the library is used on any
//!   thread ([`register`] runs from every entry point into the library).
//! - Dimensions are checked before anything is allocated for the pixels.
//! - Output is allocated with the allocator Ghostty passes in and handed
//!   over only on success.
//! - A panic never unwinds into C: the image is rejected instead.

use std::io::Cursor;
use std::sync::Once;

use ghostty_sys::*;

/// Largest PNG accepted, in pixels (the same limit as image extraction).
pub const MAX_PNG_PIXELS: u64 = 64 << 20;

/// Most memory the decoder may use while decoding one PNG.
const DECODE_LIMIT_BYTES: usize = 320 << 20;

static REGISTER: Once = Once::new();

/// Install the PNG decoder in Ghostty's process-global system interface.
/// Runs once; every path that creates an engine or queries the library
/// calls it first, so it happens before any other use of the library.
pub(crate) fn register() {
    REGISTER.call_once(|| {
        // SAFETY: the option takes the function pointer by value (cast to
        // a data pointer, as the C API documents); `Once` ensures no other
        // thread is inside the library yet.
        let rc = unsafe {
            ghostty_sys_set(
                GhosttySysOption::GHOSTTY_SYS_OPT_DECODE_PNG,
                decode_png as *const std::ffi::c_void,
            )
        };
        debug_assert_eq!(
            rc,
            GhosttyResult::GHOSTTY_SUCCESS,
            "PNG decoder not installed"
        );
    });
}

/// Decoded straight RGBA pixels, or why the PNG was rejected.
pub fn decode(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(data),
        png::Limits {
            bytes: DECODE_LIMIT_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h) = reader.info().size();
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0 || pixels > MAX_PNG_PIXELS {
        return Err(format!("{w}x{h} is outside the image size limit"));
    }
    let size = reader.output_buffer_size().ok_or("image too large")?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let n = pixels as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[g, a]| [*g, *g, *g, *a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => return Err(format!("unexpected color type {other:?} after expansion")),
    };
    if rgba.len() != n * 4 {
        return Err("decoded size does not match the image size".into());
    }
    Ok((w, h, rgba))
}

/// The callback Ghostty calls; see `GhosttySysDecodePngFn`.
unsafe extern "C" fn decode_png(
    _userdata: *mut std::ffi::c_void,
    allocator: *const GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut GhosttySysImage,
) -> bool {
    std::panic::catch_unwind(|| {
        if data.is_null() || out.is_null() {
            return false;
        }
        // SAFETY: Ghostty guarantees `data_len` readable bytes for the
        // duration of the call.
        let input = unsafe { std::slice::from_raw_parts(data, data_len) };
        let Ok((w, h, rgba)) = decode(input) else {
            return false;
        };
        // SAFETY: allocated with the allocator Ghostty passed, at exactly
        // the length recorded in `out`; a NULL return is a failure.
        unsafe {
            let ptr = ghostty_alloc(allocator, rgba.len());
            if ptr.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr, rgba.len());
            *out = GhosttySysImage {
                width: w,
                height: h,
                data: ptr,
                data_len: rgba.len(),
            };
        }
        true
    })
    .unwrap_or(false)
}
