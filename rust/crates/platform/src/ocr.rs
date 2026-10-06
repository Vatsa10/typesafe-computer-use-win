//! Reading text off pixels with Windows.Media.Ocr.
//!
//! This is the Rust side of the Python `ocr_recognize`/`_windows_recognize` pair, and it keeps the
//! same rules, each of which the Python version learned the hard way:
//!
//! * The engine is `OcrEngine::TryCreateFromLanguage(Language("en-US"))`, falling back to
//!   `TryCreateFromUserProfileLanguages()`. A machine with no matching language pack returns
//!   nothing at all, which is a missing language pack and not a bug here, so it is reported as an
//!   empty read rather than a panic.
//! * The bitmap is `SoftwareBitmap::CreateCopyFromBuffer(buffer, Bgra8, width, height)`. There is
//!   no five-argument overload with an alpha mode in the projection, and **BGRA is the format that
//!   works** - which is exactly why the capture crate hands out BGRA.
//! * A line has no box of its own; its box is the union of its words' `BoundingRect`s.
//! * `RecognizeAsync` is an `IAsyncOperation`, so a synchronous caller blocks on `get()`.

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// The language the engine is asked for first, matching Python's `OCR_LANGUAGE` default.
pub const OCR_LANGUAGE: &str = "en-US";

/// Windows OCR reports no confidence at all, so every line it returns is scored 1.0. That is
/// honest rather than useful: the engine has already dropped what it could not read, and the
/// pipeline's only use of the number is a threshold such a line always clears. Inventing a
/// plausible-looking number here would be worse than saying nothing.
pub const WINDOWS_OCR_CONFIDENCE: f32 = 1.0;

/// One recognized line: its text, its confidence, and its box in the image's own pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    pub confidence: f32,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// Whether Windows can read this language on this machine, i.e. whether the language pack is
/// installed. `recognize` falls back to the user's profile languages, so a `false` here does not
/// mean reading is impossible - only that this particular language is not available.
pub fn available(language: &str) -> bool {
    let Ok(language) = Language::CreateLanguage(&HSTRING::from(language)) else {
        return false;
    };
    OcrEngine::IsLanguageSupported(&language).unwrap_or(false)
}

/// Read one BGRA image. `bgra` is `width * height * 4` bytes, top row first, which is what the
/// capture crate produces. Returns one entry per line, in the engine's own order.
///
/// An engine that cannot be created (no language pack at all) or a bitmap the engine refuses reads
/// as no lines; nothing here panics on a machine that simply lacks the pack.
pub fn recognize(bgra: &[u8], width: u32, height: u32) -> Vec<Line> {
    try_recognize(bgra, width, height).unwrap_or_default()
}

fn try_recognize(bgra: &[u8], width: u32, height: u32) -> windows::core::Result<Vec<Line>> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let wanted = (width as usize) * (height as usize) * 4;
    if bgra.len() < wanted {
        return Ok(Vec::new());
    }

    // TryCreateFromLanguage yields nothing when the pack is missing, so fall back the way Python
    // does before giving up.
    let engine = match Language::CreateLanguage(&HSTRING::from(OCR_LANGUAGE))
        .and_then(|language| OcrEngine::TryCreateFromLanguage(&language))
    {
        Ok(engine) => engine,
        Err(_) => OcrEngine::TryCreateFromUserProfileLanguages()?,
    };

    // DataWriter is the shortest route from a byte slice to the IBuffer CreateCopyFromBuffer wants.
    let writer = DataWriter::new()?;
    writer.WriteBytes(&bgra[..wanted])?;
    let buffer = writer.DetachBuffer()?;

    // Four arguments, and Bgra8: the five-argument alpha-mode overload is not in the projection,
    // and BGRA is the format that actually reads.
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )?;

    // RecognizeAsync is an IAsyncOperation; a sync caller blocks on get().
    let result = engine.RecognizeAsync(&bitmap)?.get()?;

    let mut lines = Vec::new();
    for line in result.Lines()? {
        let mut boxes = Vec::new();
        for word in line.Words()? {
            let rect = word.BoundingRect()?;
            boxes.push((rect.X, rect.Y, rect.Width, rect.Height));
        }
        let text = line.Text()?.to_string_lossy();
        if let Some(line) = line_from_words(&text, &boxes) {
            lines.push(line);
        }
    }
    Ok(lines)
}

/// The pure half: a line's text plus its words' `(x, y, width, height)` boxes become one `Line`
/// whose box is their union. A line with no words has no box, so it is dropped, exactly as in
/// Python. Kept separate so it can be tested without an engine.
fn line_from_words(text: &str, boxes: &[(f32, f32, f32, f32)]) -> Option<Line> {
    let first = boxes.first()?;
    let mut x1 = first.0;
    let mut y1 = first.1;
    let mut x2 = first.0 + first.2;
    let mut y2 = first.1 + first.3;
    for &(x, y, w, h) in &boxes[1..] {
        x1 = x1.min(x);
        y1 = y1.min(y);
        x2 = x2.max(x + w);
        y2 = y2.max(y + h);
    }
    Some(Line {
        text: text.to_string(),
        confidence: WINDOWS_OCR_CONFIDENCE,
        x1,
        y1,
        x2,
        y2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lines_box_is_the_union_of_its_words() {
        // Two words on one baseline, the second taller and further right.
        let line = line_from_words(
            "hello world",
            &[(10.0, 20.0, 30.0, 12.0), (45.0, 18.0, 40.0, 16.0)],
        )
        .expect("two words make a box");
        assert_eq!(line.text, "hello world");
        assert_eq!(
            (line.x1, line.y1, line.x2, line.y2),
            (10.0, 18.0, 85.0, 34.0)
        );
    }

    #[test]
    fn one_word_is_its_own_box() {
        let line = line_from_words("hi", &[(5.0, 6.0, 7.0, 8.0)]).expect("one word makes a box");
        assert_eq!((line.x1, line.y1, line.x2, line.y2), (5.0, 6.0, 12.0, 14.0));
    }

    #[test]
    fn words_out_of_order_still_union() {
        let a = line_from_words("a b", &[(0.0, 0.0, 10.0, 10.0), (50.0, 5.0, 10.0, 10.0)]).unwrap();
        let b = line_from_words("a b", &[(50.0, 5.0, 10.0, 10.0), (0.0, 0.0, 10.0, 10.0)]).unwrap();
        assert_eq!((a.x1, a.y1, a.x2, a.y2), (b.x1, b.y1, b.x2, b.y2));
    }

    #[test]
    fn a_line_with_no_words_has_no_box_and_is_dropped() {
        assert!(line_from_words("ghost", &[]).is_none());
    }

    #[test]
    fn every_line_is_scored_one_because_the_engine_reports_nothing() {
        let line = line_from_words("x", &[(0.0, 0.0, 1.0, 1.0)]).unwrap();
        assert_eq!(line.confidence, WINDOWS_OCR_CONFIDENCE);
        assert_eq!(line.confidence, 1.0);
    }

    #[test]
    fn a_degenerate_image_reads_as_nothing_rather_than_panicking() {
        assert!(recognize(&[], 0, 0).is_empty());
        assert!(recognize(&[0, 0, 0, 255], 4, 4).is_empty()); // buffer too short for the size
    }

    /// The real engine, on a bitmap drawn here: white page, black bars where glyphs would be. The
    /// engine may read nothing from synthetic bars, so this asserts only that it runs and that
    /// every line it does return is well formed.
    #[test]
    #[ignore = "needs a Windows OCR language pack"]
    fn the_real_engine_runs() {
        assert!(
            available(OCR_LANGUAGE) || available("en"),
            "no English OCR language pack on this machine"
        );
        let (width, height) = (400u32, 120u32);
        let mut bgra = vec![255u8; (width * height * 4) as usize];
        for y in 40..70 {
            for x in 30..360 {
                if (x / 14) % 3 != 0 {
                    continue;
                }
                let i = ((y * width + x) * 4) as usize;
                bgra[i] = 0;
                bgra[i + 1] = 0;
                bgra[i + 2] = 0;
            }
        }
        for line in recognize(&bgra, width, height) {
            assert!(line.x2 >= line.x1 && line.y2 >= line.y1);
            assert_eq!(line.confidence, WINDOWS_OCR_CONFIDENCE);
        }
    }
}
