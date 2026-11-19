//! macOS: color spaces and conversions between them, through Core
//! Graphics, for a page's capture drawn by the toolkit.
//!
//! Unlike [`crate::mac`], these may run on any thread: a color space is
//! immutable, and a bitmap context here is used by one thread only.

use std::ffi::c_void;

type CFTypeRef = *const c_void;
type CFDataRef = *const c_void;
type CFStringRef = *const c_void;
type CGColorSpaceRef = *mut c_void;
type CGDataProviderRef = *mut c_void;
type CGImageRef = *mut c_void;
type CGContextRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFDataRef;
    fn CFDataGetLength(data: CFDataRef) -> isize;
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CFStringRef;
    static kCGColorSpaceDisplayP3: CFStringRef;
    fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    fn CGColorSpaceCreateWithICCData(data: CFTypeRef) -> CGColorSpaceRef;
    fn CGColorSpaceCopyICCData(space: CGColorSpaceRef) -> CFDataRef;
    fn CGColorSpaceIsWideGamutRGB(space: CGColorSpaceRef) -> bool;
    fn CGColorSpaceGetNumberOfComponents(space: CGColorSpaceRef) -> usize;
    fn CGColorSpaceRetain(space: CGColorSpaceRef) -> CGColorSpaceRef;
    fn CGColorSpaceRelease(space: CGColorSpaceRef);
    fn CGDataProviderCreateWithCFData(data: CFDataRef) -> CGDataProviderRef;
    fn CGDataProviderRelease(provider: CGDataProviderRef);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
        provider: CGDataProviderRef,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> CGImageRef;
    fn CGImageRelease(image: CGImageRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    fn CGContextSetBlendMode(context: CGContextRef, mode: i32);
    fn CGContextSetInterpolationQuality(context: CGContextRef, quality: i32);
    fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
    fn CGContextRelease(context: CGContextRef);
}

/// `kCGImageAlphaNoneSkipLast`: RGBX, the fourth byte ignored.
const RGBX: u32 = 5;
/// `kCGBlendModeCopy`.
const BLEND_COPY: i32 = 17;
/// `kCGInterpolationNone`.
const INTERPOLATION_NONE: i32 = 1;
/// `kCGRenderingIntentDefault`.
const INTENT_DEFAULT: i32 = 0;

/// A Core Graphics RGB color space, retained.
pub struct ColorSpace(CGColorSpaceRef);

// SAFETY: a `CGColorSpace` is immutable, and Core Graphics retains and
// releases it atomically: it may be shared and dropped on any thread.
unsafe impl Send for ColorSpace {}
// SAFETY: as above.
unsafe impl Sync for ColorSpace {}

impl ColorSpace {
    /// Take ownership of a +1 reference: `None` for null, or for a space
    /// that is not RGB.
    fn owned(space: CGColorSpaceRef) -> Option<Self> {
        if space.is_null() {
            return None;
        }
        let s = Self(space);
        // SAFETY: `space` is a live color space.
        (unsafe { CGColorSpaceGetNumberOfComponents(space) } == 3).then_some(s)
    }

    /// sRGB (IEC 61966-2.1).
    pub fn srgb() -> Option<Self> {
        // SAFETY: a constant Core Graphics exports; the result is +1.
        Self::owned(unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) })
    }

    /// Display P3.
    pub fn display_p3() -> Option<Self> {
        // SAFETY: as in `srgb`.
        Self::owned(unsafe { CGColorSpaceCreateWithName(kCGColorSpaceDisplayP3) })
    }

    /// The RGB space an ICC profile describes, if it describes one.
    pub fn from_icc(profile: &[u8]) -> Option<Self> {
        let len = isize::try_from(profile.len()).ok()?;
        // SAFETY: `profile` is valid for `len` bytes; CFDataCreate copies
        // them. The data is released once the space holds what it needs.
        unsafe {
            let data = CFDataCreate(std::ptr::null(), profile.as_ptr(), len);
            if data.is_null() {
                return None;
            }
            let space = CGColorSpaceCreateWithICCData(data);
            CFRelease(data);
            Self::owned(space)
        }
    }

    /// Retain a space the caller borrows (a `CGColorSpaceRef` from AppKit).
    ///
    /// # Safety
    ///
    /// `space` is null or a live `CGColorSpaceRef`.
    pub unsafe fn retained(space: *mut c_void) -> Option<Self> {
        if space.is_null() {
            return None;
        }
        // SAFETY: caller guarantees a live color space; retained here,
        // released on drop.
        Self::owned(unsafe { CGColorSpaceRetain(space) })
    }

    /// The space's ICC profile, if it has one.
    pub fn icc(&self) -> Option<Vec<u8>> {
        // SAFETY: `self.0` is live; the copy is +1 and released here.
        unsafe {
            let data = CGColorSpaceCopyICCData(self.0);
            if data.is_null() {
                return None;
            }
            let len = usize::try_from(CFDataGetLength(data)).unwrap_or(0);
            let ptr = CFDataGetBytePtr(data);
            let out = if ptr.is_null() || len == 0 {
                None
            } else {
                Some(std::slice::from_raw_parts(ptr, len).to_vec())
            };
            CFRelease(data);
            out
        }
    }

    /// Whether the space holds colors beyond sRGB (Display P3, say).
    pub fn is_wide_gamut(&self) -> bool {
        // SAFETY: `self.0` is live.
        unsafe { CGColorSpaceIsWideGamutRGB(self.0) }
    }

    /// Whether this is sRGB: its profile is sRGB's own.
    pub fn is_srgb(&self) -> bool {
        match (self.icc(), Self::srgb().and_then(|s| s.icc())) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

impl Drop for ColorSpace {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a +1 reference this value owns.
        unsafe { CGColorSpaceRelease(self.0) }
    }
}

impl std::fmt::Debug for ColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorSpace")
            .field("wide_gamut", &self.is_wide_gamut())
            .finish()
    }
}

/// Convert `rgba` (`width` × `height` pixels, 8 bits per channel, rows top
/// to bottom with no padding) from `from` to `to`, in place, as Core
/// Graphics matches colors (relative colorimetric; colors outside `to`
/// clipped to it). Alpha is kept as it is: the colors are converted as if
/// opaque.
pub fn convert_rgba(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    from: &ColorSpace,
    to: &ColorSpace,
) -> Result<(), String> {
    let row = width.checked_mul(4).ok_or("an image too wide")?;
    if width == 0 || height == 0 || row.checked_mul(height) != Some(rgba.len()) {
        return Err(format!(
            "{} bytes are not a {width}x{height} RGBA image",
            rgba.len()
        ));
    }
    let len = isize::try_from(rgba.len()).map_err(|_| "an image too large")?;
    let alpha: Vec<u8> = rgba.iter().skip(3).step_by(4).copied().collect();
    // SAFETY: every pointer passed is live for the call it is passed to:
    // `rgba` is borrowed mutably for the whole function and holds
    // `row * height` bytes, the context's size; the source is a copy
    // (CFData), so the image never reads the buffer it is drawn into.
    // Every object created here is released before return.
    unsafe {
        let data = CFDataCreate(std::ptr::null(), rgba.as_ptr(), len);
        if data.is_null() {
            return Err("no memory for the image".into());
        }
        let provider = CGDataProviderCreateWithCFData(data);
        CFRelease(data);
        if provider.is_null() {
            return Err("Core Graphics could not read the image".into());
        }
        let image = CGImageCreate(
            width,
            height,
            8,
            32,
            row,
            from.0,
            RGBX,
            provider,
            std::ptr::null(),
            false,
            INTENT_DEFAULT,
        );
        CGDataProviderRelease(provider);
        if image.is_null() {
            return Err("Core Graphics could not make the image".into());
        }
        let context =
            CGBitmapContextCreate(rgba.as_mut_ptr().cast(), width, height, 8, row, to.0, RGBX);
        if context.is_null() {
            CGImageRelease(image);
            return Err("Core Graphics could not make a bitmap to convert into".into());
        }
        CGContextSetBlendMode(context, BLEND_COPY);
        CGContextSetInterpolationQuality(context, INTERPOLATION_NONE);
        CGContextDrawImage(
            context,
            CGRect {
                x: 0.0,
                y: 0.0,
                width: width as f64,
                height: height as f64,
            },
            image,
        );
        CGContextRelease(context);
        CGImageRelease(image);
    }
    for (px, a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(alpha) {
        px[3] = a;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_p3_red_clips_to_srgb_red_and_srgb_red_is_inside_p3() {
        let (srgb, p3) = (
            ColorSpace::srgb().expect("sRGB"),
            ColorSpace::display_p3().expect("P3"),
        );
        assert!(srgb.is_srgb() && !p3.is_srgb());
        assert!(p3.is_wide_gamut() && !srgb.is_wide_gamut());
        // P3's red is beyond sRGB: clipped to (about) sRGB's red.
        let mut px = [255, 0, 0, 7, 40, 90, 200, 255];
        convert_rgba(&mut px, 2, 1, &p3, &srgb).expect("converted");
        assert!(px[0] > 245 && px[1] < 12 && px[2] < 12, "{px:?}");
        assert_eq!((px[3], px[7]), (7, 255), "alpha is kept");
        // sRGB's red is inside P3, less saturated there.
        let mut px = [255, 0, 0, 255];
        convert_rgba(&mut px, 1, 1, &srgb, &p3).expect("converted");
        assert!(px[0] < 245 && px[1] > 30, "{px:?}");
        // And a space read back from its profile is the same space.
        let again = ColorSpace::from_icc(&p3.icc().expect("profile")).expect("P3 again");
        assert!(again.is_wide_gamut());
        let mut px = [10, 120, 230, 255];
        convert_rgba(&mut px, 1, 1, &again, &p3).expect("converted");
        assert!(
            px.iter()
                .zip([10, 120, 230])
                .all(|(a, b)| a.abs_diff(b) <= 1),
            "{px:?}"
        );
    }

    #[test]
    fn a_buffer_of_the_wrong_size_is_refused() {
        let s = ColorSpace::srgb().expect("sRGB");
        assert!(convert_rgba(&mut [0; 7], 1, 2, &s, &s).is_err());
        assert!(ColorSpace::from_icc(b"not a profile").is_none());
    }
}
