//! Screen capture: one monitor's pixels, and where on the virtual desktop they came from.
//!
//! The capture is a BitBlt out of the screen DC, which in a DPI-aware process covers the whole
//! virtual desktop and so accepts the negative source coordinates of a monitor sitting above or
//! left of the primary.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, SRCCOPY,
};

use crate::display::{ensure_dpi_aware, monitors, Monitor};

/// One monitor's pixels, with where they came from.
///
/// `origin` is the monitor's top-left on the virtual desktop and it travels with the pixels.
/// Everything downstream converts between desktop coordinates and capture pixels with it, and
/// dropping it is the single most expensive bug in this project's history: a window on the monitor
/// above the primary reports y near -1440 while its capture starts at row 0, so without the origin
/// every box lands off the image and every click misses by a monitor.
#[derive(Debug, Clone)]
pub struct Shot {
    pub width: u32,
    pub height: u32,
    /// Row-major, top row first, four bytes per pixel: blue, green, red, alpha.
    pub bgra: Vec<u8>,
    pub origin: (i32, i32),
}

impl Shot {
    /// The byte offset of a pixel, or None when it is outside the capture.
    pub fn offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(((y as usize * self.width as usize) + x as usize) * 4)
    }

    /// A desktop point as a pixel of this capture, by taking the origin off. Signed in, unsigned
    /// out, and None when the point is not on this monitor at all.
    pub fn to_pixel(&self, x: i32, y: i32) -> Option<(u32, u32)> {
        let (dx, dy) = (x - self.origin.0, y - self.origin.1);
        if dx < 0 || dy < 0 || dx as u32 >= self.width || dy as u32 >= self.height {
            return None;
        }
        Some((dx as u32, dy as u32))
    }

    /// A pixel of this capture back as a desktop point, which may be negative.
    pub fn to_desktop(&self, x: u32, y: u32) -> (i32, i32) {
        (self.origin.0 + x as i32, self.origin.1 + y as i32)
    }
}

/// BitBlt one rectangle of the virtual desktop. `left` and `top` may be negative.
fn capture_rect(left: i32, top: i32, width: i32, height: i32) -> Option<Shot> {
    ensure_dpi_aware();
    if width <= 0 || height <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(HWND::default());
        if screen.is_invalid() {
            return None;
        }
        let shot = (|| {
            let memory = CreateCompatibleDC(screen);
            if memory.is_invalid() {
                return None;
            }
            // A negative biHeight asks for a top-down DIB, so row 0 is the top row and nothing
            // downstream has to flip the image.
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap: HBITMAP =
                match CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(bitmap) if !bitmap.is_invalid() && !bits.is_null() => bitmap,
                    _ => {
                        let _ = DeleteDC(memory);
                        return None;
                    }
                };
            let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
            let copied = BitBlt(memory, 0, 0, width, height, screen, left, top, SRCCOPY).is_ok();
            let shot = if copied {
                let len = width as usize * height as usize * 4;
                let mut bgra = vec![0u8; len];
                std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), len);
                Some(Shot {
                    width: width as u32,
                    height: height as u32,
                    bgra,
                    origin: (left, top),
                })
            } else {
                None
            };
            if !previous.is_invalid() {
                SelectObject(memory, previous);
            }
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            shot
        })();
        ReleaseDC(HWND::default(), screen);
        shot
    }
}

/// One monitor's pixels, tagged with that monitor's top-left on the virtual desktop.
pub fn capture_monitor(m: &Monitor) -> Option<Shot> {
    capture_rect(m.left, m.top, m.width(), m.height())
}

/// The primary display's pixels. Its origin is usually (0, 0), but it is read off the monitor
/// rather than assumed, because a desktop can be arranged so the primary is not at the origin.
pub fn capture_primary() -> Option<Shot> {
    let list = monitors();
    let primary = list.iter().find(|m| m.primary).or_else(|| list.first())?;
    capture_monitor(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(width: u32, height: u32, origin: (i32, i32)) -> Shot {
        Shot {
            width,
            height,
            bgra: vec![0u8; (width * height * 4) as usize],
            origin,
        }
    }

    #[test]
    fn a_pixel_offset_stays_inside_the_buffer() {
        let s = shot(4, 3, (0, 0));
        assert_eq!(s.offset(0, 0), Some(0));
        assert_eq!(s.offset(3, 2), Some((2 * 4 + 3) * 4));
        assert_eq!(s.offset(4, 0), None);
        assert_eq!(s.offset(0, 3), None);
        assert!(s.offset(3, 2).unwrap() + 4 <= s.bgra.len());
    }

    #[test]
    fn the_origin_converts_a_negative_desktop_point_into_a_pixel() {
        // The monitor above the primary: its top row is desktop y = -1440.
        let s = shot(2560, 1440, (1, -1440));
        assert_eq!(s.to_pixel(1, -1440), Some((0, 0)));
        assert_eq!(s.to_pixel(1281, -740), Some((1280, 700)));
        assert_eq!(s.to_desktop(0, 0), (1, -1440));
        assert_eq!(s.to_desktop(1280, 700), (1281, -740));
    }

    #[test]
    fn a_point_on_another_monitor_is_not_a_pixel_of_this_capture() {
        let s = shot(2560, 1440, (1, -1440));
        assert_eq!(s.to_pixel(100, 100), None); // on the primary, below this monitor
        assert_eq!(s.to_pixel(0, -1440), None); // one column left of it
        assert_eq!(s.to_pixel(2561, -700), None); // one column past its right edge
    }

    #[test]
    fn round_tripping_a_desktop_point_through_the_origin_is_identity() {
        let s = shot(1920, 1080, (2561, -496));
        for point in [(2561, -496), (3500, 100), (4480, 583)] {
            let (x, y) = s.to_pixel(point.0, point.1).expect("on this monitor");
            assert_eq!(s.to_desktop(x, y), point);
        }
    }

    #[test]
    fn an_empty_rectangle_captures_nothing_instead_of_allocating() {
        assert!(capture_rect(0, 0, 0, 10).is_none());
        assert!(capture_rect(0, 0, 10, -5).is_none());
    }
}
