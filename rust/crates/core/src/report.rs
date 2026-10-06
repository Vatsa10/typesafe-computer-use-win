//! Logging, annotated screenshots, and the human-readable payload dump.
//!
//! A port of `typesafe_computer_use_win/report.py`.
//!
//! Index labels on the annotated capture are drawn with an embedded 5x7 bitmap digit font, scaled
//! up by an integer factor. The Python asked the OS for a TrueType font and, when the path was
//! wrong (it was once a macOS path), silently fell back to PIL's unreadable bitmap default. Labels
//! here are only ever digits, so a font compiled into the binary removes that whole class of
//! failure: nothing to find, nothing to fall back from.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use crate::models::{Item, PixelBox};

pub const RULE: &str =
    "==============================================================================";

/// A sink for log lines, typically a UI feed.
pub type Echo = Box<dyn FnMut(&str) + Send>;

/// Send each line to a sink and append it to a file.
///
/// A sink that panics must not take the run down: the panic is caught and the line still reaches
/// the file. A file that cannot be written is likewise ignored, matching how a log must never be
/// the reason a run stops.
pub struct Log {
    pub path: Option<PathBuf>,
    echo: Option<Echo>,
}

impl Log {
    pub fn new(path: Option<PathBuf>, echo: Option<Echo>) -> Self {
        Self { path, echo }
    }

    /// A log that echoes to stdout, like the Python default of `print`.
    pub fn stdout(path: Option<PathBuf>) -> Self {
        Self::new(path, Some(Box::new(|s: &str| println!("{s}"))))
    }

    pub fn line(&mut self, msg: &str) {
        if let Some(echo) = self.echo.as_mut() {
            // A broken sink must not kill the loop.
            let _ = catch_unwind(AssertUnwindSafe(|| echo(msg)));
        }
        if let Some(path) = &self.path {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(format!("{msg}\n").as_bytes());
            }
        }
    }
}

/// The `n` most probable options, highest first.
pub fn top(probabilities: &BTreeMap<String, f64>, n: usize) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = probabilities.iter().map(|(k, p)| (k.clone(), *p)).collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(n);
    v
}

pub fn ax_count(items: &[Item]) -> usize {
    items.iter().filter(|it| it.from_ax()).count()
}

/// One item as the payload table shows it; the caller supplies the screen-derived pieces.
pub struct PayloadItem<'a> {
    pub item: &'a Item,
    /// Where a click would land, in virtual-desktop points (`Screen::to_points`).
    pub click: (f64, f64),
    /// `Screen::region`.
    pub region: String,
}

/// Everything `render_payload` prints, as plain values so this module does not depend on decide.
pub struct Payload<'a> {
    pub state: &'a serde_json::Value,
    pub kind_criteria: &'a serde_json::Value,
    pub item_criteria: &'a serde_json::Value,
    pub site_criteria: &'a serde_json::Value,
    /// Present only when the screen has off-screen controls.
    pub offscreen_criteria: Option<&'a serde_json::Value>,
    pub items: Vec<PayloadItem<'a>>,
    /// (role, label) of each off-screen control.
    pub offscreen: Vec<(String, String)>,
    /// `Field::record()` of the focused field, if any.
    pub field: Option<serde_json::Value>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn section(parts: &mut Vec<String>, title: &str, body: &serde_json::Value) {
    parts.extend([
        RULE.into(),
        title.into(),
        RULE.into(),
        pretty(body),
        String::new(),
    ]);
}

/// Python's `{x:g}` for the common cases: no trailing zeros.
fn fmt_g(x: f64) -> String {
    let s = format!("{x}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Python's `repr` of a str, near enough for a human dump.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c)
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Exactly what goes to TypeSafe for this screen, plus a table of every item.
pub fn render_payload(p: &Payload) -> String {
    let mut parts: Vec<String> = Vec::new();
    section(&mut parts, "STATE  (sent as `state`)", p.state);
    section(
        &mut parts,
        "QUESTION kind  (Choice criteria)",
        p.kind_criteria,
    );
    section(
        &mut parts,
        "QUESTION item  (Choice criteria)",
        p.item_criteria,
    );
    section(
        &mut parts,
        "QUESTION site  (Choice criteria)",
        p.site_criteria,
    );
    if let Some(off) = p.offscreen_criteria {
        section(&mut parts, "QUESTION offscreen  (Choice criteria)", off);
    }
    let ax = p.items.iter().filter(|pi| pi.item.from_ax()).count();
    parts.push(RULE.into());
    parts.push(format!(
        "ITEMS  ({} after merge/filter, {ax} from the accessibility tree; pixel boxes on the {}x{} capture, scale {})",
        p.items.len(),
        p.width,
        p.height,
        fmt_g(p.scale)
    ));
    parts.push(RULE.into());
    for pi in &p.items {
        let it = pi.item;
        let role = if it.role.is_empty() {
            "-"
        } else {
            it.role.as_str()
        };
        parts.push(format!(
            "[{:3}] src={:6} role={:8} conf={:.2} box=({:.0},{:.0})-({:.0},{:.0}) click_pt=({:.0},{:.0}) {:13} {}",
            it.index,
            it.source,
            role,
            it.ocr_confidence,
            it.x1,
            it.y1,
            it.x2,
            it.y2,
            pi.click.0,
            pi.click.1,
            pi.region,
            py_repr(&it.text)
        ));
    }
    if !p.offscreen.is_empty() {
        parts.extend([
            String::new(),
            RULE.into(),
            format!(
                "OFFSCREEN CONTROLS  ({} the app exposes without showing; pressed through accessibility, never clicked)",
                p.offscreen.len()
            ),
            RULE.into(),
        ]);
        for (i, (role, label)) in p.offscreen.iter().enumerate() {
            parts.push(format!("[{i:3}] role={role:22} {}", py_repr(label)));
        }
    }
    if let Some(f) = &p.field {
        parts.extend([String::new(), "FOCUSED FIELD".into(), pretty(f)]);
    }
    parts.join("\n") + "\n"
}

// ---------------------------------------------------------------------------------------------
// Annotation

pub const RED: [u8; 3] = [255, 0, 0];
pub const ORANGE: [u8; 3] = [255, 140, 0];
pub const BLUE: [u8; 3] = [0, 160, 255];
pub const GREEN: [u8; 3] = [0, 200, 0];

/// 5x7 digits, one byte per row, low five bits, most significant bit leftmost.
const DIGITS: [[u8; 7]; 10] = [
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E], // 0
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E], // 1
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F], // 2
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E], // 3
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02], // 4
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E], // 5
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E], // 6
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08], // 7
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E], // 8
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C], // 9
];

struct Canvas {
    rgba: Vec<u8>,
    w: i64,
    h: i64,
}

impl Canvas {
    fn put(&mut self, x: i64, y: i64, c: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        self.rgba[i..i + 3].copy_from_slice(&c);
        self.rgba[i + 3] = 255;
    }

    fn fill(&mut self, x1: i64, y1: i64, x2: i64, y2: i64, c: [u8; 3]) {
        for y in y1.max(0)..=y2.min(self.h - 1) {
            for x in x1.max(0)..=x2.min(self.w - 1) {
                self.put(x, y, c);
            }
        }
    }

    /// An outline grown inward from the box edge, as PIL draws `width`.
    fn rect(&mut self, b: PixelBox, width: i64, c: [u8; 3]) {
        let (x1, y1, x2, y2) = (
            b.0.round() as i64,
            b.1.round() as i64,
            b.2.round() as i64,
            b.3.round() as i64,
        );
        for k in 0..width {
            let (a, b_, cc, d) = (x1 + k, y1 + k, x2 - k, y2 - k);
            if a > cc || b_ > d {
                break;
            }
            self.fill(a, b_, cc, b_, c);
            self.fill(a, d, cc, d, c);
            self.fill(a, b_, a, d, c);
            self.fill(cc, b_, cc, d, c);
        }
    }

    /// Digits at (x, y), each font pixel a `scale`-sized square.
    fn text(&mut self, x: i64, y: i64, s: &str, scale: i64, c: [u8; 3]) {
        let mut cx = x;
        for ch in s.chars() {
            if let Some(d) = ch.to_digit(10) {
                for (row, bits) in DIGITS[d as usize].iter().enumerate() {
                    for col in 0..5 {
                        if bits & (0x10 >> col) != 0 {
                            let px = cx + col * scale;
                            let py = y + row as i64 * scale;
                            self.fill(px, py, px + scale - 1, py + scale - 1, c);
                        }
                    }
                }
            }
            cx += 6 * scale;
        }
    }
}

/// Blue boxes for OCR blocks, orange for accessibility controls, red (thicker) for the chosen one,
/// green for the focused field. Each item is labelled with its index just above its box.
///
/// `screen_bgra` is the capture (top-down BGRA); `field_box` is the focused field already in
/// capture pixels (the caller multiplies its points by the scale); `scale` sizes the labels.
#[allow(clippy::too_many_arguments)]
pub fn annotate(
    screen_bgra: &[u8],
    width: u32,
    height: u32,
    scale: f64,
    items: &[Item],
    chosen: &str,
    field_box: Option<PixelBox>,
    out_path: &Path,
) -> Result<(), String> {
    let n = width as usize * height as usize * 4;
    if screen_bgra.len() < n {
        return Err(format!(
            "capture has {} bytes, {width}x{height} needs {n}",
            screen_bgra.len()
        ));
    }
    let mut rgba = screen_bgra[..n].to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let mut cv = Canvas {
        rgba,
        w: width as i64,
        h: height as i64,
    };
    let glyph = ((scale * 11.0 / 7.0).round() as i64).max(1);
    // The chosen item is drawn last so nothing paints over it.
    let mut order: Vec<&Item> = items.iter().collect();
    order.sort_by_key(|it| it.index.to_string() == chosen);
    for it in order {
        let hit = it.index.to_string() == chosen;
        let color = if hit {
            RED
        } else if it.from_ax() {
            ORANGE
        } else {
            BLUE
        };
        cv.rect((it.x1, it.y1, it.x2, it.y2), if hit { 3 } else { 1 }, color);
        let ty = (it.y1 - 12.0 * scale).max(0.0).round() as i64;
        cv.text(
            it.x1.round() as i64,
            ty,
            &it.index.to_string(),
            glyph,
            color,
        );
    }
    if let Some(f) = field_box {
        cv.rect(f, 3, GREEN);
    }
    image::save_buffer(
        out_path,
        &cv.rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("could not write {}: {e}", out_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wcore_report_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn item(index: usize, source: &str, b: PixelBox, text: &str) -> Item {
        Item {
            index,
            text: text.into(),
            ocr_confidence: 0.9,
            x1: b.0,
            y1: b.1,
            x2: b.2,
            y2: b.3,
            role: if source == "ocr" {
                String::new()
            } else {
                "button".into()
            },
            source: source.into(),
        }
    }

    #[test]
    fn a_line_reaches_file_and_sink() {
        let path = tmp("both").join("run.log");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        let mut log = Log::new(
            Some(path.clone()),
            Some(Box::new(move |m: &str| s2.lock().unwrap().push(m.into()))),
        );
        log.line("hello");
        log.line("world");
        assert_eq!(*seen.lock().unwrap(), vec!["hello", "world"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn a_panicking_sink_does_not_stop_the_file_write() {
        let path = tmp("panic").join("run.log");
        let mut log = Log::new(
            Some(path.clone()),
            Some(Box::new(|_: &str| panic!("broken ui"))),
        );
        log.line("first");
        log.line("second");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn non_ascii_round_trips_through_the_log_file() {
        let path = tmp("utf8").join("run.log");
        let mut log = Log::new(Some(path.clone()), None);
        let msg = "café → 東京 — naïve “quotes” 🙂";
        log.line(msg);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), format!("{msg}\n"));
    }

    #[test]
    fn annotate_writes_a_png_with_the_chosen_box_red() {
        let (w, h) = (200u32, 120u32);
        let bgra = vec![40u8; (w * h * 4) as usize];
        let items = vec![
            item(0, "ocr", (10.0, 30.0, 60.0, 50.0), "a"),
            item(1, "ax", (70.0, 30.0, 120.0, 50.0), "b"),
            item(12, "ocr", (130.0, 40.0, 190.0, 100.0), "c"),
        ];
        let out = tmp("png").join("shot.png");
        annotate(
            &bgra,
            w,
            h,
            1.0,
            &items,
            "12",
            Some((5.0, 105.0, 50.0, 118.0)),
            &out,
        )
        .unwrap();
        let img = image::open(&out).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (w, h));
        assert_eq!(&img.get_pixel(130, 70).0[..3], &RED); // left edge of the chosen box
        assert_eq!(&img.get_pixel(132, 70).0[..3], &RED); // three pixels thick
        assert_eq!(&img.get_pixel(133, 70).0[..3], &[40, 40, 40]); // interior untouched
        assert_eq!(&img.get_pixel(70, 40).0[..3], &ORANGE);
        assert_eq!(&img.get_pixel(10, 40).0[..3], &BLUE);
        assert_eq!(&img.get_pixel(5, 110).0[..3], &GREEN);
    }

    #[test]
    fn the_payload_contains_the_goal_and_every_item() {
        let items = [
            item(0, "ocr", (1.0, 2.0, 3.0, 4.0), "Sign in"),
            item(1, "ax+ocr", (5.0, 6.0, 7.0, 8.0), "Search"),
        ];
        let state = serde_json::json!({"goal": "open my inbox"});
        let empty = serde_json::json!({});
        let p = Payload {
            state: &state,
            kind_criteria: &empty,
            item_criteria: &empty,
            site_criteria: &empty,
            offscreen_criteria: Some(&empty),
            items: items
                .iter()
                .map(|it| PayloadItem {
                    item: it,
                    click: (2.0, 3.0),
                    region: "top-left".into(),
                })
                .collect(),
            offscreen: vec![("button".into(), "More options".into())],
            field: None,
            width: 2560,
            height: 1440,
            scale: 1.0,
        };
        let text = render_payload(&p);
        assert!(text.contains("open my inbox"));
        assert!(text.contains("'Sign in'") && text.contains("'Search'"));
        assert!(text.contains("2 after merge/filter, 1 from the accessibility tree"));
        assert!(text.contains("2560x1440 capture, scale 1)"));
        assert!(text.contains("src=ax+ocr role=button"));
        assert!(text.contains("'More options'"));
        assert!(text.starts_with(RULE));
    }

    #[test]
    fn top_orders_by_probability_and_truncates() {
        let probs: BTreeMap<String, f64> = [("a", 0.1), ("b", 0.5), ("c", 0.3), ("d", 0.1)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let t = top(&probs, 2);
        assert_eq!(t, vec![("b".to_string(), 0.5), ("c".to_string(), 0.3)]);
        assert_eq!(top(&probs, 10).len(), 4);
    }

    #[test]
    fn ax_count_counts_ax_and_merged() {
        let items = [
            item(0, "ocr", (0.0, 0.0, 1.0, 1.0), ""),
            item(1, "ax", (0.0, 0.0, 1.0, 1.0), ""),
            item(2, "ax+ocr", (0.0, 0.0, 1.0, 1.0), ""),
        ];
        assert_eq!(ax_count(&items), 2);
    }

    /// Live: annotate a real capture of the primary display. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_annotate_a_real_capture() {
        let shot = platform::capture::capture_primary().expect("capture");
        let items = vec![
            item(0, "ocr", (40.0, 40.0, 340.0, 90.0), "ocr"),
            item(7, "ax", (400.0, 40.0, 700.0, 90.0), "ax"),
            item(23, "ocr", (40.0, 160.0, 500.0, 260.0), "chosen"),
            item(105, "ax+ocr", (600.0, 160.0, 900.0, 260.0), "merged"),
        ];
        let out = std::env::temp_dir().join("wcore_report_live.png");
        annotate(
            &shot.bgra,
            shot.width,
            shot.height,
            1.0,
            &items,
            "23",
            Some((40.0, 320.0, 700.0, 370.0)),
            &out,
        )
        .unwrap();
        println!(
            "LIVE {} {}x{} {} bytes",
            out.display(),
            shot.width,
            shot.height,
            std::fs::metadata(&out).unwrap().len()
        );
    }
}
