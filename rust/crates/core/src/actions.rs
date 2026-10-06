//! Execute one decided action. Every function returns a one-line description for the history.
//!
//! A port of `typesafe_computer_use_win/actions.py`. The descriptions are the Python strings, byte
//! for byte, because the loop and the run logs read them: a description containing "refused",
//! "failed" or "waited" is a step that achieved nothing ([`is_noop`]).
//!
//! Every effect on the machine goes through [`Desktop`], so the tests run against a recorder and
//! never touch the user's apps. [`RealDesktop`] is the one implementation that does. The writer and
//! the typing verifier are traits for the same reason: a test never pays for a model call.

use std::collections::HashMap;

use platform::uia::{Hit, PressHandle};
use platform::winlist::WindowInfo;

use crate::apps::App;
use crate::catalog::Site;
use crate::config;
use crate::models::{Field, Item, Screen};
use crate::sitepick::{app_for, url_for};

pub const VERIFY_THRESHOLD: f64 = 0.5;
pub const NOOP_MARKERS: [&str; 3] = ["refused", "failed", "waited"];

/// Same values as `decide::OFFSCREEN_PREFIX` and friends; repeated here so this module does not
/// depend on the decision request's internals.
pub const OFFSCREEN_PREFIX: &str = "offscreen:";
pub const WINDOW_PREFIX: &str = "window:";
pub const APP_PREFIX: &str = "app:";

/// How long a switch waits for the foreground to arrive, as the Python `timeout=3.0`.
pub const ACTIVATE_TIMEOUT_MS: u64 = 3000;

// ---------------------------------------------------------------- the seams

/// The keys an action presses, with the virtual-key codes `windows.VK` maps them to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Return,
    Tab,
    Escape,
    A,
    Delete,
    Back,
}

impl Key {
    pub fn vk(self) -> u16 {
        match self {
            Key::Return => 0x0D,
            Key::Tab => 0x09,
            Key::Escape => 0x1B,
            Key::A => 0x41,
            Key::Delete => 0x2E,
            Key::Back => 0x08,
        }
    }
}

/// Every trip to the machine an action makes.
///
/// `Handle` is the accessibility element type: [`PressHandle`] for the real desktop, anything at
/// all for a test, which is what lets a test press "an element" without COM.
pub trait Desktop {
    type Handle;

    /// Activate an element through the accessibility tree.
    fn ax_press(&self, handle: &Self::Handle) -> bool;
    fn ax_focus(&self, handle: &Self::Handle) -> bool;
    fn ax_set_value(&self, handle: &Self::Handle, text: &str) -> bool;
    fn ax_value(&self, handle: &Self::Handle) -> Option<String>;
    /// The element holding the keyboard focus right now, to write a value into.
    fn focused_handle(&self) -> Option<Self::Handle>;
    /// The focused element as a field record, for the read-back after typing.
    fn focused_field(&self) -> Option<Field>;

    /// A left click at a virtual-desktop point.
    fn click_at(&self, point: (f64, f64));
    fn type_text(&self, text: &str);
    /// Tap a key, with Control held when `ctrl`.
    fn press(&self, key: Key, ctrl: bool);
    /// Wheel lines; positive scrolls up.
    fn scroll(&self, lines: i32);

    /// Bring one window forward and confirm the foreground moved there.
    fn activate_window(&self, hwnd: isize) -> bool;
    /// The foreground window's handle, 0 when none.
    fn foreground_window(&self) -> isize;
    /// Bring an app, named loosely ("Google Chrome", "chrome"), to the front.
    fn activate(&self, app: &str) -> bool;
    /// Open a page in the named browser and bring it to the front.
    fn open_url(&self, browser: &str, url: &str) -> bool;
    /// Start an installed application from the catalog.
    fn launch(&self, app: &App) -> bool;
    /// Keep a writer-resolved site in the learned catalog. Best effort: must not fail.
    fn remember_site(&self, url: &str);
    /// Wait, for an effect that is not instant.
    fn pause(&self, seconds: f64);
}

/// The model that proposes a URL or a field's text. `Err` carries the reason it is unavailable,
/// already in words (`writer::problem`), and reads as a refusal rather than a crash.
pub trait Writer {
    fn compose_url(&self, goal: &str, history: &[String]) -> Result<String, String>;
    fn compose_text(
        &self,
        goal: &str,
        screen: &Screen,
        items: &[Item],
        history: &[String],
    ) -> Result<String, String>;
}

/// The classifier that checks typed text landed where the goal meant it to: a probability.
pub trait Verifier {
    fn verify_typed(&self, goal: &str, before: &Field, typed: &str, after: Option<&Field>) -> f64;
}

/// What one step's actions need beyond the screen.
pub struct Context<'a> {
    pub goal: String,
    pub browser: String,
    pub email: Option<String>,
    pub writer: Option<&'a dyn Writer>,
    pub verifier: &'a dyn Verifier,
    pub history: Vec<String>,
    /// The per-goal shortlist; empty falls back to the pinned catalog.
    pub sites: Vec<Site>,
    /// The installed applications worth offering for this goal.
    pub apps: Vec<App>,
}

/// The parts of a screen an action reads, with the accessibility handles separable from the
/// capture. [`View::of`] borrows a real screen's own handles; a test supplies its own.
pub struct View<'a, H> {
    pub screen: &'a Screen,
    pub ax_refs: &'a HashMap<usize, H>,
    pub offscreen: &'a [Hit<H>],
}

impl<'a> View<'a, PressHandle> {
    pub fn of(screen: &'a Screen) -> Self {
        View {
            screen,
            ax_refs: &screen.ax_refs,
            offscreen: &screen.offscreen,
        }
    }
}

/// `perform` was asked for an action it does not know: the Python `ValueError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAction(pub String);

impl std::fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown action {}", py_repr(&self.0))
    }
}

impl std::error::Error for UnknownAction {}

// ---------------------------------------------------------------- the actions

pub fn is_noop(description: &str) -> bool {
    NOOP_MARKERS.iter().any(|m| description.contains(m))
}

/// Keep a writer-resolved site, so the next run finds it by name instead of paying for a model.
pub fn remember<D: Desktop>(desktop: &D, url: &str) {
    if !url.is_empty() {
        desktop.remember_site(url);
    }
}

/// Run one decision. `chosen` is `Decision::chosen()`, `site` is the site answer's choice.
pub fn perform<D: Desktop>(
    desktop: &D,
    chosen: &str,
    site: &str,
    view: &View<D::Handle>,
    items: &[Item],
    ctx: &Context,
) -> Result<String, UnknownAction> {
    if let Some(item) = items.iter().find(|it| it.index.to_string() == chosen) {
        return Ok(click_item(desktop, item, view));
    }
    if let Some(key) = chosen.strip_prefix(OFFSCREEN_PREFIX) {
        return Ok(press_offscreen(desktop, key, view.offscreen));
    }
    if let Some(key) = chosen.strip_prefix(WINDOW_PREFIX) {
        return Ok(switch_window(desktop, key, &view.screen.windows));
    }
    if let Some(key) = chosen.strip_prefix(APP_PREFIX) {
        return Ok(open_app(desktop, key, ctx));
    }
    let screen = view.screen;
    Ok(match chosen {
        "use_browser" => use_browser(desktop, site, ctx),
        "type_email" => type_email(desktop, screen, ctx),
        "type_text" => type_text(desktop, screen, items, ctx),
        "press_enter" => press_key(desktop, Key::Return, "pressed Return"),
        "press_escape" => press_key(desktop, Key::Escape, "pressed Escape"),
        "scroll_down" => scroll(desktop, -10, "scrolled down"),
        "scroll_up" => scroll(desktop, 10, "scrolled up"),
        "wait" => "waited".to_string(),
        other => return Err(UnknownAction(other.to_string())),
    })
}

/// Press an item the app declared through the accessibility tree; click the pixel under it
/// otherwise.
///
/// A press goes to the control itself, so it lands even when the center of the box is covered by
/// a sticky header, a cookie banner, or a tooltip. An element that refuses still has a location.
pub fn click_item<D: Desktop>(desktop: &D, item: &Item, view: &View<D::Handle>) -> String {
    let handle = view.ax_refs.get(&item.index);
    if let Some(h) = handle {
        if desktop.ax_press(h) {
            return format!("pressed {} via accessibility", py_repr(&item.text));
        }
    }
    desktop.click_at(view.screen.to_points(item));
    match handle {
        None => format!("clicked {}", py_repr(&item.text)),
        Some(_) => format!(
            "clicked {} (accessibility press did not take)",
            py_repr(&item.text)
        ),
    }
}

/// Press a control the app exposes but does not show. There is no pixel to fall back on, so a
/// refusal is the end of it and reads as a no-op.
pub fn press_offscreen<D: Desktop>(desktop: &D, key: &str, nodes: &[Hit<D::Handle>]) -> String {
    let node = if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
        key.parse::<usize>().ok().and_then(|i| nodes.get(i))
    } else {
        None
    };
    let Some(node) = node else {
        return format!(
            "press_offscreen refused: there is no off-screen control {}",
            py_repr(key)
        );
    };
    if desktop.ax_press(&node.handle) {
        return format!(
            "pressed {} (off-screen control) via accessibility",
            py_repr(&node.label)
        );
    }
    format!(
        "press_offscreen refused: {} did not accept the press",
        py_repr(&node.label)
    )
}

/// Bring an already-open window to the front, wherever it is.
///
/// Switching to the window that is already in front is refused: it changes nothing, so the loop
/// would pick the same window again forever.
pub fn switch_window<D: Desktop>(desktop: &D, key: &str, windows: &[WindowInfo]) -> String {
    let window = key.trim().parse::<i64>().ok().and_then(|i| {
        // Python indexing: a negative index counts from the end.
        let len = windows.len() as i64;
        let i = if i < 0 { i + len } else { i };
        if (0..len).contains(&i) {
            windows.get(i as usize)
        } else {
            None
        }
    });
    let Some(window) = window else {
        return format!(
            "switch_window failed: no window {} is open any more",
            py_repr(key)
        );
    };
    let title = py_repr(&window.title.chars().take(60).collect::<String>());
    if already_in_front(desktop, window) {
        return format!(
            "switch_window refused: {} {title} on monitor {} was already in front",
            window.app,
            window.monitor + 1
        );
    }
    if desktop.activate_window(window.hwnd) {
        return format!(
            "switched to {} {title} on monitor {}",
            window.app,
            window.monitor + 1
        );
    }
    format!(
        "switch_window failed: {} did not come to the front",
        window.app
    )
}

/// Whether this window is the one the user is looking at: the inventory's flag, or the live
/// foreground when the flag is stale.
fn already_in_front<D: Desktop>(desktop: &D, window: &WindowInfo) -> bool {
    window.foreground || desktop.foreground_window() == window.hwnd
}

/// Launch an installed application by the key the classifier answered with. The key indexes the
/// catalog, so there is no way for this to run something that is not installed.
pub fn open_app<D: Desktop>(desktop: &D, key: &str, ctx: &Context) -> String {
    let Some(app) = app_for(&ctx.apps, key) else {
        return format!(
            "open_app failed: {} is not one of the applications offered",
            py_repr(key)
        );
    };
    if desktop.launch(app) {
        desktop.pause(1.0); // a launch is not instant, and the next step reads the screen
        return format!("launched {}", app.label);
    }
    format!("open_app failed: {} did not start", app.label)
}

/// Put text in the focused field, by value if the element accepts one and keystrokes otherwise.
/// Only a field that reads the text back counts. Returns which path ran, for the history.
pub fn fill_field<D: Desktop>(desktop: &D, handle: Option<&D::Handle>, text: &str) -> &'static str {
    if let Some(h) = handle {
        desktop.ax_focus(h);
        if desktop.ax_set_value(h, text) {
            if let Some(back) = desktop.ax_value(h) {
                if back.ends_with(text) {
                    return "via accessibility";
                }
            }
        }
    }
    desktop.type_text(text);
    "via keystrokes"
}

/// Go to the browser, and open the website the site answer named. `none` is the page already open
/// there; a catalog key is its URL; `other` is a site only the writer can name.
pub fn use_browser<D: Desktop>(desktop: &D, site: &str, ctx: &Context) -> String {
    if site == "none" {
        if desktop.activate(&ctx.browser) {
            return format!("activated {}", ctx.browser);
        }
        return format!(
            "use_browser failed: {} did not come to the front",
            ctx.browser
        );
    }
    let known = if ctx.sites.is_empty() {
        config::SITES
            .iter()
            .find(|(k, _)| *k == site)
            .map(|(_, u)| u.to_string())
    } else {
        url_for(&ctx.sites, site)
    };
    let url = match known {
        Some(url) => url,
        None => {
            let Some(writer) = ctx.writer else {
                return "use_browser refused: the site is outside the catalog and no writer is available to propose a URL".to_string();
            };
            match writer.compose_url(&ctx.goal, &ctx.history) {
                Ok(url) => {
                    remember(desktop, &url); // resolved once by a model, a lookup after this
                    url
                }
                Err(e) => {
                    return format!("use_browser refused: the writer could not propose a URL ({e})")
                }
            }
        }
    };
    if url.is_empty() {
        return "use_browser refused: the writer proposed no usable URL for this goal".to_string();
    }
    if desktop.open_url(&ctx.browser, &url) {
        return format!("opened {url}");
    }
    format!(
        "use_browser failed: opened {url} but {} did not come to the front",
        ctx.browser
    )
}

fn text_field(screen: &Screen) -> Option<&Field> {
    screen.field.as_ref().filter(|f| f.is_text())
}

pub fn type_email<D: Desktop>(desktop: &D, screen: &Screen, ctx: &Context) -> String {
    if text_field(screen).is_none() {
        return "type_email refused: no text field is focused".to_string();
    }
    let handle = desktop.focused_handle();
    let how = fill_field(desktop, handle.as_ref(), ctx.email.as_deref().unwrap_or(""));
    format!("typed email {how}")
}

pub fn type_text<D: Desktop>(
    desktop: &D,
    screen: &Screen,
    items: &[Item],
    ctx: &Context,
) -> String {
    let Some(field) = text_field(screen) else {
        return "type_text refused: no text field is focused".to_string();
    };
    let Some(writer) = ctx.writer else {
        return "type_text refused: no writer available".to_string();
    };
    let text = match writer.compose_text(&ctx.goal, screen, items, &ctx.history) {
        Ok(t) => t,
        Err(e) => return format!("type_text refused: the writer is unavailable ({e})"),
    };
    if text.is_empty() {
        return "type_text refused: writer declined to fill this field".to_string();
    }
    let handle = desktop.focused_handle();
    let how = fill_field(desktop, handle.as_ref(), &text);
    desktop.pause(0.3);
    let after = desktop.focused_field();
    let p = ctx
        .verifier
        .verify_typed(&ctx.goal, field, &text, after.as_ref());
    let (typed, label) = (py_repr(&text), py_repr(&field.label));
    if p < VERIFY_THRESHOLD {
        clear_field(desktop);
        return format!(
            "typed {typed} into {label} {how} but verification failed ({p:.2}); cleared it"
        );
    }
    format!("typed {typed} into {label} {how} (verified {p:.2})")
}

/// Select everything in the focused field and delete it.
pub fn clear_field<D: Desktop>(desktop: &D) {
    desktop.press(Key::A, true);
    desktop.press(Key::Delete, false);
}

fn press_key<D: Desktop>(desktop: &D, key: Key, description: &str) -> String {
    desktop.press(key, false);
    description.to_string()
}

fn scroll<D: Desktop>(desktop: &D, lines: i32, description: &str) -> String {
    desktop.scroll(lines);
    description.to_string()
}

/// Python's `repr` of a string, which is how the history quotes text: single quotes unless the
/// text holds one and no double quote.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

// ---------------------------------------------------------------- the real desktop

/// The desktop itself: synthetic input, UI Automation, the window list and the shell.
///
/// Not `Send` in practice: the handles it presses are COM pointers bound to the thread that
/// fetched them, so use it on the thread that captured the screen.
pub struct RealDesktop;

/// The `windows._app_matches` rule: an app is named loosely, and a process name or title must hit.
fn app_matches(app: &str, process: &str, title: &str) -> bool {
    let squash = |s: &str| s.to_lowercase().replace(' ', "");
    let (wanted, name, title) = (squash(app), squash(process), squash(title));
    !name.is_empty()
        && (wanted.contains(&name) || name.contains(&wanted) || title.contains(&wanted))
}

fn browser_executable(browser: &str) -> String {
    match browser.to_lowercase().as_str() {
        "google chrome" => "chrome".to_string(),
        "microsoft edge" => "msedge".to_string(),
        "firefox" => "firefox".to_string(),
        _ => browser.to_string(),
    }
}

impl Desktop for RealDesktop {
    type Handle = PressHandle;

    fn ax_press(&self, handle: &PressHandle) -> bool {
        platform::uia::press(handle)
    }

    fn ax_focus(&self, handle: &PressHandle) -> bool {
        platform::uia::focus(handle)
    }

    fn ax_set_value(&self, handle: &PressHandle, text: &str) -> bool {
        platform::uia::set_value(handle, text)
    }

    fn ax_value(&self, handle: &PressHandle) -> Option<String> {
        platform::uia::value(handle)
    }

    fn focused_handle(&self) -> Option<PressHandle> {
        platform::uia::focused_field().map(|n| n.handle)
    }

    fn focused_field(&self) -> Option<Field> {
        let node = platform::uia::focused_field()?;
        Some(Field {
            value: platform::uia::value(&node.handle).unwrap_or_default(),
            role: node.role,
            label: node.label,
            placeholder: String::new(),
            x: node.x,
            y: node.y,
            w: node.w,
            h: node.h,
        })
    }

    fn click_at(&self, point: (f64, f64)) {
        platform::input::click_at(point.0.round() as i32, point.1.round() as i32);
    }

    fn type_text(&self, text: &str) {
        platform::input::type_text(text);
    }

    fn press(&self, key: Key, ctrl: bool) {
        platform::input::press(key.vk(), ctrl);
    }

    fn scroll(&self, lines: i32) {
        platform::input::scroll(lines);
    }

    fn activate_window(&self, hwnd: isize) -> bool {
        platform::winlist::activate_window(hwnd, ACTIVATE_TIMEOUT_MS)
    }

    fn foreground_window(&self) -> isize {
        platform::winlist::foreground()
    }

    fn activate(&self, app: &str) -> bool {
        let windows = platform::winlist::open_windows(platform::winlist::MIN_WINDOW_SIDE_PX);
        match windows
            .iter()
            .find(|w| !w.title.is_empty() && app_matches(app, &w.app, &w.title))
        {
            Some(w) => platform::winlist::activate_window(w.hwnd, ACTIVATE_TIMEOUT_MS),
            None => false,
        }
    }

    fn open_url(&self, browser: &str, url: &str) -> bool {
        let executable = browser_executable(browser);
        let started = std::process::Command::new(&executable)
            .arg(url)
            .spawn()
            .is_ok();
        if !started {
            // The default browser, as os.startfile(url) would.
            let _ = std::process::Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", url])
                .spawn();
        }
        self.pause(1.0);
        self.activate(browser) || self.activate(&executable)
    }

    fn launch(&self, app: &App) -> bool {
        crate::apps::launch(app)
    }

    fn remember_site(&self, url: &str) {
        // Best effort on purpose: a catalog that cannot be written must not fail the action that
        // just succeeded.
        let url = url.to_string();
        let _ = std::panic::catch_unwind(move || crate::catalog::remember_site("", &url));
    }

    fn pause(&self, seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0)));
    }
}

// ---------------------------------------------------------------- tests
//
// Every test runs against `Recorder`: nothing here clicks, types, raises a window or launches.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Press(u32),
        Focus(u32),
        SetValue(u32, String),
        Click((f64, f64)),
        Type(String),
        Key(Key, bool),
        Scroll(i32),
        ActivateWindow(isize),
        Activate(String),
        Open(String, String),
        Launch(String),
        Remember(String),
    }

    /// Every trip to the machine, recorded instead of made. Handles are plain numbers.
    struct Recorder {
        calls: RefCell<Vec<Call>>,
        press_ok: bool,
        set_ok: bool,
        /// What a value read returns: `None` echoes the last value written.
        read_back: Option<Option<String>>,
        focused: Option<u32>,
        foreground: isize,
        window_ok: bool,
        activate_ok: bool,
        open_ok: bool,
        launch_ok: bool,
        reads: Cell<usize>,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Recorder {
                calls: RefCell::new(Vec::new()),
                press_ok: true,
                set_ok: true,
                read_back: None,
                focused: None,
                foreground: 0,
                window_ok: true,
                activate_ok: true,
                open_ok: true,
                launch_ok: true,
                reads: Cell::new(0),
            }
        }
    }

    impl Recorder {
        fn log(&self, c: Call) {
            self.calls.borrow_mut().push(c);
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl Desktop for Recorder {
        type Handle = u32;
        fn ax_press(&self, h: &u32) -> bool {
            self.log(Call::Press(*h));
            self.press_ok
        }
        fn ax_focus(&self, h: &u32) -> bool {
            self.log(Call::Focus(*h));
            true
        }
        fn ax_set_value(&self, h: &u32, text: &str) -> bool {
            self.log(Call::SetValue(*h, text.into()));
            self.set_ok
        }
        fn ax_value(&self, _h: &u32) -> Option<String> {
            self.reads.set(self.reads.get() + 1);
            match &self.read_back {
                Some(v) => v.clone(),
                None => self.calls.borrow().iter().rev().find_map(|c| match c {
                    Call::SetValue(_, t) => Some(t.clone()),
                    _ => None,
                }),
            }
        }
        fn focused_handle(&self) -> Option<u32> {
            self.focused
        }
        fn focused_field(&self) -> Option<Field> {
            None
        }
        fn click_at(&self, p: (f64, f64)) {
            self.log(Call::Click(p));
        }
        fn type_text(&self, t: &str) {
            self.log(Call::Type(t.into()));
        }
        fn press(&self, k: Key, ctrl: bool) {
            self.log(Call::Key(k, ctrl));
        }
        fn scroll(&self, lines: i32) {
            self.log(Call::Scroll(lines));
        }
        fn activate_window(&self, hwnd: isize) -> bool {
            self.log(Call::ActivateWindow(hwnd));
            self.window_ok
        }
        fn foreground_window(&self) -> isize {
            self.foreground
        }
        fn activate(&self, app: &str) -> bool {
            self.log(Call::Activate(app.into()));
            self.activate_ok
        }
        fn open_url(&self, b: &str, u: &str) -> bool {
            self.log(Call::Open(b.into(), u.into()));
            self.open_ok
        }
        fn launch(&self, app: &App) -> bool {
            self.log(Call::Launch(app.label.clone()));
            self.launch_ok
        }
        fn remember_site(&self, url: &str) {
            self.log(Call::Remember(url.into()));
        }
        fn pause(&self, _s: f64) {}
    }

    struct FakeWriter {
        url: Result<String, String>,
        text: Result<String, String>,
        asked: RefCell<Vec<String>>,
    }

    impl FakeWriter {
        fn url(url: Result<&str, &str>) -> Self {
            FakeWriter {
                url: url.map(String::from).map_err(String::from),
                text: Ok(String::new()),
                asked: RefCell::new(Vec::new()),
            }
        }
        fn text(text: Result<&str, &str>) -> Self {
            FakeWriter {
                url: Err("not asked".into()),
                text: text.map(String::from).map_err(String::from),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Writer for FakeWriter {
        fn compose_url(&self, goal: &str, _h: &[String]) -> Result<String, String> {
            self.asked.borrow_mut().push(goal.into());
            self.url.clone()
        }
        fn compose_text(
            &self,
            goal: &str,
            _s: &Screen,
            _i: &[Item],
            _h: &[String],
        ) -> Result<String, String> {
            self.asked.borrow_mut().push(goal.into());
            self.text.clone()
        }
    }

    struct FixedVerifier(f64);
    impl Verifier for FixedVerifier {
        fn verify_typed(&self, _g: &str, _b: &Field, _t: &str, _a: Option<&Field>) -> f64 {
            self.0
        }
    }
    const VERIFIED: FixedVerifier = FixedVerifier(0.9);

    fn screen() -> Screen {
        Screen {
            bgra: Vec::new(),
            width: 2880,
            height: 1800,
            scale: 2.0,
            app: "Google Chrome".into(),
            field: None,
            url: None,
            pid: None,
            window: None,
            origin: (0.0, 0.0),
            monitor: 0,
            windows: Vec::new(),
            monitors: Vec::new(),
            ax_refs: Default::default(),
            offscreen: Vec::new(),
        }
    }

    fn item(source: &str) -> Item {
        Item {
            index: 3,
            text: "Register Now".into(),
            ocr_confidence: 1.0,
            x1: 100.0,
            y1: 100.0,
            x2: 300.0,
            y2: 140.0,
            role: if source == "ocr" {
                String::new()
            } else {
                "link".into()
            },
            source: source.into(),
        }
    }

    fn node(label: &str, y: f64, handle: u32) -> Hit<u32> {
        Hit {
            role: "AXLink".into(),
            label: label.into(),
            x: 0.0,
            y,
            w: 120.0,
            h: 32.0,
            pressable: true,
            handle,
        }
    }

    fn text_field(label: &str) -> Field {
        Field {
            role: "AXTextField".into(),
            label: label.into(),
            placeholder: String::new(),
            value: String::new(),
            x: 10.0,
            y: 20.0,
            w: 200.0,
            h: 30.0,
        }
    }

    fn context<'a>(writer: Option<&'a dyn Writer>) -> Context<'a> {
        Context {
            goal: "find the next upcoming bruno mars concert".into(),
            browser: "Google Chrome".into(),
            email: None,
            writer,
            verifier: &VERIFIED,
            history: Vec::new(),
            sites: Vec::new(),
            apps: Vec::new(),
        }
    }

    fn view<'a>(
        s: &'a Screen,
        refs: &'a HashMap<usize, u32>,
        off: &'a [Hit<u32>],
    ) -> View<'a, u32> {
        View {
            screen: s,
            ax_refs: refs,
            offscreen: off,
        }
    }

    fn browse(d: &Recorder, site: &str, ctx: &Context) -> String {
        let (s, refs) = (screen(), HashMap::new());
        perform(d, "use_browser", site, &view(&s, &refs, &[]), &[], ctx).unwrap()
    }

    // ------------------------------------------------------------ clicking and pressing

    #[test]
    fn an_item_from_the_accessibility_tree_is_pressed() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::from([(3, 42)]));
        assert_eq!(
            click_item(&d, &item("ax"), &view(&s, &refs, &[])),
            "pressed 'Register Now' via accessibility"
        );
        assert_eq!(d.calls(), [Call::Press(42)]);
    }

    #[test]
    fn a_refused_press_falls_back_to_the_mouse() {
        let d = Recorder {
            press_ok: false,
            ..Default::default()
        };
        let (s, refs) = (screen(), HashMap::from([(3, 42)]));
        assert_eq!(
            click_item(&d, &item("ax"), &view(&s, &refs, &[])),
            "clicked 'Register Now' (accessibility press did not take)"
        );
        assert_eq!(d.calls(), [Call::Press(42), Call::Click((100.0, 60.0))]);
    }

    #[test]
    fn an_ocr_only_item_is_clicked_without_asking_accessibility() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::new());
        assert_eq!(
            click_item(&d, &item("ocr"), &view(&s, &refs, &[])),
            "clicked 'Register Now'"
        );
        assert_eq!(d.calls(), [Call::Click((100.0, 60.0))]);
    }

    #[test]
    fn a_click_on_another_monitor_adds_its_origin_once() {
        let d = Recorder::default();
        let mut s = screen();
        s.origin = (1.0, -1440.0);
        let refs = HashMap::new();
        click_item(&d, &item("ocr"), &view(&s, &refs, &[]));
        assert_eq!(d.calls(), [Call::Click((101.0, -1380.0))]);
    }

    #[test]
    fn an_off_screen_control_is_pressed_through_accessibility() {
        let d = Recorder::default();
        let nodes = [node("Register Now", -4200.0, 7)];
        assert_eq!(
            press_offscreen(&d, "0", &nodes),
            "pressed 'Register Now' (off-screen control) via accessibility"
        );
        assert_eq!(d.calls(), [Call::Press(7)]);
    }

    #[test]
    fn a_refused_off_screen_press_is_a_no_op_with_nothing_to_click() {
        let d = Recorder {
            press_ok: false,
            ..Default::default()
        };
        let refusal = press_offscreen(&d, "0", &[node("Register Now", -4200.0, 7)]);
        assert_eq!(
            refusal,
            "press_offscreen refused: 'Register Now' did not accept the press"
        );
        assert!(is_noop(&refusal));
        assert_eq!(d.calls(), [Call::Press(7)]);
    }

    #[test]
    fn an_offscreen_key_that_names_nothing_is_refused() {
        let d = Recorder::default();
        let refusal = press_offscreen(&d, "4", &[]);
        assert_eq!(
            refusal,
            "press_offscreen refused: there is no off-screen control '4'"
        );
        assert!(is_noop(&refusal) && d.calls().is_empty());
        assert!(is_noop(&press_offscreen(&d, "-1", &[node("x", 0.0, 1)])));
        assert!(d.calls().is_empty());
    }

    #[test]
    fn perform_routes_an_offscreen_key_to_the_press() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::new());
        let nodes = [node("Note 900", 42718.0, 9)];
        let got = perform(
            &d,
            "offscreen:0",
            "none",
            &view(&s, &refs, &nodes),
            &[],
            &context(None),
        );
        assert_eq!(
            got.unwrap(),
            "pressed 'Note 900' (off-screen control) via accessibility"
        );
        assert_eq!(d.calls(), [Call::Press(9)]);
    }

    #[test]
    fn perform_routes_an_item_index_to_the_click() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::new());
        let got = perform(
            &d,
            "3",
            "none",
            &view(&s, &refs, &[]),
            &[item("ocr")],
            &context(None),
        );
        assert_eq!(got.unwrap(), "clicked 'Register Now'");
    }

    #[test]
    fn a_fallback_click_is_not_treated_as_a_no_op() {
        assert!(!is_noop(
            "clicked 'Register Now' (accessibility press did not take)"
        ));
    }

    // ------------------------------------------------------------ the browser

    #[test]
    fn use_browser_with_no_site_only_brings_the_browser_forward() {
        let d = Recorder::default();
        assert_eq!(
            browse(&d, "none", &context(None)),
            "activated Google Chrome"
        );
        assert_eq!(d.calls(), [Call::Activate("Google Chrome".into())]);
    }

    #[test]
    fn use_browser_opens_a_catalog_site_by_its_url() {
        let d = Recorder::default();
        let w = FakeWriter::url(Ok("https://wrong.example/"));
        assert_eq!(
            browse(&d, "github", &context(Some(&w))),
            "opened https://github.com/"
        );
        assert!(
            w.asked.borrow().is_empty(),
            "the catalog already names this site"
        );
        assert_eq!(
            d.calls(),
            [Call::Open(
                "Google Chrome".into(),
                "https://github.com/".into()
            )]
        );
    }

    #[test]
    fn use_browser_reads_the_shortlist_when_there_is_one() {
        let d = Recorder::default();
        let mut ctx = context(None);
        ctx.sites = vec![Site::new(
            "songkick",
            "Songkick",
            "https://www.songkick.com/",
            0.8,
            "learned",
        )];
        assert_eq!(
            browse(&d, "songkick", &ctx),
            "opened https://www.songkick.com/"
        );
        // The pinned catalog is not consulted once a shortlist exists.
        assert!(is_noop(&browse(&d, "github", &ctx)));
    }

    #[test]
    fn use_browser_asks_the_writer_for_a_site_outside_the_catalog() {
        let d = Recorder::default();
        let w = FakeWriter::url(Ok("https://www.songkick.com/"));
        assert_eq!(
            browse(&d, "other", &context(Some(&w))),
            "opened https://www.songkick.com/"
        );
        assert_eq!(
            *w.asked.borrow(),
            ["find the next upcoming bruno mars concert"]
        );
        assert_eq!(
            d.calls(),
            [
                Call::Remember("https://www.songkick.com/".into()),
                Call::Open("Google Chrome".into(), "https://www.songkick.com/".into()),
            ]
        );
    }

    #[test]
    fn use_browser_without_a_writer_refuses_a_site_outside_the_catalog() {
        let d = Recorder::default();
        let refusal = browse(&d, "other", &context(None));
        assert_eq!(
            refusal,
            "use_browser refused: the site is outside the catalog and no writer is available to propose a URL"
        );
        assert!(is_noop(&refusal) && d.calls().is_empty());
    }

    #[test]
    fn use_browser_refuses_when_the_writer_proposes_nothing() {
        let d = Recorder::default();
        let w = FakeWriter::url(Ok(""));
        let refusal = browse(&d, "other", &context(Some(&w)));
        assert_eq!(
            refusal,
            "use_browser refused: the writer proposed no usable URL for this goal"
        );
        assert!(is_noop(&refusal) && d.calls().is_empty());
    }

    #[test]
    fn a_browser_that_does_not_come_to_the_front_is_a_no_op() {
        let d = Recorder {
            activate_ok: false,
            ..Default::default()
        };
        let failure = browse(&d, "none", &context(None));
        assert_eq!(
            failure,
            "use_browser failed: Google Chrome did not come to the front"
        );
        assert!(is_noop(&failure));
        let d = Recorder {
            open_ok: false,
            ..Default::default()
        };
        let failure = browse(&d, "github", &context(None));
        assert_eq!(failure, "use_browser failed: opened https://github.com/ but Google Chrome did not come to the front");
    }

    #[test]
    fn a_site_outside_the_catalog_refuses_rather_than_crashing_when_the_account_is_empty() {
        let d = Recorder::default();
        let w = FakeWriter::url(Err("the account is out of credit"));
        let what = browse(&d, "other", &context(Some(&w)));
        assert_eq!(what, "use_browser refused: the writer could not propose a URL (the account is out of credit)");
        assert!(d.calls().is_empty(), "nothing should have been opened");
    }

    // ------------------------------------------------------------ typing

    #[test]
    fn typing_sets_the_value_when_the_field_reads_it_back() {
        let d = Recorder::default();
        assert_eq!(
            fill_field(&d, Some(&5), "user@example.com"),
            "via accessibility"
        );
        assert_eq!(
            d.calls(),
            [Call::Focus(5), Call::SetValue(5, "user@example.com".into())]
        );
    }

    #[test]
    fn typing_accepts_a_read_back_that_ends_with_the_text() {
        let d = Recorder {
            read_back: Some(Some("mailto:user@example.com".into())),
            ..Default::default()
        };
        assert_eq!(
            fill_field(&d, Some(&5), "user@example.com"),
            "via accessibility"
        );
        assert!(!d.calls().contains(&Call::Type("user@example.com".into())));
    }

    #[test]
    fn typing_falls_back_to_keystrokes_when_the_value_does_not_stick() {
        let d = Recorder {
            read_back: Some(Some(String::new())),
            ..Default::default()
        };
        assert_eq!(
            fill_field(&d, Some(&5), "user@example.com"),
            "via keystrokes"
        );
        assert_eq!(
            d.calls().last(),
            Some(&Call::Type("user@example.com".into()))
        );
    }

    #[test]
    fn typing_falls_back_to_keystrokes_when_the_element_refuses() {
        let d = Recorder {
            set_ok: false,
            ..Default::default()
        };
        assert_eq!(fill_field(&d, Some(&5), "hello"), "via keystrokes");
        assert_eq!(d.reads.get(), 0, "nothing was written");
        assert_eq!(d.calls().last(), Some(&Call::Type("hello".into())));
    }

    #[test]
    fn typing_uses_keystrokes_when_there_is_no_element() {
        let d = Recorder::default();
        assert_eq!(fill_field(&d, None, "hello"), "via keystrokes");
        assert_eq!(d.calls(), [Call::Type("hello".into())]);
    }

    #[test]
    fn typing_keeps_non_ascii_text_whole() {
        let d = Recorder::default();
        assert_eq!(fill_field(&d, None, "café 🎵"), "via keystrokes");
        assert_eq!(d.calls(), [Call::Type("café 🎵".into())]);
    }

    #[test]
    fn the_field_record_serialises_without_an_element() {
        let mut f = text_field("Email");
        f.value = "hello".into();
        let record = serde_json::to_value(&f).unwrap();
        assert!(record.get("ref").is_none() && record["value"] == "hello");
    }

    #[test]
    fn typing_needs_a_focused_text_field() {
        let d = Recorder::default();
        let s = screen();
        assert_eq!(
            type_email(&d, &s, &context(None)),
            "type_email refused: no text field is focused"
        );
        assert_eq!(
            type_text(&d, &s, &[], &context(None)),
            "type_text refused: no text field is focused"
        );
        let mut s = screen();
        s.field = Some(Field {
            role: "AXButton".into(),
            ..text_field("Go")
        });
        assert!(is_noop(&type_text(&d, &s, &[], &context(None))));
        assert!(d.calls().is_empty());
    }

    #[test]
    fn the_email_goes_into_the_focused_field() {
        let d = Recorder {
            focused: Some(5),
            ..Default::default()
        };
        let mut s = screen();
        s.field = Some(text_field("Email"));
        let mut ctx = context(None);
        ctx.email = Some("user@example.com".into());
        assert_eq!(type_email(&d, &s, &ctx), "typed email via accessibility");
    }

    #[test]
    fn typing_refuses_when_the_writer_is_unavailable() {
        let d = Recorder::default();
        let mut s = screen();
        s.field = Some(text_field("Search"));
        assert_eq!(
            type_text(&d, &s, &[], &context(None)),
            "type_text refused: no writer available"
        );
        let w = FakeWriter::text(Err("the key was rejected"));
        let what = type_text(&d, &s, &[], &context(Some(&w)));
        assert_eq!(
            what,
            "type_text refused: the writer is unavailable (the key was rejected)"
        );
        let w = FakeWriter::text(Ok(""));
        assert_eq!(
            type_text(&d, &s, &[], &context(Some(&w))),
            "type_text refused: writer declined to fill this field"
        );
        assert!(d.calls().is_empty());
    }

    #[test]
    fn typed_text_is_verified_and_reported() {
        let d = Recorder::default();
        let mut s = screen();
        s.field = Some(text_field("Search"));
        let w = FakeWriter::text(Ok("bruno mars tour"));
        let (s, refs) = (s, HashMap::new());
        let what = perform(
            &d,
            "type_text",
            "none",
            &view(&s, &refs, &[]),
            &[],
            &context(Some(&w)),
        )
        .unwrap();
        assert_eq!(
            what,
            "typed 'bruno mars tour' into 'Search' via keystrokes (verified 0.90)"
        );
    }

    #[test]
    fn text_that_fails_verification_is_cleared() {
        let d = Recorder::default();
        let mut s = screen();
        s.field = Some(text_field("Search"));
        let w = FakeWriter::text(Ok("bruno mars tour"));
        let mut ctx = context(Some(&w));
        let low = FixedVerifier(0.2);
        ctx.verifier = &low;
        let what = type_text(&d, &s, &[], &ctx);
        assert_eq!(what, "typed 'bruno mars tour' into 'Search' via keystrokes but verification failed (0.20); cleared it");
        assert_eq!(
            d.calls(),
            [
                Call::Type("bruno mars tour".into()),
                Call::Key(Key::A, true),
                Call::Key(Key::Delete, false)
            ]
        );
    }

    // ------------------------------------------------------------ keys, scrolling, waiting

    #[test]
    fn keys_scrolls_and_waits_say_what_they_did() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::new());
        let v = view(&s, &refs, &[]);
        let ctx = context(None);
        let run = |k: &str| perform(&d, k, "none", &v, &[], &ctx).unwrap();
        assert_eq!(run("press_enter"), "pressed Return");
        assert_eq!(run("press_escape"), "pressed Escape");
        assert_eq!(run("scroll_down"), "scrolled down");
        assert_eq!(run("scroll_up"), "scrolled up");
        assert_eq!(run("wait"), "waited");
        assert!(is_noop("waited"));
        assert_eq!(
            d.calls(),
            [
                Call::Key(Key::Return, false),
                Call::Key(Key::Escape, false),
                Call::Scroll(-10),
                Call::Scroll(10)
            ]
        );
    }

    #[test]
    fn an_unknown_action_is_an_error() {
        let d = Recorder::default();
        let (s, refs) = (screen(), HashMap::new());
        let got = perform(
            &d,
            "format_disk",
            "none",
            &view(&s, &refs, &[]),
            &[],
            &context(None),
        );
        assert_eq!(got, Err(UnknownAction("format_disk".into())));
        assert_eq!(got.unwrap_err().to_string(), "unknown action 'format_disk'");
    }

    #[test]
    fn remembering_skips_an_empty_url() {
        let d = Recorder::default();
        remember(&d, "");
        remember(&d, "https://x.example/");
        assert_eq!(d.calls(), [Call::Remember("https://x.example/".into())]);
    }

    // ------------------------------------------------------------ switching and launching

    fn fake_win(foreground: bool) -> WindowInfo {
        WindowInfo {
            hwnd: 7,
            title: "WhatsApp".into(),
            app: "whatsapp".into(),
            pid: 9,
            rect: (0, 0, 800, 600),
            monitor: 1,
            minimized: false,
            foreground,
        }
    }

    #[test]
    fn switching_brings_an_open_window_to_the_front() {
        let d = Recorder::default();
        let what = switch_window(&d, "0", &[fake_win(false)]);
        assert_eq!(d.calls(), [Call::ActivateWindow(7)]);
        assert_eq!(what, "switched to whatsapp 'WhatsApp' on monitor 2");
    }

    #[test]
    fn switching_to_a_window_that_closed_is_a_failure_not_a_crash() {
        let d = Recorder::default();
        assert_eq!(
            switch_window(&d, "4", &[fake_win(false)]),
            "switch_window failed: no window '4' is open any more"
        );
        assert!(switch_window(&d, "x", &[fake_win(false)]).contains("failed"));
        assert!(d.calls().is_empty(), "nothing to switch to");
    }

    #[test]
    fn a_window_that_refuses_to_come_forward_reports_failure() {
        let d = Recorder {
            window_ok: false,
            ..Default::default()
        };
        let what = switch_window(&d, "0", &[fake_win(false)]);
        assert_eq!(
            what,
            "switch_window failed: whatsapp did not come to the front"
        );
        assert!(is_noop(&what));
    }

    #[test]
    fn switching_to_the_window_already_in_front_is_a_no_op() {
        let d = Recorder::default();
        let what = switch_window(&d, "0", &[fake_win(true)]);
        assert!(is_noop(&what));
        assert_eq!(
            what,
            "switch_window refused: whatsapp 'WhatsApp' on monitor 2 was already in front"
        );
        assert!(d.calls().is_empty(), "it is already in front");
    }

    #[test]
    fn a_stale_flag_still_asks_windows_for_the_foreground() {
        let d = Recorder {
            foreground: 7,
            ..Default::default()
        };
        assert!(is_noop(&switch_window(&d, "0", &[fake_win(false)])));
        assert!(d.calls().is_empty());
    }

    #[test]
    fn a_long_title_is_cut_at_sixty_characters() {
        let d = Recorder::default();
        let mut w = fake_win(false);
        w.title = "é".repeat(80);
        let what = switch_window(&d, "0", &[w]);
        assert_eq!(
            what,
            format!("switched to whatsapp '{}' on monitor 2", "é".repeat(60))
        );
    }

    fn fake_app() -> App {
        App {
            key: "notepad".into(),
            label: "Notepad".into(),
            url: r"C:\fake\Notepad.lnk".into(),
            weight: 1.0,
            source: "app".into(),
        }
    }

    #[test]
    fn launching_an_offered_app_starts_it() {
        let d = Recorder::default();
        let mut ctx = context(None);
        ctx.apps = vec![fake_app()];
        assert_eq!(open_app(&d, "notepad", &ctx), "launched Notepad");
        assert_eq!(d.calls(), [Call::Launch("Notepad".into())]);
    }

    #[test]
    fn a_key_that_was_never_offered_launches_nothing() {
        // The model names a key and code owns the path, so an unknown key must not become a command.
        let d = Recorder::default();
        let mut ctx = context(None);
        ctx.apps = vec![fake_app()];
        assert_eq!(
            open_app(&d, "format_c_drive", &ctx),
            "open_app failed: 'format_c_drive' is not one of the applications offered"
        );
        assert!(d.calls().is_empty());
    }

    #[test]
    fn an_app_that_will_not_start_reports_failure() {
        let d = Recorder {
            launch_ok: false,
            ..Default::default()
        };
        let mut ctx = context(None);
        ctx.apps = vec![fake_app()];
        assert_eq!(
            open_app(&d, "notepad", &ctx),
            "open_app failed: Notepad did not start"
        );
    }

    #[test]
    fn perform_routes_window_and_app_keys() {
        let d = Recorder::default();
        let mut s = screen();
        s.windows = vec![fake_win(false)];
        let refs = HashMap::new();
        let mut ctx = context(None);
        ctx.apps = vec![fake_app()];
        let v = view(&s, &refs, &[]);
        assert!(perform(&d, "window:0", "none", &v, &[], &ctx)
            .unwrap()
            .starts_with("switched to"));
        assert_eq!(
            perform(&d, "app:notepad", "none", &v, &[], &ctx).unwrap(),
            "launched Notepad"
        );
    }

    // ------------------------------------------------------------ helpers

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(py_repr("Register Now"), "'Register Now'");
        assert_eq!(py_repr("don't"), "\"don't\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("a\\b\nc"), "'a\\\\b\\nc'");
        assert_eq!(py_repr("café"), "'café'");
        assert_eq!(py_repr("\u{1}"), "'\\x01'");
    }

    #[test]
    fn an_app_is_matched_loosely() {
        assert!(app_matches(
            "Google Chrome",
            "chrome",
            "New Tab - Google Chrome"
        ));
        assert!(app_matches("chrome", "chrome", ""));
        assert!(!app_matches("Google Chrome", "", "Google Chrome"));
        assert!(!app_matches("firefox", "chrome", "New Tab"));
        assert_eq!(browser_executable("Microsoft Edge"), "msedge");
        assert_eq!(browser_executable("Brave"), "Brave");
    }

    #[test]
    fn keys_carry_the_python_virtual_key_codes() {
        assert_eq!(Key::Return.vk(), 0x0D);
        assert_eq!(Key::Escape.vk(), 0x1B);
        assert_eq!(Key::A.vk(), 0x41);
        assert_eq!(Key::Delete.vk(), 0x2E);
    }
}
