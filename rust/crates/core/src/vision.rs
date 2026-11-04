//! The screen as an image TypeSafe can look at, sized to its limits.
//!
//! TypeSafe rejects, rather than resizes, an image whose width or height is outside 280..1568 px
//! or whose pixel count is outside 78,400..2,458,624. So the capture is fitted here, and a capture
//! that cannot be fitted (an extreme strip) yields no image at all: the caller then sends the
//! text-only request, which is always valid.

use api::typesafe::Asset;
use base64::Engine;

pub const MIN_SIDE: u32 = 280;
pub const MAX_SIDE: u32 = 1568;
pub const MIN_PIXELS: u64 = 78_400;
pub const MAX_PIXELS: u64 = 2_458_624;
/// The long side a capture is brought to: enough to read a screen, cheap to send and to read.
pub const TARGET_LONG_SIDE: u32 = 1024;
/// The key the state names the image by.
pub const SHOT_KEY: &str = "shot";

/// `POINTER_JEV_VISION`: whether the screen image goes to TypeSafe with a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionMode {
    /// Only when the screen has few readable items (the default).
    Auto,
    On,
    Off,
}

/// Fewer readable (OCR or accessibility) items than this makes a screen sparse. Measured: the
/// image tripled decision latency (1.2 s against 0.42 s) with no better decisions on a screen
/// that reads well as text, so it is kept for the screens that do not.
pub const SPARSE_ITEMS: usize = 8;

pub fn vision_mode(value: Option<&str>) -> VisionMode {
    match value.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("on" | "1" | "true" | "yes") => VisionMode::On,
        Some("off" | "0" | "false" | "no") => VisionMode::Off,
        _ => VisionMode::Auto,
    }
}

pub fn vision_current() -> VisionMode {
    vision_mode(std::env::var("POINTER_JEV_VISION").ok().as_deref())
}

/// Whether to send the image for a screen with `readable_items` items.
pub fn should_send(mode: VisionMode, readable_items: usize) -> bool {
    match mode {
        VisionMode::On => true,
        VisionMode::Off => false,
        VisionMode::Auto => readable_items < SPARSE_ITEMS,
    }
}

fn within(w: u32, h: u32) -> bool {
    let px = w as u64 * h as u64;
    (MIN_SIDE..=MAX_SIDE).contains(&w)
        && (MIN_SIDE..=MAX_SIDE).contains(&h)
        && (MIN_PIXELS..=MAX_PIXELS).contains(&px)
}

/// The size to send a `w` by `h` capture at, keeping its shape; None when no size inside the
/// limits keeps it.
pub fn fit_dims(w: u32, h: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 {
        return None;
    }
    let (wf, hf) = (w as f64, h as f64);
    let long = wf.max(hf);
    let short = wf.min(hf);
    let mut s = TARGET_LONG_SIDE as f64 / long;
    // The short side must reach the floor, the long side must stay under the ceiling.
    if short * s < MIN_SIDE as f64 {
        s = MIN_SIDE as f64 / short;
    }
    if long * s > MAX_SIDE as f64 {
        return None;
    }
    // Pixel count, with a margin for rounding.
    let px = wf * hf * s * s;
    if px < MIN_PIXELS as f64 {
        s = (MIN_PIXELS as f64 / (wf * hf)).sqrt() * 1.001;
    } else if px > MAX_PIXELS as f64 {
        s = (MAX_PIXELS as f64 / (wf * hf)).sqrt() * 0.999;
    }
    let (nw, nh) = ((wf * s).round() as u32, (hf * s).round() as u32);
    if within(nw, nh) {
        Some((nw, nh))
    } else {
        None
    }
}

/// A BGRA capture as a downscaled JPEG asset named [`SHOT_KEY`], or None when it cannot be
/// fitted or encoded. Never fails a caller: no image just means no image.
pub fn screen_asset(bgra: &[u8], width: u32, height: u32) -> Option<Asset> {
    if bgra.len() < width as usize * height as usize * 4 {
        return None;
    }
    let (nw, nh) = fit_dims(width, height)?;
    let mut rgb = image::RgbImage::new(width, height);
    for (i, px) in rgb.pixels_mut().enumerate() {
        let o = i * 4;
        *px = image::Rgb([bgra[o + 2], bgra[o + 1], bgra[o]]);
    }
    let scaled = if (nw, nh) == (width, height) {
        rgb
    } else if nw < width {
        image::imageops::thumbnail(&rgb, nw, nh)
    } else {
        image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::Triangle)
    };
    let mut jpg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 72);
    scaled.write_with_encoder(encoder).ok()?;
    Some(Asset {
        key: SHOT_KEY.to_string(),
        format: "jpg".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&jpg),
    })
}

/// A box in fractions of the image to x, y, w, h on the virtual desktop in physical pixels.
/// The display's origin is added once; `scale` is capture pixels per desktop pixel. Fractions are
/// clamped to the image and put in order.
pub fn box_to_desktop(
    (top, bottom, left, right): (f64, f64, f64, f64),
    capture: (u32, u32),
    origin: (f64, f64),
    scale: f64,
) -> (f64, f64, f64, f64) {
    let s = if scale > 0.0 { scale } else { 1.0 };
    let c = |v: f64| v.clamp(0.0, 1.0);
    let (t, b) = (c(top).min(c(bottom)), c(top).max(c(bottom)));
    let (l, r) = (c(left).min(c(right)), c(left).max(c(right)));
    let (cw, ch) = (capture.0 as f64, capture.1 as f64);
    (
        origin.0 + l * cw / s,
        origin.1 + t * ch / s,
        (r - l) * cw / s,
        (b - t) * ch / s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_auto_unless_set() {
        assert_eq!(vision_mode(None), VisionMode::Auto);
        assert_eq!(vision_mode(Some("")), VisionMode::Auto);
        assert_eq!(vision_mode(Some("garbage")), VisionMode::Auto);
        assert_eq!(vision_mode(Some(" ON ")), VisionMode::On);
        for off in ["off", "OFF", "0", "false", " no "] {
            assert_eq!(vision_mode(Some(off)), VisionMode::Off, "{off}");
        }
    }

    #[test]
    fn auto_sends_the_image_only_for_a_sparse_screen() {
        assert!(should_send(VisionMode::Auto, 0));
        assert!(should_send(VisionMode::Auto, SPARSE_ITEMS - 1));
        assert!(!should_send(VisionMode::Auto, SPARSE_ITEMS));
        assert!(!should_send(VisionMode::Auto, 400));
        assert!(should_send(VisionMode::On, 400) && !should_send(VisionMode::Off, 0));
    }

    #[test]
    fn ordinary_screens_fit_inside_every_limit() {
        for (w, h) in [(2560, 1440), (1920, 1080), (3840, 2160), (1440, 2560), (1366, 768)] {
            let (nw, nh) = fit_dims(w, h).unwrap_or_else(|| panic!("{w}x{h}"));
            assert!(within(nw, nh), "{w}x{h} -> {nw}x{nh}");
            assert!(nw.max(nh) <= TARGET_LONG_SIDE + 1);
        }
    }

    #[test]
    fn a_tiny_capture_is_scaled_up_to_the_floor() {
        let (w, h) = fit_dims(100, 60).unwrap();
        assert!(within(w, h), "{w}x{h}");
        let (w, h) = fit_dims(10, 10).unwrap();
        assert!(within(w, h), "{w}x{h}");
    }

    #[test]
    fn a_huge_capture_is_scaled_down() {
        let (w, h) = fit_dims(15360, 8640).unwrap();
        assert!(within(w, h), "{w}x{h}");
    }

    #[test]
    fn a_strip_that_cannot_fit_gives_no_image() {
        assert_eq!(fit_dims(8000, 100), None);
        assert_eq!(fit_dims(0, 100), None);
    }

    #[test]
    fn a_screen_becomes_a_small_jpeg_asset() {
        let (w, h) = (800u32, 500u32);
        let bgra = vec![200u8; (w * h * 4) as usize];
        let a = screen_asset(&bgra, w, h).unwrap();
        assert_eq!((a.key.as_str(), a.format.as_str()), ("shot", "jpg"));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&a.data_base64)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(within(img.width(), img.height()));
        assert!(screen_asset(&bgra[..10], w, h).is_none());
        assert!(screen_asset(&vec![0u8; 8000 * 100 * 4], 8000, 100).is_none());
    }

    #[test]
    fn a_box_on_a_monitor_above_the_primary_is_offset_once() {
        // The display at (1, -1440), 2560 x 1440, fractions 0.25..0.5 across, 0.5..0.75 down.
        let r = box_to_desktop((0.5, 0.75, 0.25, 0.5), (2560, 1440), (1.0, -1440.0), 1.0);
        assert_eq!(r, (1.0 + 640.0, -1440.0 + 720.0, 640.0, 360.0));
        // Swapped or out-of-range fractions are put in order and clamped.
        let r = box_to_desktop((0.75, 0.5, 1.5, 0.5), (1000, 1000), (0.0, 0.0), 1.0);
        assert_eq!(r, (500.0, 500.0, 500.0, 250.0));
        // A scaled capture.
        let r = box_to_desktop((0.0, 0.5, 0.0, 0.5), (3000, 2000), (10.0, 20.0), 1.5);
        assert_eq!(r, (10.0, 20.0, 1000.0, 666.6666666666666));
    }
}
