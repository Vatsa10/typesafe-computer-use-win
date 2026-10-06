//! Turn the display into clickable items: OCR text blocks and accessibility controls.
//!
//! A port of `typesafe_computer_use_win/perception.py`. Everything that touches the desktop goes
//! through the [`Desktop`] trait, so the decisions (which display, which window, which text, how
//! the two sources merge) are tested against a fake with no desktop involved. [`LiveDesktop`] is
//! the real one.
//!
//! Coordinates: window rectangles and control frames are virtual-desktop points; items are capture
//! pixels. The captured display's origin is subtracted exactly once on the way in, and
//! `Screen::to_points` adds it back exactly once on the way out.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use platform::capture::Shot;
use platform::display::Monitor;
use platform::uia::{Hit, Node, Walked, WalkedOf};
use platform::winlist::{WindowInfo, MIN_WINDOW_SIDE_PX};

use crate::config::{MAX_OPTIONS, MIN_OCR_CONFIDENCE};
use crate::models::{role_word, Field, Item, PixelBox, Screen};
use crate::timing::{phase, Timing, OCR_RECTS, OCR_REGION_PCT};
use crate::worldmodel::{rank, MAX_WINDOWS};

/// One raw OCR line: text, confidence, and its box in capture pixels.
pub type Line = (String, f64, PixelBox);

pub const ECHO_CHARS: usize = 24;
/// A history line shorter than this never filters anything: "waited" or "typed y" would otherwise
/// eat legitimate screen text wherever those words happen to appear.
pub const MIN_HISTORY_ECHO_CHARS: usize = 12;
pub const MIN_BOX_OVERLAP: f64 = 0.5; // intersection over the smaller box
pub const MIN_TOKEN_OVERLAP: f64 = 0.5;

// OCR scales with the amount of text, so the way to make it cheaper is to read less of the screen:
// the frontmost window's own columns, and within them only the blobs of tiles that changed.
pub const MENU_BAR_PT: f64 = 40.0;
pub const REGION_MARGIN_PT: f64 = 8.0;
pub const THUMB_DIVISOR: u32 = 8;
pub const TILE_PX: f64 = 256.0;
pub const TILE_DIFF: f64 = 6.0;
pub const REOCR_FRACTION: f64 = 0.6;
pub const MAX_REOCR_RECTS: usize = 4;

/// Address-bar names the browsers use, lower case.
pub const ADDRESS_BAR_NAMES: [&str; 3] = [
    "address and search bar",
    "address field",
    "search or enter address",
];

// ------------------------------------------------------------------ the desktop seam

/// Everything perception asks of the machine. The live implementation is [`LiveDesktop`]; tests
/// pass a fake so the decisions can be checked without a desktop.
pub trait Desktop {
    fn monitors(&self) -> Vec<Monitor>;
    /// Every real window, own-process windows already left out (as `winlist::open_windows` does).
    fn open_windows(&self) -> Vec<WindowInfo>;
    /// The window that actually has the foreground right now, 0 when none.
    fn foreground_hwnd(&self) -> isize;
    fn own_pid(&self) -> u32 {
        std::process::id()
    }
    fn process_name(&self, pid: u32) -> String;
    fn screenshot(&self, monitor: Option<&Monitor>) -> Option<Shot>;
    fn focused_field(&self) -> Option<Field>;
    /// The address bar's text of one browser window.
    fn browser_url(&self, hwnd: isize, display: (f64, f64), origin: (f64, f64)) -> Option<String>;
    /// OCR one BGRA image; boxes in that image's own pixels.
    fn ocr(&self, bgra: &[u8], width: u32, height: u32) -> Vec<Line>;
    /// The labelled controls of one window, never of a whole process.
    fn walk(&self, hwnd: isize, display_w: f64, display_h: f64, origin: (f64, f64)) -> Walked;
}

/// The real desktop.
pub struct LiveDesktop;

impl Desktop for LiveDesktop {
    fn monitors(&self) -> Vec<Monitor> {
        platform::display::monitors()
    }

    fn open_windows(&self) -> Vec<WindowInfo> {
        platform::winlist::open_windows(MIN_WINDOW_SIDE_PX)
    }

    fn foreground_hwnd(&self) -> isize {
        platform::winlist::foreground()
    }

    fn process_name(&self, pid: u32) -> String {
        platform::winlist::process_name(pid)
    }

    fn screenshot(&self, monitor: Option<&Monitor>) -> Option<Shot> {
        match monitor {
            Some(m) => platform::capture::capture_monitor(m),
            None => platform::capture::capture_primary(),
        }
    }

    fn focused_field(&self) -> Option<Field> {
        let node = platform::uia::focused_field()?;
        let value = platform::uia::value(&node.handle).unwrap_or_default();
        Some(Field {
            role: node.role,
            label: node.label,
            placeholder: String::new(),
            value,
            x: node.x,
            y: node.y,
            w: node.w,
            h: node.h,
        })
    }

    fn browser_url(&self, hwnd: isize, display: (f64, f64), origin: (f64, f64)) -> Option<String> {
        let walked = platform::uia::walk_window(hwnd, display.0, display.1, origin);
        let nodes = walked.on_screen.iter().chain(walked.off_screen.iter());
        let found: Vec<(String, String, String)> = nodes
            .filter(|n| n.role == "AXTextField" || n.role == "AXTextArea")
            .map(|n| {
                let value = platform::uia::value(&n.handle).unwrap_or_default();
                (n.role.clone(), n.label.clone(), value)
            })
            .collect();
        url_from_fields(&found)
    }

    fn ocr(&self, bgra: &[u8], width: u32, height: u32) -> Vec<Line> {
        platform::ocr::recognize(bgra, width, height)
            .into_iter()
            .map(|l| {
                (
                    l.text,
                    l.confidence as f64,
                    (l.x1 as f64, l.y1 as f64, l.x2 as f64, l.y2 as f64),
                )
            })
            .collect()
    }

    fn walk(&self, hwnd: isize, display_w: f64, display_h: f64, origin: (f64, f64)) -> Walked {
        platform::uia::walk_window(hwnd, display_w, display_h, origin)
    }
}

/// The URL among a window's text controls, given as (role, label, value): the omnibox by its
/// name, or any field or document whose value is already an address.
pub fn url_from_fields(fields: &[(String, String, String)]) -> Option<String> {
    for (_role, label, value) in fields {
        let name = label.to_lowercase();
        if !value.is_empty()
            && (ADDRESS_BAR_NAMES.iter().any(|h| name.contains(h))
                || value.starts_with("http://")
                || value.starts_with("https://"))
        {
            return Some(if value.contains("://") {
                value.clone()
            } else {
                format!("https://{value}")
            });
        }
    }
    None
}

/// An app named loosely: "Google Chrome", "chrome" and a window title all have to hit.
pub fn app_matches(wanted: &str, process: &str, title: &str) -> bool {
    let squash = |s: &str| s.to_lowercase().replace(' ', "");
    let (wanted, name, title) = (squash(wanted), squash(process), squash(title));
    !name.is_empty()
        && !wanted.is_empty()
        && (wanted.contains(&name) || name.contains(&wanted) || title.contains(&wanted))
}

// ------------------------------------------------------------------ which display, which window

/// The monitors, what is open on them, which display to read, and the window being read.
#[derive(Debug, Clone, Default)]
pub struct Survey {
    pub monitors: Vec<Monitor>,
    /// Ranked, best first (`worldmodel::rank`).
    pub windows: Vec<WindowInfo>,
    pub which: usize,
    pub origin: (f64, f64),
    /// The window this step is about: the actual foreground one, or a stand-in when the foreground
    /// is ours. None when the foreground is ours and nothing else is open.
    pub target: Option<WindowInfo>,
}

/// The display holding the foreground window, and that window.
///
/// The foreground is the handle Windows reports, looked up in the unranked inventory, never the
/// first record that happens to carry a foreground flag. The inventory leaves our own process out,
/// so a foreground handle missing from it is our own panel (or no real window): then the window
/// read is the top-ranked one someone else owns. The monitor comes from the record, which asked
/// `monitor_of`, so a minimized window is placed on its real display, not at -32000.
pub fn survey(desktop: &dyn Desktop) -> Survey {
    let monitors = desktop.monitors();
    let all = desktop.open_windows();
    let windows = rank(&all, MAX_WINDOWS);
    if monitors.is_empty() {
        return Survey {
            windows,
            ..Survey::default()
        };
    }
    let own = desktop.own_pid();
    let fg = desktop.foreground_hwnd();
    let front = all
        .iter()
        .find(|w| fg != 0 && w.hwnd == fg && w.pid != own)
        .cloned();
    let target = front.or_else(|| stand_in_window(&windows, own).cloned());
    let which = match &target {
        Some(w) if w.monitor < monitors.len() => w.monitor,
        _ => 0,
    };
    let m = &monitors[which];
    Survey {
        origin: (m.left as f64, m.top as f64),
        monitors,
        windows,
        which,
        target,
    }
}

/// The window to read instead of our own panel: the top-ranked one that is not this process's.
pub fn stand_in_window(inventory: &[WindowInfo], own_pid: u32) -> Option<&WindowInfo> {
    inventory.iter().find(|w| w.pid != own_pid)
}

/// The handle of the window this step is about.
///
/// The actual foreground handle when it belongs to the process being read. Otherwise (the step is
/// about a stand-in because our panel is in front) a window of that process from the inventory,
/// visible ones first. Never a window of a different process: the Python version fell back to
/// whichever record carried the foreground flag, so asking for Chrome could walk VS Code.
pub fn foreground_hwnd_of(
    foreground: isize,
    inventory: &[WindowInfo],
    pid: Option<u32>,
    own_pid: u32,
) -> Option<isize> {
    let pid = pid?;
    if foreground != 0 {
        match inventory.iter().find(|w| w.hwnd == foreground) {
            Some(w) if w.pid == pid => return Some(foreground),
            // Not in the inventory: our own window, which is what is being read when no
            // stand-in existed.
            None if pid == own_pid => return Some(foreground),
            _ => {}
        }
    }
    inventory
        .iter()
        .filter(|w| w.pid == pid)
        .min_by_key(|w| w.minimized)
        .map(|w| w.hwnd)
}

/// A window's rectangle as x, y, w, h in virtual-desktop points, when it is a window being worked
/// in: not minimized (its rectangle is parked at -32000) and bigger than a sliver.
pub fn window_bounds(window: &WindowInfo) -> Option<(f64, f64, f64, f64)> {
    let (l, t, r, b) = window.rect;
    let (w, h) = (r - l, b - t);
    if window.minimized || w <= MIN_WINDOW_SIDE_PX || h <= MIN_WINDOW_SIDE_PX {
        return None;
    }
    Some((l as f64, t as f64, w as f64, h as f64))
}

/// Capture the display holding the foreground window.
///
/// `browser` names the browser whose address bar to read; empty for none. `timing` records the
/// seconds under "world", "screenshot", "app", "window", "field" and "url".
pub fn capture(
    desktop: &dyn Desktop,
    browser: &str,
    mut timing: Option<&mut Timing>,
) -> Result<Screen, String> {
    let s = phase(timing.as_deref_mut(), "world", || survey(desktop));
    let shot = phase(timing.as_deref_mut(), "screenshot", || {
        desktop.screenshot(s.monitors.get(s.which))
    })
    .ok_or("the screenshot failed")?;
    let own = desktop.own_pid();
    let (app, pid) = phase(timing.as_deref_mut(), "app", || match &s.target {
        Some(w) => (w.app.clone(), w.pid),
        None => (desktop.process_name(own), own),
    });
    let window = phase(timing.as_deref_mut(), "window", || {
        s.target.as_ref().and_then(window_bounds)
    });
    let field = phase(timing.as_deref_mut(), "field", || desktop.focused_field());
    let origin = if s.monitors.is_empty() {
        (shot.origin.0 as f64, shot.origin.1 as f64)
    } else {
        s.origin
    };
    let url = phase(timing, "url", || {
        let w = s.target.as_ref()?;
        if browser.is_empty() || !app_matches(browser, &w.app, &w.title) {
            return None;
        }
        desktop.browser_url(w.hwnd, (shot.width as f64, shot.height as f64), origin)
    });
    Ok(Screen {
        bgra: shot.bgra,
        width: shot.width,
        height: shot.height,
        scale: 1.0, // the process is DPI aware: capture pixels are points
        app,
        field,
        url,
        pid: Some(pid),
        window,
        origin,
        monitor: s.which,
        windows: s.windows,
        monitors: s.monitors,
        ax_refs: HashMap::new(),
        offscreen: Vec::new(),
    })
}

/// Load a saved capture for replay. App and url are taken as given; nothing is asked of the
/// desktop except the primary's width, for the scale of a capture taken elsewhere.
pub fn replay(path: &Path, app: &str, url: Option<String>) -> Result<Screen, String> {
    let rgba = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let primary = platform::display::primary_size().0;
    let scale = if primary > 0 {
        width as f64 / primary as f64
    } else {
        1.0
    };
    Ok(Screen {
        bgra,
        width,
        height,
        scale,
        app: app.to_string(),
        field: None,
        url,
        pid: None,
        window: None,
        origin: (0.0, 0.0),
        monitor: 0,
        windows: Vec::new(),
        monitors: Vec::new(),
        ax_refs: HashMap::new(),
        offscreen: Vec::new(),
    })
}

// ------------------------------------------------------------------ never reading our own words

fn normalized(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// Substrings that identify a screen line as the command that launched this run.
pub fn goal_echoes(goal: &str) -> HashSet<String> {
    let norm = normalized(goal);
    if norm.chars().count() >= ECHO_CHARS {
        [head(&norm, ECHO_CHARS), tail(&norm, ECHO_CHARS)]
            .into_iter()
            .collect()
    } else {
        [norm].into_iter().collect()
    }
}

/// Substrings that identify a screen line as this run's own output read back off the screen. Only
/// the head of each entry, and only once the entry is long enough to be distinctive.
pub fn history_echoes(history: &[String]) -> HashSet<String> {
    history
        .iter()
        .map(|e| normalized(e))
        .filter(|n| n.chars().count() >= MIN_HISTORY_ECHO_CHARS)
        .map(|n| head(&n, ECHO_CHARS))
        .collect()
}

pub fn is_echo(text: &str, echoes: &HashSet<String>) -> bool {
    let norm = normalized(text);
    echoes
        .iter()
        .any(|e| !e.is_empty() && norm.contains(e.as_str()))
}

// ------------------------------------------------------------------ the step's items

/// Everything worth clicking on this screen: OCR text blocks, plus the app's own controls.
///
/// Fills `screen.ax_refs` (final item index to the control behind it) and `screen.offscreen`
/// (labelled controls the app exposes but does not show). A `cache` carries the previous capture's
/// OCR so only changed tiles are read again; None reads the whole region. `history` is the run's
/// own recent action descriptions, so the terminal showing them is not read back.
pub fn perceive(
    desktop: &dyn Desktop,
    screen: &mut Screen,
    budget: usize,
    goal: &str,
    mut timing: Option<&mut Timing>,
    cache: Option<&mut OcrCache>,
    history: &[String],
) -> Vec<Item> {
    let mut extras = (0.0, 0usize);
    let blocks = phase(timing.as_deref_mut(), "ocr", || {
        let (blocks, pct, rects) = ocr(desktop, screen, budget, goal, cache, history);
        extras = (pct, rects);
        blocks
    });
    if let Some(t) = timing.as_deref_mut() {
        t.set(OCR_REGION_PCT, (extras.0 * 10.0).round() / 10.0);
        t.set(OCR_RECTS, extras.1 as f64);
    }
    let (nodes, hidden, controls) = phase(timing, "ax", || {
        let (nodes, hidden) = ax_nodes(desktop, screen, budget);
        let controls = to_ax_items(&nodes, screen.scale, screen.origin);
        (nodes, hidden, controls)
    });
    let merged = merge_with_origins(&blocks, &controls, budget);
    screen.ax_refs.clear();
    for (item, origin) in &merged {
        if let Some(o) = origin {
            screen.ax_refs.insert(item.index, nodes[*o].handle.clone());
        }
    }
    let items: Vec<Item> = merged.into_iter().map(|(it, _)| it).collect();
    screen.offscreen = offscreen_controls(hidden, &items);
    items
}

/// The screen's text as items, filtered and merged into blocks, plus the share of the capture
/// read and how many crops it took.
pub fn ocr(
    desktop: &dyn Desktop,
    screen: &Screen,
    budget: usize,
    goal: &str,
    cache: Option<&mut OcrCache>,
    history: &[String],
) -> (Vec<Item>, f64, usize) {
    let (lines, pct, rects) = ocr_lines(desktop, screen, cache);
    (filter_and_block(lines, goal, history, budget), pct, rects)
}

/// The filter and merge over raw lines: drop blanks, faint lines and echoes, then join blocks.
pub fn filter_and_block(
    lines: Vec<Line>,
    goal: &str,
    history: &[String],
    budget: usize,
) -> Vec<Item> {
    let mut echoes = goal_echoes(goal);
    echoes.extend(history_echoes(history));
    let kept: Vec<Line> = lines
        .into_iter()
        .filter(|(t, c, _)| {
            !t.trim().is_empty() && *c >= MIN_OCR_CONFIDENCE && !is_echo(t, &echoes)
        })
        .map(|(t, c, b)| (t.trim().to_string(), c, b))
        .collect();
    to_items(&merge_blocks(&kept), budget)
}

// ------------------------------------------------------------------ reading less of the screen

/// A reduced grayscale copy of a capture.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Thumb {
    fn at(&self, x: i64, y: i64) -> u8 {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            0 // a crop past the edge reads black, as PIL's does
        } else {
            self.data[y as usize * self.width as usize + x as usize]
        }
    }
}

/// The previous capture's OCR, and what makes it reusable. One cache belongs to one run.
#[derive(Debug, Clone, Default)]
pub struct OcrCache {
    pub app: Option<String>,
    pub window: Option<(f64, f64, f64, f64)>,
    pub region: Option<PixelBox>,
    pub thumb: Option<Thumb>,
    pub lines: Vec<Line>,
}

impl OcrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Never across a different app, a moved or resized window, or a different read region.
    pub fn reusable(&self, screen: &Screen, region: PixelBox, thumb: &Thumb) -> bool {
        matches!(&self.thumb, Some(t) if t.width == thumb.width && t.height == thumb.height)
            && self.app.as_deref() == Some(screen.app.as_str())
            && self.window == screen.window
            && self.region == Some(region)
    }

    pub fn store(&mut self, screen: &Screen, region: PixelBox, thumb: Thumb, lines: Vec<Line>) {
        self.app = Some(screen.app.clone());
        self.window = screen.window;
        self.region = Some(region);
        self.thumb = Some(thumb);
        self.lines = lines;
    }
}

/// Raw OCR lines in full-capture pixels, the share of the capture read, and how many crops.
pub fn ocr_lines(
    desktop: &dyn Desktop,
    screen: &Screen,
    cache: Option<&mut OcrCache>,
) -> (Vec<Line>, f64, usize) {
    let region = ocr_region(screen);
    let area = (screen.width as f64 * screen.height as f64).max(1.0);
    let read_pct =
        |rects: &[PixelBox]| 100.0 * rects.iter().map(|r| area_of(*r)).sum::<f64>() / area;
    let Some(cache) = cache else {
        return (ocr_crop(desktop, screen, region), read_pct(&[region]), 0);
    };
    let thumb = thumbnail(&screen.bgra, screen.width, screen.height, THUMB_DIVISOR);
    let full = |cache: &mut OcrCache, thumb: Thumb| {
        let lines = ocr_crop(desktop, screen, region);
        cache.store(screen, region, thumb, lines.clone());
        (lines, read_pct(&[region]), 0)
    };
    if !cache.reusable(screen, region, &thumb) {
        return full(cache, thumb);
    }
    let tiles = tiles_in(region, TILE_PX);
    let previous = cache.thumb.clone().expect("reusable implies a thumb");
    let changed = changed_tiles(&thumb, &previous, &tiles, THUMB_DIVISOR, TILE_DIFF);
    if changed.len() as f64 > REOCR_FRACTION * tiles.len() as f64 {
        return full(cache, thumb);
    }
    let rects = reocr_rects(&changed, region, &cache.lines, MAX_REOCR_RECTS, TILE_PX);
    if rects.is_empty() {
        let lines = cache.lines.clone();
        cache.store(screen, region, thumb, lines.clone());
        return (lines, 0.0, 0);
    }
    if rects.iter().map(|r| area_of(*r)).sum::<f64>() > REOCR_FRACTION * area_of(region) {
        return full(cache, thumb);
    }
    let fresh: Vec<Line> = rects
        .iter()
        .flat_map(|r| ocr_crop(desktop, screen, *r))
        .collect();
    let lines = merge_reocr(&cache.lines, fresh, &rects);
    cache.store(screen, region, thumb, lines.clone());
    let pct = read_pct(&rects);
    (lines, pct, rects.len())
}

/// The part of the capture worth reading, in capture pixels: the frontmost window with a margin,
/// joined with the menu-bar strip over the same columns, clamped to the display. The window is in
/// virtual-desktop points, so the display's origin comes off first.
pub fn ocr_region(screen: &Screen) -> PixelBox {
    let (width, height) = (screen.width as f64, screen.height as f64);
    let Some((x, y, w, h)) = screen.window else {
        return (0.0, 0.0, width, height);
    };
    let (x, y) = (x - screen.origin.0, y - screen.origin.1);
    let (scale, margin) = (screen.scale, REGION_MARGIN_PT);
    let window = (
        (x - margin) * scale,
        (y - margin) * scale,
        (x + w + margin) * scale,
        (y + h + margin) * scale,
    );
    let joined = (
        window.0,
        window.1.min(0.0),
        window.2,
        window.3.max(MENU_BAR_PT * scale),
    );
    let clamped = (
        joined.0.max(0.0),
        joined.1.max(0.0),
        joined.2.min(width),
        joined.3.min(height),
    );
    if clamped.2 > clamped.0 && clamped.3 > clamped.1 {
        clamped
    } else {
        (0.0, 0.0, width, height)
    }
}

/// OCR one rectangle of the capture. Boxes come back in full-capture pixels.
pub fn ocr_crop(desktop: &dyn Desktop, screen: &Screen, rect: PixelBox) -> Vec<Line> {
    let clampw = |v: f64, max: u32| (v.round_ties_even().max(0.0) as u32).min(max);
    let (x1, y1) = (clampw(rect.0, screen.width), clampw(rect.1, screen.height));
    let (x2, y2) = (clampw(rect.2, screen.width), clampw(rect.3, screen.height));
    if x2 <= x1 || y2 <= y1 {
        return Vec::new();
    }
    let raw = if (x1, y1, x2, y2) == (0, 0, screen.width, screen.height) {
        desktop.ocr(&screen.bgra, screen.width, screen.height)
    } else {
        let crop = crop_bgra(&screen.bgra, screen.width, (x1, y1, x2, y2));
        desktop.ocr(&crop, x2 - x1, y2 - y1)
    };
    let (dx, dy) = (x1 as f64, y1 as f64);
    raw.into_iter()
        .map(|(t, c, b)| (t, c, (b.0 + dx, b.1 + dy, b.2 + dx, b.3 + dy)))
        .collect()
}

fn crop_bgra(bgra: &[u8], width: u32, (x1, y1, x2, y2): (u32, u32, u32, u32)) -> Vec<u8> {
    let stride = width as usize * 4;
    let mut out = Vec::with_capacity(((x2 - x1) * (y2 - y1) * 4) as usize);
    for y in y1..y2 {
        let row = y as usize * stride;
        out.extend_from_slice(&bgra[row + x1 as usize * 4..row + x2 as usize * 4]);
    }
    out
}

/// A grayscale copy at 1/divisor scale, box-averaged (PIL's `convert("L").reduce(divisor)`): the
/// last row and column average the pixels they have.
pub fn thumbnail(bgra: &[u8], width: u32, height: u32, divisor: u32) -> Thumb {
    let d = divisor.max(1);
    let (tw, th) = (width.div_ceil(d), height.div_ceil(d));
    let mut data = Vec::with_capacity((tw * th) as usize);
    for ty in 0..th {
        for tx in 0..tw {
            let (mut sum, mut n) = (0u32, 0u32);
            for y in ty * d..((ty + 1) * d).min(height) {
                for x in tx * d..((tx + 1) * d).min(width) {
                    let i = (y as usize * width as usize + x as usize) * 4;
                    let (b, g, r) = (bgra[i] as u32, bgra[i + 1] as u32, bgra[i + 2] as u32);
                    sum += (r * 19595 + g * 38470 + b * 7471 + 0x8000) >> 16;
                    n += 1;
                }
            }
            data.push(((sum + n / 2) / n.max(1)) as u8);
        }
    }
    Thumb {
        width: tw,
        height: th,
        data,
    }
}

/// The region cut into tiles aligned to its own origin. The last row and column are short.
pub fn tiles_in(region: PixelBox, tile: f64) -> Vec<PixelBox> {
    let (x1, y1, x2, y2) = region;
    let mut out = Vec::new();
    let mut y = y1;
    while y < y2 {
        let mut x = x1;
        while x < x2 {
            out.push((x, y, (x + tile).min(x2), (y + tile).min(y2)));
            x += tile;
        }
        y += tile;
    }
    out
}

fn patch_bounds(tile: PixelBox, divisor: u32) -> (i64, i64, i64, i64) {
    let d = divisor as f64;
    let r = |v: f64| (v / d).round_ties_even() as i64;
    let (x1, y1) = (r(tile.0), r(tile.1));
    (x1, y1, r(tile.2).max(x1 + 1), r(tile.3).max(y1 + 1))
}

/// True when the tile's mean absolute pixel difference clears the threshold. A tile that cannot
/// be compared counts as changed, so a doubt is paid for with a re-read.
pub fn tile_changed(
    thumb: &Thumb,
    previous: &Thumb,
    tile: PixelBox,
    divisor: u32,
    threshold: f64,
) -> bool {
    if thumb.width != previous.width || thumb.height != previous.height {
        return true;
    }
    let (x1, y1, x2, y2) = patch_bounds(tile, divisor);
    let (mut sum, mut n) = (0u64, 0u64);
    for y in y1..y2 {
        for x in x1..x2 {
            sum += (thumb.at(x, y) as i32 - previous.at(x, y) as i32).unsigned_abs() as u64;
            n += 1;
        }
    }
    n == 0 || sum as f64 / n as f64 > threshold
}

pub fn changed_tiles(
    thumb: &Thumb,
    previous: &Thumb,
    tiles: &[PixelBox],
    divisor: u32,
    threshold: f64,
) -> Vec<PixelBox> {
    tiles
        .iter()
        .copied()
        .filter(|t| tile_changed(thumb, previous, *t, divisor, threshold))
        .collect()
}

/// The changed tiles grouped into blobs that touch along a side or at a corner.
pub fn tile_clusters(changed: &[PixelBox], tile: f64) -> Vec<Vec<PixelBox>> {
    let mut parent: Vec<usize> = (0..changed.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for i in 0..changed.len() {
        for j in i + 1..changed.len() {
            let (a, b) = (changed[i], changed[j]);
            if (a.0 - b.0).abs() <= 1.5 * tile && (a.1 - b.1).abs() <= 1.5 * tile {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    // Blobs in order of first appearance, as the Python dict keeps them.
    let mut order: Vec<usize> = Vec::new();
    let mut blobs: BTreeMap<usize, Vec<PixelBox>> = BTreeMap::new();
    for (i, b) in changed.iter().enumerate() {
        let r = root(&mut parent, i);
        if !blobs.contains_key(&r) {
            order.push(r);
        }
        blobs.entry(r).or_default().push(*b);
    }
    order
        .into_iter()
        .map(|r| blobs.remove(&r).unwrap_or_default())
        .collect()
}

/// The rectangles to read again, one OCR call each. Empty when nothing changed.
pub fn reocr_rects(
    changed: &[PixelBox],
    region: PixelBox,
    lines: &[Line],
    limit: usize,
    tile: f64,
) -> Vec<PixelBox> {
    let blobs: Vec<PixelBox> = tile_clusters(changed, tile)
        .iter()
        .map(|b| blob_rect(b, region, tile))
        .collect();
    let mut rects = settled(blobs, lines, region);
    while rects.len() > limit {
        let (i, j) = closest_pair(&rects);
        let mut next = vec![union_box(rects[i], rects[j])];
        next.extend(
            rects
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != i && *k != j)
                .map(|(_, r)| *r),
        );
        rects = settled(next, lines, region);
    }
    rects
}

/// One blob's bounding box, padded by a tile so a line crossing the edge is read whole, clamped.
pub fn blob_rect(blob: &[PixelBox], region: PixelBox, tile: f64) -> PixelBox {
    let x1 = blob.iter().map(|b| b.0).fold(f64::INFINITY, f64::min) - tile;
    let y1 = blob.iter().map(|b| b.1).fold(f64::INFINITY, f64::min) - tile;
    let x2 = blob.iter().map(|b| b.2).fold(f64::NEG_INFINITY, f64::max) + tile;
    let y2 = blob.iter().map(|b| b.3).fold(f64::NEG_INFINITY, f64::max) + tile;
    (
        region.0.max(x1),
        region.1.max(y1),
        region.2.min(x2),
        region.3.min(y2),
    )
}

/// Grow every rectangle past the lines it would cut and merge the ones that meet, to a fixed point.
pub fn settled(mut rects: Vec<PixelBox>, lines: &[Line], region: PixelBox) -> Vec<PixelBox> {
    loop {
        let grown: Vec<PixelBox> = rects
            .iter()
            .map(|r| grown_for_lines(*r, lines, region))
            .collect();
        let merged = merge_touching(&grown);
        if grown == rects && merged == grown {
            return merged;
        }
        rects = merged;
    }
}

/// The rectangles with every overlapping or touching pair replaced by the box around both.
pub fn merge_touching(rects: &[PixelBox]) -> Vec<PixelBox> {
    let mut out = rects.to_vec();
    'outer: loop {
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                if boxes_touch(out[i], out[j]) {
                    let mut next = vec![union_box(out[i], out[j])];
                    next.extend(
                        out.iter()
                            .enumerate()
                            .filter(|(k, _)| *k != i && *k != j)
                            .map(|(_, r)| *r),
                    );
                    out = next;
                    continue 'outer;
                }
            }
        }
        return out;
    }
}

/// Positions of the two rectangles with the smallest gap between them.
pub fn closest_pair(rects: &[PixelBox]) -> (usize, usize) {
    let mut best = (f64::INFINITY, 0, 1);
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            let g = gap_between(rects[i], rects[j]);
            if g < best.0 {
                best = (g, i, j);
            }
        }
    }
    (best.1, best.2)
}

/// Distance between two rectangles, zero when they overlap or touch.
pub fn gap_between(a: PixelBox, b: PixelBox) -> f64 {
    let dx = (a.0.max(b.0) - a.2.min(b.2)).max(0.0);
    let dy = (a.1.max(b.1) - a.3.min(b.3)).max(0.0);
    dx.hypot(dy)
}

pub fn union_box(a: PixelBox, b: PixelBox) -> PixelBox {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

/// Overlapping, or meeting along an edge or at a corner.
pub fn boxes_touch(a: PixelBox, b: PixelBox) -> bool {
    a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3
}

pub fn area_of(b: PixelBox) -> f64 {
    (b.2 - b.0).max(0.0) * (b.3 - b.1).max(0.0)
}

/// The rectangle grown until no known line straddles its edge, clamped to the region.
pub fn grown_for_lines(rect: PixelBox, lines: &[Line], region: PixelBox) -> PixelBox {
    let mut r = rect;
    let mut growing = true;
    while growing {
        growing = false;
        for (_, _, b) in lines {
            if !boxes_intersect(*b, r) {
                continue;
            }
            let grown = (
                region.0.max(r.0.min(b.0)),
                region.1.max(r.1.min(b.1)),
                region.2.min(r.2.max(b.2)),
                region.3.min(r.3.max(b.3)),
            );
            if grown != r {
                r = grown;
                growing = true;
            }
        }
    }
    r
}

pub fn boxes_intersect(a: PixelBox, b: PixelBox) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// Previous lines no re-read rectangle touches, plus every line just read inside them.
pub fn merge_reocr(previous: &[Line], fresh: Vec<Line>, rects: &[PixelBox]) -> Vec<Line> {
    let mut out: Vec<Line> = previous
        .iter()
        .filter(|l| !rects.iter().any(|r| boxes_intersect(l.2, *r)))
        .cloned()
        .collect();
    out.extend(fresh);
    out
}

// ------------------------------------------------------------------ the accessibility tree

/// The foreground window's labelled controls, and the off-screen ones it still exposes.
///
/// Scoped to one window handle: Chrome hosts every Chrome window in one process, so a
/// whole-process walk offers a sibling window's controls as this one's. No handle, no walk.
pub fn ax_nodes(desktop: &dyn Desktop, screen: &Screen, budget: usize) -> (Vec<Node>, Vec<Node>) {
    let Some(hwnd) = foreground_hwnd_of(
        desktop.foreground_hwnd(),
        &screen.windows,
        screen.pid,
        desktop.own_pid(),
    ) else {
        return (Vec::new(), Vec::new());
    };
    let (w, h) = screen.size_pt();
    let walked = desktop.walk(hwnd, w, h, screen.origin);
    labelled(walked, budget)
}

/// The labelled on-screen nodes within the budget, and the labelled off-screen ones.
pub fn labelled<N>(walked: WalkedOf<N>, budget: usize) -> (Vec<Hit<N>>, Vec<Hit<N>>) {
    let on: Vec<Hit<N>> = walked
        .on_screen
        .into_iter()
        .take(budget)
        .filter(|n| !n.label.is_empty())
        .collect();
    let off = walked
        .off_screen
        .into_iter()
        .filter(|n| !n.label.is_empty())
        .collect();
    (on, off)
}

/// The off-screen controls worth offering: one per role and label, minus anything on screen.
pub fn offscreen_controls<N>(nodes: Vec<Hit<N>>, items: &[Item]) -> Vec<Hit<N>> {
    let visible: HashSet<&str> = items.iter().map(|it| it.text.as_str()).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    nodes
        .into_iter()
        .filter(|n| {
            !visible.contains(n.label.as_str()) && seen.insert((n.role.clone(), n.label.clone()))
        })
        .collect()
}

/// Controls as items, from virtual-desktop points to capture pixels: the origin comes off once.
pub fn to_ax_items<N>(nodes: &[Hit<N>], scale: f64, origin: (f64, f64)) -> Vec<Item> {
    let (left, top) = origin;
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| Item {
            index: i,
            text: n.label.clone(),
            ocr_confidence: 1.0,
            x1: (n.x - left) * scale,
            y1: (n.y - top) * scale,
            x2: (n.x + n.w - left) * scale,
            y2: (n.y + n.h - top) * scale,
            role: role_word(&n.role).to_string(),
            source: "ax".into(),
        })
        .collect()
}

/// The frontmost window's labelled on-screen controls as items on the capture.
pub fn ax_items(desktop: &dyn Desktop, screen: &Screen, budget: usize) -> Vec<Item> {
    to_ax_items(
        &ax_nodes(desktop, screen, budget).0,
        screen.scale,
        screen.origin,
    )
}

// ------------------------------------------------------------------ merging the two sources

/// One item per thing. A control that sits on the OCR block naming it replaces both.
pub fn merge_sources(ocr_items: &[Item], ax_items: &[Item], budget: usize) -> Vec<Item> {
    merge_with_origins(ocr_items, ax_items, budget)
        .into_iter()
        .map(|(it, _)| it)
        .collect()
}

/// [`merge_sources`] with the default budget, the Choice ceiling.
pub fn merge_sources_default(ocr_items: &[Item], ax_items: &[Item]) -> Vec<Item> {
    merge_sources(ocr_items, ax_items, MAX_OPTIONS)
}

/// The merge, each item paired with the position of the control it came from, None for text.
pub fn merge_with_origins(
    ocr_items: &[Item],
    ax_items: &[Item],
    budget: usize,
) -> Vec<(Item, Option<usize>)> {
    let mut taken: HashSet<usize> = HashSet::new();
    let mut merged: Vec<(Item, Option<usize>)> = Vec::new();
    for (origin, control) in ax_items.iter().enumerate() {
        let mut best: Option<usize> = None;
        let mut best_overlap = MIN_BOX_OVERLAP;
        for (i, block) in ocr_items.iter().enumerate() {
            if taken.contains(&i) {
                continue;
            }
            let overlap = box_overlap(control, block);
            if overlap >= best_overlap && texts_match(&control.text, &block.text) {
                best = Some(i);
                best_overlap = overlap;
            }
        }
        let Some(b) = best else {
            merged.push((control.clone(), Some(origin)));
            continue;
        };
        taken.insert(b);
        let block = &ocr_items[b];
        let text = if control.text.chars().count() >= block.text.chars().count() {
            control.text.clone()
        } else {
            block.text.clone()
        };
        merged.push((
            Item {
                text,
                role: control.role.clone(),
                source: "ax+ocr".into(),
                ..block.clone()
            },
            Some(origin),
        ));
    }
    merged.extend(
        ocr_items
            .iter()
            .enumerate()
            .filter(|(i, _)| !taken.contains(i))
            .map(|(_, b)| (b.clone(), None)),
    );
    let all: Vec<Item> = merged.iter().map(|(it, _)| it.clone()).collect();
    let kept: Vec<(Item, Option<usize>)> = kept_by_budget(&all, budget)
        .into_iter()
        .map(|i| merged[i].clone())
        .collect();
    let items: Vec<Item> = kept.iter().map(|(it, _)| it.clone()).collect();
    reading_order(&items)
        .into_iter()
        .enumerate()
        .map(|(i, j)| {
            let (it, o) = &kept[j];
            (
                Item {
                    index: i,
                    ..it.clone()
                },
                *o,
            )
        })
        .collect()
}

/// Intersection over the smaller box, so a tight control inside a wide text line still counts.
pub fn box_overlap(a: &Item, b: &Item) -> f64 {
    let wide = a.x2.min(b.x2) - a.x1.max(b.x1);
    let tall = a.y2.min(b.y2) - a.y1.max(b.y1);
    let smaller = ((a.x2 - a.x1) * (a.y2 - a.y1)).min((b.x2 - b.x1) * (b.y2 - b.y1));
    if wide > 0.0 && tall > 0.0 && smaller > 0.0 {
        wide * tall / smaller
    } else {
        0.0
    }
}

/// One label contains the other, or they share half their words.
pub fn texts_match(a: &str, b: &str) -> bool {
    let (x, y) = (normalized(a), normalized(b));
    if x.is_empty() || y.is_empty() {
        return false;
    }
    if x.contains(&y) || y.contains(&x) {
        return true;
    }
    let wx: HashSet<&str> = x.split(' ').collect();
    let wy: HashSet<&str> = y.split(' ').collect();
    wx.intersection(&wy).count() as f64 / wx.len().min(wy.len()) as f64 >= MIN_TOKEN_OVERLAP
}

/// Which items survive the ceiling: the faintest OCR-only blocks go first, and a control is never
/// dropped for text. Their positions, in the order given.
pub fn kept_by_budget(items: &[Item], budget: usize) -> Vec<usize> {
    if items.len() <= budget {
        return (0..items.len()).collect();
    }
    let mut ranked: Vec<usize> = (0..items.len()).collect();
    ranked.sort_by(|&i, &j| {
        (items[i].from_ax(), items[i].ocr_confidence)
            .partial_cmp(&(items[j].from_ax(), items[j].ocr_confidence))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let dropped: HashSet<usize> = ranked[..items.len() - budget].iter().copied().collect();
    (0..items.len()).filter(|i| !dropped.contains(i)).collect()
}

pub fn to_items(lines: &[Line], budget: usize) -> Vec<Item> {
    let items: Vec<Item> = lines
        .iter()
        .map(|(t, c, b)| Item {
            index: 0,
            text: t.clone(),
            ocr_confidence: *c,
            x1: b.0,
            y1: b.1,
            x2: b.2,
            y2: b.3,
            role: String::new(),
            source: "ocr".into(),
        })
        .collect();
    let mut out = order_items(&items);
    out.truncate(budget);
    out
}

/// Number items in reading order: rows by the median item height, then left to right.
pub fn order_items(items: &[Item]) -> Vec<Item> {
    reading_order(items)
        .into_iter()
        .enumerate()
        .map(|(i, j)| Item {
            index: i,
            ..items[j].clone()
        })
        .collect()
}

/// Positions of the items in reading order.
pub fn reading_order(items: &[Item]) -> Vec<usize> {
    let mut heights: Vec<f64> = items.iter().map(|it| it.y2 - it.y1).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let row_h = heights
        .get(heights.len() / 2)
        .copied()
        .unwrap_or(1.0)
        .max(1.0);
    let key = |i: usize| {
        let it = &items[i];
        (((it.y1 + it.y2) / 2.0 / row_h).round_ties_even(), it.x1)
    };
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        key(a)
            .partial_cmp(&key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    order
}

/// Join lines that continue a block above them: aligned left edge, small gap, similar height.
pub fn merge_blocks(lines: &[Line]) -> Vec<Line> {
    struct Block {
        text: String,
        conf: f64,
        b: PixelBox,
        last_h: f64,
    }
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by(|a, b| {
        (a.2 .1, a.2 .0)
            .partial_cmp(&(b.2 .1, b.2 .0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut blocks: Vec<Block> = Vec::new();
    for (text, conf, (x1, y1, x2, y2)) in sorted {
        let h = y2 - y1;
        let mut best: Option<(f64, usize)> = None;
        for (k, block) in blocks.iter().enumerate() {
            let (bx1, by2, bh) = (block.b.0, block.b.3, block.last_h);
            let gap = y1 - by2;
            let continues = (x1 - bx1).abs() < 0.6 * bh
                && -0.2 * bh < gap
                && gap < 0.8 * bh
                && 0.7 < h / bh.max(1.0)
                && h / bh.max(1.0) < 1.4;
            if continues && best.is_none_or(|(g, _)| gap < g) {
                best = Some((gap, k));
            }
        }
        match best {
            None => blocks.push(Block {
                text: text.clone(),
                conf: *conf,
                b: (*x1, *y1, *x2, *y2),
                last_h: h,
            }),
            Some((_, k)) => {
                let block = &mut blocks[k];
                block.text = format!("{} {}", block.text, text);
                block.conf = block.conf.min(*conf);
                block.b = (block.b.0.min(*x1), block.b.1, block.b.2.max(*x2), *y2);
                block.last_h = h;
            }
        }
    }
    blocks.into_iter().map(|b| (b.text, b.conf, b.b)).collect()
}

/// Text of items within a radius of the focused field, in screen points.
pub fn near_field(screen: &Screen, items: &[Item], radius_pt: f64) -> Vec<String> {
    let Some(f) = &screen.field else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|it| {
            let (cx, cy) = screen.to_points(it);
            (cx - (f.x + f.w / 2.0)).abs() < radius_pt + f.w / 2.0
                && (cy - (f.y + f.h / 2.0)).abs() < radius_pt
        })
        .map(|it| it.text.clone())
        .collect()
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const OWN: u32 = 1000;

    fn line(text: &str, x1: f64, y1: f64, x2: f64, y2: f64) -> Line {
        (text.into(), 1.0, (x1, y1, x2, y2))
    }

    fn texts(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| i.text.clone()).collect()
    }

    fn item(index: usize, text: &str, b: PixelBox, conf: f64, role: &str, source: &str) -> Item {
        Item {
            index,
            text: text.into(),
            ocr_confidence: conf,
            x1: b.0,
            y1: b.1,
            x2: b.2,
            y2: b.3,
            role: role.into(),
            source: source.into(),
        }
    }

    fn ocr_item(i: usize, t: &str, x1: f64, y1: f64, x2: f64, y2: f64) -> Item {
        item(i, t, (x1, y1, x2, y2), 0.9, "", "ocr")
    }

    fn ax_item(i: usize, t: &str, x1: f64, y1: f64, x2: f64, y2: f64) -> Item {
        item(i, t, (x1, y1, x2, y2), 1.0, "button", "ax")
    }

    fn sources(items: &[Item]) -> Vec<String> {
        let mut s: Vec<String> = items.iter().map(|i| i.source.clone()).collect();
        s.sort();
        s
    }

    fn blank_screen(w: u32, h: u32) -> Screen {
        Screen {
            bgra: vec![0; (w * h * 4) as usize],
            width: w,
            height: h,
            scale: 1.0,
            app: "code".into(),
            field: None,
            url: None,
            pid: None,
            window: None,
            origin: (0.0, 0.0),
            monitor: 0,
            windows: Vec::new(),
            monitors: Vec::new(),
            ax_refs: HashMap::new(),
            offscreen: Vec::new(),
        }
    }

    // ------------------------------------------------ blocks and order

    #[test]
    fn merges_stacked_lines_across_columns() {
        let lines = vec![
            line("Kash Patel defends", 1080., 1531., 1300., 1561.),
            line("Two House Democrats", 1400., 1531., 1600., 1561.),
            line("removing bestiality as", 1075., 1571., 1300., 1601.),
            line("defect again on key vote", 1400., 1571., 1600., 1601.),
            line("FBI applicants", 1080., 1606., 1300., 1636.),
        ];
        let mut t: Vec<String> = merge_blocks(&lines).into_iter().map(|l| l.0).collect();
        t.sort();
        assert_eq!(
            t,
            vec![
                "Kash Patel defends removing bestiality as FBI applicants",
                "Two House Democrats defect again on key vote"
            ]
        );
    }

    #[test]
    fn does_not_merge_far_or_misaligned_lines() {
        let lines = vec![
            line("Home", 100., 100., 200., 130.),
            line("World", 400., 100., 500., 130.),
            line("Footer", 100., 900., 200., 930.),
        ];
        assert_eq!(merge_blocks(&lines).len(), 3);
    }

    #[test]
    fn merged_block_keeps_min_confidence_and_union_box() {
        let lines = vec![
            ("a".to_string(), 1.0, (100., 100., 200., 130.)),
            ("b".to_string(), 0.5, (102., 140., 260., 170.)),
        ];
        let out = merge_blocks(&lines);
        assert_eq!(
            out,
            vec![("a b".to_string(), 0.5, (100., 100., 260., 170.))]
        );
    }

    #[test]
    fn reading_order_rows_then_columns() {
        let lines = vec![
            line("right", 800., 100., 900., 130.),
            line("left", 100., 105., 200., 135.),
            line("below", 100., 300., 200., 330.),
        ];
        assert_eq!(
            texts(&to_items(&lines, 255)),
            vec!["left", "right", "below"]
        );
    }

    #[test]
    fn budget_caps_items() {
        let lines: Vec<Line> = (0..10)
            .map(|i| {
                let y = 100. + 40. * i as f64;
                line(&i.to_string(), 100., y, 200., y + 30.)
            })
            .collect();
        assert_eq!(to_items(&lines, 3).len(), 3);
    }

    #[test]
    fn order_items_renumbers_rows_then_columns() {
        let items = vec![
            ocr_item(7, "right", 800., 100., 900., 130.),
            ocr_item(2, "left", 100., 105., 200., 135.),
        ];
        let got: Vec<(usize, String)> = order_items(&items)
            .into_iter()
            .map(|i| (i.index, i.text))
            .collect();
        assert_eq!(got, vec![(0, "left".into()), (1, "right".into())]);
    }

    // ------------------------------------------------ echoes

    #[test]
    fn goal_echo_matches_wrapped_command_lines() {
        let echoes =
            goal_echoes("go to cnn and click onto something related to AI on the homepage");
        assert!(is_echo(
            "clear && uv run clicker \"go to cnn and click onto something",
            &echoes
        ));
        assert!(is_echo("related to AI on the homepage\" --act", &echoes));
        assert!(!is_echo("Trending: Trump and AI warnings", &echoes));
    }

    #[test]
    fn a_screen_line_echoing_a_recent_action_is_dropped() {
        let echoes = history_echoes(&[
            "did: switched to chrome 'Vatsa10/typesafe-computer-use' on monitor 3".into(),
            "did: typed the search query".into(),
        ]);
        assert!(is_echo(
            "did: switched to chrome \u{2014} Google Chrome'",
            &echoes
        ));
        assert!(is_echo("0.10 [22] did: switched to CHROME ...", &echoes));
        assert!(!is_echo("Google Chrome", &echoes));
        assert!(!is_echo("Trending: Trump and AI warnings", &echoes));
    }

    #[test]
    fn a_short_history_line_does_not_filter_unrelated_screen_text() {
        let echoes = history_echoes(&["waited".into(), "typed y".into(), "did: done".into()]);
        assert!(echoes.is_empty());
        assert!(!is_echo("waited for the page", &echoes));
        assert!(!is_echo("Start", &echoes));
    }

    #[test]
    fn history_echoes_tolerate_no_history() {
        assert!(history_echoes(&[]).is_empty());
    }

    #[test]
    fn the_goal_echo_filter_still_works_alongside_history() {
        let mut echoes =
            goal_echoes("go to cnn and click onto something related to AI on the homepage");
        echoes.extend(history_echoes(&[
            "did: switched to chrome on monitor 3".into()
        ]));
        assert!(is_echo(
            "clear && uv run clicker \"go to cnn and click onto something",
            &echoes
        ));
        assert!(is_echo("did: switched to chrome on monitor 3", &echoes));
        assert!(!is_echo("Trending: Trump and AI warnings", &echoes));
    }

    #[test]
    fn an_empty_goal_filters_nothing() {
        // Python's goal_echoes("") is {""}, which every line contains: an empty goal dropped all text.
        assert!(!is_echo("Headline", &goal_echoes("")));
    }

    #[test]
    fn echoes_count_characters_not_bytes() {
        // 24 chars of non-ASCII text must not split a code point.
        let goal = "\u{00e9}".repeat(30);
        let echoes = goal_echoes(&goal);
        assert!(echoes.iter().all(|e| e.chars().count() == ECHO_CHARS));
    }

    #[test]
    fn the_filter_drops_echoes_faint_and_blank_lines() {
        let lines = vec![
            line("  ", 0., 0., 10., 10.),
            ("faint".into(), 0.1, (0., 50., 10., 60.)),
            line("go to cnn and click it", 0., 100., 10., 110.),
            line("  Headline  ", 0., 200., 100., 230.),
        ];
        let items = filter_and_block(lines, "go to cnn and click it", &[], 255);
        assert_eq!(texts(&items), vec!["Headline"]);
    }

    // ------------------------------------------------ merging sources

    #[test]
    fn merge_folds_an_overlapping_control_onto_the_ocr_block_that_names_it() {
        let block = ocr_item(0, "Register Now", 100., 100., 300., 130.);
        let mut control = ax_item(0, "Register Now for Disrupt", 110., 102., 290., 128.);
        control.role = "link".into();
        let merged = merge_sources_default(&[block], &[control]);
        assert_eq!(merged.len(), 1);
        let m = &merged[0];
        assert_eq!((m.source.as_str(), m.role.as_str()), ("ax+ocr", "link"));
        assert_eq!(m.text, "Register Now for Disrupt");
        assert_eq!((m.x1, m.y1, m.x2, m.y2), (100., 100., 300., 130.));
    }

    #[test]
    fn merge_matches_on_shared_words_not_only_containment() {
        let block = ocr_item(0, "Buy tickets now", 100., 100., 300., 130.);
        let control = ax_item(0, "Buy tickets", 100., 100., 300., 130.);
        assert_eq!(
            sources(&merge_sources_default(&[block], &[control])),
            vec!["ax+ocr"]
        );
    }

    #[test]
    fn merge_keeps_both_when_the_boxes_overlap_but_the_text_does_not_agree() {
        let block = ocr_item(0, "Search the docs", 100., 100., 300., 130.);
        let control = ax_item(0, "Clear input", 100., 100., 300., 130.);
        assert_eq!(
            sources(&merge_sources_default(&[block], &[control])),
            vec!["ax", "ocr"]
        );
    }

    #[test]
    fn merge_keeps_both_when_the_text_agrees_but_the_boxes_are_apart() {
        let block = ocr_item(0, "Share", 100., 100., 200., 130.);
        let control = ax_item(0, "Share", 900., 600., 960., 630.);
        assert_eq!(
            sources(&merge_sources_default(&[block], &[control])),
            vec!["ax", "ocr"]
        );
    }

    #[test]
    fn merge_consumes_each_ocr_block_at_most_once() {
        let block = ocr_item(0, "Send", 100., 100., 200., 130.);
        let controls = vec![
            ax_item(0, "Send", 100., 100., 200., 130.),
            ax_item(1, "Send", 104., 104., 196., 126.),
        ];
        assert_eq!(
            sources(&merge_sources_default(&[block], &controls)),
            vec!["ax", "ax+ocr"]
        );
    }

    #[test]
    fn merge_numbers_everything_in_reading_order() {
        let blocks = vec![
            ocr_item(0, "below", 100., 300., 200., 330.),
            ocr_item(1, "right", 800., 100., 900., 130.),
        ];
        let controls = vec![ax_item(0, "left", 100., 105., 200., 135.)];
        let got: Vec<(usize, String)> = merge_sources_default(&blocks, &controls)
            .into_iter()
            .map(|i| (i.index, i.text))
            .collect();
        assert_eq!(
            got,
            vec![(0, "left".into()), (1, "right".into()), (2, "below".into())]
        );
    }

    #[test]
    fn merge_pairs_each_item_with_the_control_it_came_from() {
        let blocks = vec![ocr_item(0, "Send", 100., 300., 200., 330.)];
        let controls = vec![
            ax_item(0, "Menu", 100., 100., 200., 130.),
            ax_item(1, "Send", 100., 300., 200., 330.),
        ];
        let got: Vec<(String, Option<usize>)> = merge_with_origins(&blocks, &controls, 255)
            .into_iter()
            .map(|(i, o)| (i.text, o))
            .collect();
        assert_eq!(
            got,
            vec![("Menu".into(), Some(0)), ("Send".into(), Some(1))]
        );
    }

    #[test]
    fn budget_drops_the_faintest_ocr_blocks_before_any_control() {
        let blocks: Vec<Item> = (0..3)
            .map(|i| {
                let y = 100. + 40. * i as f64;
                item(
                    i,
                    &format!("text {i}"),
                    (100., y, 200., y + 30.),
                    0.3 + 0.1 * i as f64,
                    "",
                    "ocr",
                )
            })
            .collect();
        let controls = vec![ax_item(0, "Send", 800., 100., 900., 130.)];
        let mut kept = texts(&merge_sources(&blocks, &controls, 2));
        kept.sort();
        assert_eq!(kept, vec!["Send", "text 2"]);
    }

    #[test]
    fn budget_falls_back_to_dropping_controls_when_only_controls_remain() {
        let controls: Vec<Item> = (0..4)
            .map(|i| {
                let y = 100. + 40. * i as f64;
                ax_item(i, &format!("control {i}"), 100., y, 200., y + 30.)
            })
            .collect();
        assert_eq!(merge_sources(&[], &controls, 2).len(), 2);
    }

    // ------------------------------------------------ coordinates

    #[test]
    fn the_read_region_is_measured_from_the_display_being_captured() {
        let mut base = blank_screen(2560, 1440);
        base.window = Some((2600., 200., 1200., 800.));
        let on_primary = ocr_region(&base);
        base.origin = (2560., 157.);
        let own = ocr_region(&base);
        assert_ne!(own, on_primary);
        let (l, t, r, b) = own;
        assert!(0. <= l && l < r && r <= 2560. && 0. <= t && t < b && b <= 1440.);
        assert!(
            r - l < 2560.,
            "the crop should be the window, not the whole screen"
        );
    }

    #[test]
    fn a_window_on_the_captured_display_is_unaffected_by_the_offset() {
        let mut primary = blank_screen(2560, 1440);
        primary.window = Some((100., 100., 800., 600.));
        assert_eq!(ocr_region(&primary), (92., 0., 908., 708.));
    }

    fn node(label: &str, x: f64, y: f64, w: f64, h: f64) -> Hit<()> {
        Hit {
            role: "AXButton".into(),
            label: label.into(),
            x,
            y,
            w,
            h,
            pressable: true,
            handle: (),
        }
    }

    #[test]
    fn a_control_is_placed_against_the_display_being_captured_not_the_desktop() {
        let n = [node("File", 44., -1440., 46., 44.)];
        let on_primary = &to_ax_items(&n, 1.0, (0., 0.))[0];
        let own = &to_ax_items(&n, 1.0, (1., -1440.))[0];
        assert_eq!(on_primary.y1, -1440.);
        assert_eq!((own.x1, own.y1, own.y2), (43., 0., 44.));
        assert_eq!(own.role, "button");
    }

    #[test]
    fn a_click_point_is_not_offset_twice() {
        let origin = (2560., 157.);
        let it = to_ax_items(&[node("Send", 2600., 200., 40., 20.)], 1.0, origin).remove(0);
        let mut s = blank_screen(2560, 1440);
        s.origin = origin;
        let (x, y) = s.to_points(&it);
        assert_eq!((x.round(), y.round()), (2620., 210.));
    }

    #[test]
    fn offscreen_controls_dedupe_and_skip_what_is_visible() {
        let nodes = vec![
            node("Like", 0., 0., 1., 1.),
            node("Like", 5., 5., 1., 1.),
            node("Send", 0., 0., 1., 1.),
        ];
        let visible = [ocr_item(0, "Send", 0., 0., 1., 1.)];
        let out = offscreen_controls(nodes, &visible);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Like");
    }

    #[test]
    fn labelled_drops_unlabelled_and_caps_the_on_screen_list() {
        let walked = WalkedOf {
            on_screen: vec![
                node("a", 0., 0., 1., 1.),
                node("", 0., 0., 1., 1.),
                node("c", 0., 0., 1., 1.),
            ],
            off_screen: vec![node("", 0., 0., 1., 1.), node("d", 0., 0., 1., 1.)],
            capped: false,
        };
        let (on, off) = labelled(walked, 2);
        assert_eq!(on.len(), 1);
        assert_eq!(off.len(), 1);
    }

    // ------------------------------------------------ the change detector

    #[test]
    fn tiles_cover_the_region_with_short_edges() {
        let tiles = tiles_in((0., 0., 300., 260.), 256.);
        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[3], (256., 256., 300., 260.));
    }

    #[test]
    fn a_changed_tile_is_found_and_an_unchanged_one_is_not() {
        let (w, h) = (512u32, 256u32);
        let before = vec![0u8; (w * h * 4) as usize];
        let mut after = before.clone();
        for y in 0..256 {
            for x in 256..512 {
                let i = ((y * w + x) * 4) as usize;
                after[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let (a, b) = (thumbnail(&after, w, h, 8), thumbnail(&before, w, h, 8));
        assert_eq!((a.width, a.height), (64, 32));
        let tiles = tiles_in((0., 0., 512., 256.), 256.);
        assert_eq!(changed_tiles(&a, &b, &tiles, 8, TILE_DIFF), vec![tiles[1]]);
    }

    #[test]
    fn scattered_change_reads_as_separate_rectangles() {
        let changed = vec![(0., 0., 256., 256.), (2048., 1024., 2304., 1280.)];
        assert_eq!(tile_clusters(&changed, 256.).len(), 2);
        let rects = reocr_rects(&changed, (0., 0., 2560., 1440.), &[], 4, 256.);
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn a_rectangle_grows_past_a_line_it_would_cut() {
        let lines = vec![line("headline", 200., 10., 900., 40.)];
        let r = grown_for_lines((0., 0., 512., 512.), &lines, (0., 0., 2560., 1440.));
        assert_eq!(r, (0., 0., 900., 512.));
    }

    #[test]
    fn too_many_rectangles_are_merged_down_to_the_limit() {
        let changed: Vec<PixelBox> = (0..6)
            .map(|i| {
                let x = i as f64 * 1024.;
                (x, 0., x + 256., 256.)
            })
            .collect();
        let rects = reocr_rects(&changed, (0., 0., 8000., 1440.), &[], 4, 256.);
        assert!(rects.len() <= 4);
    }

    #[test]
    fn reocr_keeps_untouched_lines_and_takes_fresh_ones() {
        let previous = vec![
            line("old", 0., 0., 10., 10.),
            line("moved", 500., 500., 510., 510.),
        ];
        let fresh = vec![line("new", 500., 500., 520., 510.)];
        let got = merge_reocr(&previous, fresh, &[(400., 400., 600., 600.)]);
        assert_eq!(
            got.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(),
            vec!["old", "new"]
        );
    }

    // ------------------------------------------------ the fake desktop

    struct Fake {
        monitors: Vec<Monitor>,
        windows: Vec<WindowInfo>,
        foreground: isize,
        ocr_calls: RefCell<Vec<(u32, u32)>>,
        lines: Vec<Line>,
        walked: RefCell<Vec<isize>>,
    }

    impl Desktop for Fake {
        fn monitors(&self) -> Vec<Monitor> {
            self.monitors.clone()
        }
        fn open_windows(&self) -> Vec<WindowInfo> {
            self.windows.clone()
        }
        fn foreground_hwnd(&self) -> isize {
            self.foreground
        }
        fn own_pid(&self) -> u32 {
            OWN
        }
        fn process_name(&self, pid: u32) -> String {
            if pid == OWN {
                "python".into()
            } else {
                "other".into()
            }
        }
        fn screenshot(&self, m: Option<&Monitor>) -> Option<Shot> {
            let origin = m.map(|m| (m.left, m.top)).unwrap_or((0, 0));
            Some(Shot {
                width: 640,
                height: 480,
                bgra: vec![0; 640 * 480 * 4],
                origin,
            })
        }
        fn focused_field(&self) -> Option<Field> {
            None
        }
        fn browser_url(&self, _: isize, _: (f64, f64), _: (f64, f64)) -> Option<String> {
            Some("https://example.com".into())
        }
        fn ocr(&self, _: &[u8], w: u32, h: u32) -> Vec<Line> {
            self.ocr_calls.borrow_mut().push((w, h));
            self.lines.clone()
        }
        fn walk(&self, hwnd: isize, _: f64, _: f64, _: (f64, f64)) -> Walked {
            self.walked.borrow_mut().push(hwnd);
            WalkedOf {
                on_screen: Vec::new(),
                off_screen: Vec::new(),
                capped: false,
            }
        }
    }

    fn monitor(index: usize, left: i32, top: i32, right: i32, bottom: i32) -> Monitor {
        Monitor {
            index,
            left,
            top,
            right,
            bottom,
            primary: index == 0,
        }
    }

    fn window(hwnd: isize, app: &str, pid: u32, monitor: usize, foreground: bool) -> WindowInfo {
        WindowInfo {
            hwnd,
            title: format!("{app} window"),
            app: app.into(),
            pid,
            rect: (0, 0, 800, 600),
            monitor,
            minimized: false,
            foreground,
        }
    }

    fn fake(monitors: Vec<Monitor>, windows: Vec<WindowInfo>, foreground: isize) -> Fake {
        Fake {
            monitors,
            windows,
            foreground,
            ocr_calls: RefCell::new(Vec::new()),
            lines: Vec::new(),
            walked: RefCell::new(Vec::new()),
        }
    }

    fn two_screens() -> Vec<Monitor> {
        vec![
            monitor(0, 0, 0, 1920, 1080),
            monitor(1, 1920, 0, 4480, 1440),
        ]
    }

    // Our own window has the foreground: its handle (77) is not in the inventory.
    #[test]
    fn survey_falls_back_to_another_window_display_when_the_foreground_is_ours() {
        let d = fake(two_screens(), vec![window(2, "chrome", 4242, 1, false)], 77);
        let s = survey(&d);
        assert_eq!((s.which, s.origin), (1, (1920., 0.)));
    }

    #[test]
    fn capture_reports_the_fallback_window_app_and_pid_when_the_foreground_is_ours() {
        let d = fake(
            vec![monitor(0, 0, 0, 1920, 1080)],
            vec![window(2, "chrome", 4242, 0, false)],
            77,
        );
        let screen = capture(&d, "", None).unwrap();
        assert_eq!((screen.app.as_str(), screen.pid), ("chrome", Some(4242)));
    }

    #[test]
    fn our_own_foreground_with_no_other_window_keeps_todays_behaviour() {
        let d = fake(two_screens(), vec![], 77);
        let s = survey(&d);
        assert_eq!((s.which, s.origin), (0, (0., 0.)));
        let screen = capture(&d, "", None).unwrap();
        assert_eq!((screen.app.as_str(), screen.pid), ("python", Some(OWN)));
    }

    #[test]
    fn capture_leaves_a_foreign_foreground_window_alone() {
        let d = fake(
            two_screens(),
            vec![
                window(2, "chrome", 4242, 1, true),
                window(3, "code", 99, 0, false),
            ],
            2,
        );
        assert_eq!(survey(&d).which, 1);
        let screen = capture(&d, "chrome", None).unwrap();
        assert_eq!((screen.app.as_str(), screen.pid), ("chrome", Some(4242)));
        assert_eq!(screen.origin, (1920., 0.));
        assert_eq!(screen.url.as_deref(), Some("https://example.com"));
        assert_eq!(screen.window, Some((0., 0., 800., 600.)));
    }

    #[test]
    fn a_minimized_foreground_is_placed_on_its_real_display() {
        let mut w = window(2, "notepad", 5, 1, true);
        w.minimized = true;
        w.rect = (-32000, -32000, -31840, -31972);
        let d = fake(two_screens(), vec![w], 2);
        let screen = capture(&d, "", None).unwrap();
        assert_eq!(screen.monitor, 1);
        assert_eq!(
            screen.window, None,
            "a parked rectangle is not a window to crop to"
        );
    }

    #[test]
    fn the_url_is_read_only_for_the_browser_asked_about() {
        let d = fake(two_screens(), vec![window(3, "code", 99, 0, true)], 3);
        assert_eq!(capture(&d, "chrome", None).unwrap().url, None);
    }

    #[test]
    fn the_survey_uses_the_actual_foreground_handle_not_a_stale_flag() {
        // VS Code's record still says foreground, but Windows says Chrome (hwnd 2) is in front.
        let d = fake(
            two_screens(),
            vec![
                window(3, "code", 99, 0, true),
                window(2, "chrome", 4242, 1, false),
            ],
            2,
        );
        let s = survey(&d);
        assert_eq!(s.target.map(|w| w.hwnd), Some(2));
        assert_eq!(s.which, 1);
    }

    // The reported Python bug: asked for Chrome, foreground_hwnd_of could hand back VS Code.
    #[test]
    fn window_selection_never_picks_another_process_window() {
        let inventory = vec![
            window(1, "code", 99, 0, true),
            window(2, "chrome", 4242, 1, false),
            window(3, "chrome", 4242, 1, false),
        ];
        // Chrome window 3 really is in front: that one, not the first Chrome record.
        assert_eq!(foreground_hwnd_of(3, &inventory, Some(4242), OWN), Some(3));
        // VS Code is in front but the step is about Chrome (a stand-in): a Chrome window, never 1.
        assert_eq!(foreground_hwnd_of(1, &inventory, Some(4242), OWN), Some(2));
        // No Chrome window in the inventory at all: no walk rather than a walk of VS Code.
        assert_eq!(
            foreground_hwnd_of(1, &inventory[..1], Some(4242), OWN),
            None
        );
        assert_eq!(foreground_hwnd_of(3, &inventory, None, OWN), None);
        // Our own window in front, being read because nothing else is open.
        assert_eq!(foreground_hwnd_of(77, &inventory, Some(OWN), OWN), Some(77));
    }

    #[test]
    fn perceive_walks_only_the_foreground_window() {
        let inventory = vec![
            window(2, "chrome", 4242, 0, false),
            window(3, "chrome", 4242, 0, true),
        ];
        let mut d = fake(vec![monitor(0, 0, 0, 640, 480)], inventory, 3);
        d.lines = vec![line("Hello world", 10., 10., 100., 30.)];
        let mut screen = capture(&d, "", None).unwrap();
        let mut timing = Timing::new();
        let items = perceive(&d, &mut screen, 255, "a goal", Some(&mut timing), None, &[]);
        assert_eq!(*d.walked.borrow(), vec![3]);
        assert_eq!(texts(&items), vec!["Hello world"]);
        assert!(timing.contains("ocr") && timing.contains("ax"));
        assert!(screen.ax_refs.is_empty() && screen.offscreen.is_empty());
    }

    #[test]
    fn the_cache_skips_the_engine_when_nothing_changed() {
        let mut d = fake(
            vec![monitor(0, 0, 0, 640, 480)],
            vec![window(2, "x", 5, 0, true)],
            2,
        );
        d.lines = vec![line("Static", 10., 10., 100., 30.)];
        let screen = capture(&d, "", None).unwrap();
        let mut cache = OcrCache::new();
        let (first, pct, _) = ocr_lines(&d, &screen, Some(&mut cache));
        assert!(pct > 0.0);
        let calls = d.ocr_calls.borrow().len();
        let (second, pct2, rects) = ocr_lines(&d, &screen, Some(&mut cache));
        assert_eq!(d.ocr_calls.borrow().len(), calls, "no new OCR call");
        assert_eq!((pct2, rects), (0.0, 0));
        assert_eq!(first, second);
    }

    #[test]
    fn a_crop_reports_boxes_in_full_capture_pixels() {
        let mut d = fake(vec![], vec![], 0);
        d.lines = vec![line("x", 1., 2., 3., 4.)];
        let screen = blank_screen(640, 480);
        let got = ocr_crop(&d, &screen, (100., 50., 300., 200.));
        assert_eq!(got[0].2, (101., 52., 103., 54.));
        assert_eq!(d.ocr_calls.borrow()[0], (200, 150));
    }

    #[test]
    fn url_comes_from_the_omnibox_or_an_address_value() {
        let f = |r: &str, l: &str, v: &str| (r.to_string(), l.to_string(), v.to_string());
        assert_eq!(
            url_from_fields(&[
                f("AXTextField", "Search", "cats"),
                f("AXTextField", "Address and search bar", "example.com/a")
            ]),
            Some("https://example.com/a".into())
        );
        assert_eq!(
            url_from_fields(&[f("AXTextArea", "Page", "https://x.org/")]),
            Some("https://x.org/".into())
        );
        assert_eq!(url_from_fields(&[f("AXTextField", "Search", "cats")]), None);
    }

    #[test]
    fn app_matching_is_loose() {
        assert!(app_matches("Google Chrome", "chrome", ""));
        assert!(app_matches("chrome", "chrome", ""));
        assert!(!app_matches(
            "chrome",
            "Code",
            "perception.rs - Visual Studio Code"
        ));
    }

    #[test]
    fn near_field_finds_items_around_the_focused_field() {
        let mut s = blank_screen(2560, 1440);
        s.origin = (100., 0.);
        s.field = Some(Field {
            role: "AXTextField".into(),
            label: String::new(),
            placeholder: String::new(),
            value: String::new(),
            x: 200.,
            y: 100.,
            w: 100.,
            h: 20.,
        });
        let items = [
            ocr_item(0, "near", 100., 100., 200., 120.),
            ocr_item(1, "far", 2000., 1000., 2100., 1020.),
        ];
        assert_eq!(near_field(&s, &items, 160.), vec!["near"]);
    }

    // ------------------------------------------------ live

    /// Reads the real desktop and prints counts. Never clicks or types.
    #[test]
    #[ignore]
    fn live_capture_prints_item_counts() {
        let d = LiveDesktop;
        let mut timing = Timing::new();
        let mut screen = capture(&d, "chrome", Some(&mut timing)).expect("capture");
        let items = perceive(
            &d,
            &mut screen,
            MAX_OPTIONS,
            "",
            Some(&mut timing),
            None,
            &[],
        );
        let ax = items.iter().filter(|i| i.source == "ax").count();
        let both = items.iter().filter(|i| i.source == "ax+ocr").count();
        let ocr = items.iter().filter(|i| i.source == "ocr").count();
        println!(
            "app={} pid={:?} monitor={} origin={:?} size={}x{} window={:?} url={:?}",
            screen.app,
            screen.pid,
            screen.monitor,
            screen.origin,
            screen.width,
            screen.height,
            screen.window,
            screen.url
        );
        println!(
            "items={} ocr={} ax={} ax+ocr={} ax_refs={} offscreen={} windows={}",
            items.len(),
            ocr,
            ax,
            both,
            screen.ax_refs.len(),
            screen.offscreen.len(),
            screen.windows.len()
        );
        println!("timing={:?}", crate::timing::ordered(&timing));
    }
}
