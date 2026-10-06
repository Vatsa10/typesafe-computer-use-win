//! Global hotkeys: specs like "ctrl+alt+space", the registration table, and the pump that
//! delivers WM_HOTKEY.
//!
//! Port of `hotkeys.py` plus the hotkey section of `windows.py`. The rule that shapes the type:
//! `RegisterHotKey(NULL, ..)` posts WM_HOTKEY only to the thread that registered, and only while
//! that thread pumps messages. So registration and the pump happen on one thread, inside
//! [`Hotkeys::serve`], and the only thing another thread may do is [`StopHandle::stop`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PeekMessageW, PostThreadMessageW, MSG, PM_REMOVE, WM_HOTKEY, WM_QUIT,
};

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
/// One WM_HOTKEY per press, however long the key is held. Added to every registration.
pub const MOD_NOREPEAT: u32 = 0x4000;

/// How long the pump sleeps when the queue is empty, so `stop` is noticed on a timer.
const PUMP_IDLE: Duration = Duration::from_millis(20);

fn modifier(name: &str) -> Option<u32> {
    match name {
        "ctrl" | "control" => Some(MOD_CONTROL),
        "alt" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "win" => Some(MOD_WIN),
        _ => None,
    }
}

/// Only the keys worth binding to. A letter or digit needs no entry: its virtual key is its
/// upper-case ASCII code.
fn named_vk(name: &str) -> Option<u32> {
    let vk = match name {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "escape" | "esc" => 0x1B,
        "backspace" => 0x08,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        // Keys that are normally modifiers, bound on their own. Right alt is the one worth having:
        // nothing on a normal layout needs it, so it is free to mean "talk to the computer".
        "rightalt" | "ralt" => 0xA5,
        "rightctrl" | "rctrl" => 0xA3,
        "rightshift" | "rshift" => 0xA1,
        "capslock" => 0x14,
        "pause" => 0x13, // the Pause/Break key
        "scrolllock" => 0x91,
        _ => {
            let n: u32 = name.strip_prefix('f')?.parse().ok()?;
            return (1..=12).contains(&n).then_some(0x6F + n);
        }
    };
    Some(vk)
}

/// "ctrl+alt+space" to the modifier mask and virtual key `RegisterHotKey` wants.
///
/// Case and whitespace insensitive. A bare key with no modifier is valid, which is what makes
/// "rightalt" or "f9" a hotkey. The returned mask does not include `MOD_NOREPEAT`.
pub fn parse_hotkey(spec: &str) -> Result<(u32, u32), String> {
    let parts: Vec<String> = spec
        .split('+')
        .map(|p| p.split_whitespace().collect::<String>().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(format!("hotkey {spec:?} is empty"));
    }
    let mut mods = 0;
    let mut key: Option<&str> = None;
    for part in &parts {
        if let Some(m) = modifier(part) {
            mods |= m;
        } else if let Some(k) = key {
            return Err(format!(
                "hotkey {spec:?} names two keys, {k:?} and {part:?}"
            ));
        } else {
            key = Some(part);
        }
    }
    let Some(key) = key else {
        return Err(format!("hotkey {spec:?} has no key, only modifiers"));
    };
    if let Some(vk) = named_vk(key) {
        return Ok((mods, vk));
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Ok((mods, c.to_ascii_uppercase() as u32));
        }
    }
    Err(format!(
        "hotkey {spec:?} names a key this does not know: {key:?}"
    ))
}

type Callback = Box<dyn FnMut() + Send>;

struct Binding {
    name: String,
    mods: u32,
    vk: u32,
    callback: Callback,
}

/// Shared between the pumping thread and whoever wants it stopped.
struct Shared {
    stop: AtomicBool,
    /// The pumping thread's id, 0 while no `serve` is running.
    thread_id: AtomicU32,
}

/// Stops a [`Hotkeys::serve`] from any thread. Cloneable, idempotent.
#[derive(Clone)]
pub struct StopHandle(Arc<Shared>);

impl StopHandle {
    /// Set the flag and post WM_QUIT to the pumping thread. The pump unregisters every binding on
    /// its own thread as it returns, because `UnregisterHotKey(NULL, id)` only works from the
    /// thread that registered. Safe to call any number of times, before, during or after `serve`.
    pub fn stop(&self) {
        self.0.stop.store(true, Ordering::SeqCst);
        let tid = self.0.thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: a plain message post; an unknown thread id only makes it fail.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// The registration table. Ids are handed out in order (1, 2, ...) and never reused.
pub struct Hotkeys {
    bindings: Vec<Binding>,
    shared: Arc<Shared>,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self::new()
    }
}

impl Hotkeys {
    pub fn new() -> Self {
        Hotkeys {
            bindings: Vec::new(),
            shared: Arc::new(Shared {
                stop: AtomicBool::new(false),
                thread_id: AtomicU32::new(0),
            }),
        }
    }

    /// Bind `spec` to `callback` under `name`. Fails only when the spec does not parse.
    ///
    /// The callback runs on the pumping thread, between messages, so it must only enqueue (send on
    /// a channel, set a flag) and return: while it runs nothing pumps, so the next hotkey press is
    /// not delivered and a stop request sits unread. Real work belongs on another thread.
    pub fn add(
        &mut self,
        name: &str,
        spec: &str,
        callback: impl FnMut() + Send + 'static,
    ) -> Result<(), String> {
        let (mods, vk) = parse_hotkey(spec)?;
        self.bindings.push(Binding {
            name: name.to_string(),
            mods,
            vk,
            callback: Box::new(callback),
        });
        Ok(())
    }

    /// The virtual key bound under `name`, for push-to-talk's `GetAsyncKeyState` polling.
    pub fn vk_of(&self, name: &str) -> Option<u32> {
        self.bindings.iter().find(|b| b.name == name).map(|b| b.vk)
    }

    /// A handle that stops `serve` from another thread.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(self.shared.clone())
    }

    /// Stop from the owning side. Same as `stop_handle().stop()`.
    pub fn stop(&self) {
        self.stop_handle().stop();
    }

    /// Register every binding on the calling thread, report the names another process already
    /// owns through `on_ready`, then pump until stopped. Everything is unregistered before return.
    pub fn serve(&mut self, on_ready: impl FnOnce(Vec<String>)) {
        // SAFETY: no preconditions.
        let tid = unsafe { GetCurrentThreadId() };
        self.shared.thread_id.store(tid, Ordering::SeqCst);

        let mut registered: Vec<i32> = Vec::new();
        let mut refused = Vec::new();
        for (i, b) in self.bindings.iter().enumerate() {
            let id = i as i32 + 1;
            // SAFETY: a thread hotkey (no window); fails when another process owns it.
            let ok = unsafe {
                RegisterHotKey(None, id, HOT_KEY_MODIFIERS(b.mods | MOD_NOREPEAT), b.vk).is_ok()
            };
            if ok {
                registered.push(id);
            } else {
                refused.push(b.name.clone());
            }
        }
        on_ready(refused);

        let mut msg = MSG::default();
        while !self.shared.stop.load(Ordering::SeqCst) {
            // SAFETY: msg is a valid out pointer; no hwnd means this thread's whole queue.
            // PeekMessage rather than GetMessage, so the stop flag is consulted on a timer too.
            let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
            if !got {
                std::thread::sleep(PUMP_IDLE);
                continue;
            }
            if msg.message == WM_QUIT {
                break;
            }
            if msg.message == WM_HOTKEY {
                // An id with no binding is a stale message, not an error.
                let binding = msg
                    .wParam
                    .0
                    .checked_sub(1)
                    .and_then(|i| self.bindings.get_mut(i));
                if let Some(b) = binding {
                    (b.callback)();
                }
            }
        }

        for id in registered {
            // SAFETY: the same thread that registered.
            unsafe {
                let _ = UnregisterHotKey(None, id);
            }
        }
        self.shared.thread_id.store(0, Ordering::SeqCst);
        // Ready for another serve; a stop that raced in after the loop has nothing left to stop.
        self.shared.stop.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_and_keys() {
        assert_eq!(
            parse_hotkey("ctrl+alt+space"),
            Ok((MOD_CONTROL | MOD_ALT, 0x20))
        );
        assert_eq!(
            parse_hotkey("Control+Shift+G"),
            Ok((MOD_CONTROL | MOD_SHIFT, b'G' as u32))
        );
        assert_eq!(parse_hotkey(" win + 5 "), Ok((MOD_WIN, b'5' as u32)));
        assert_eq!(
            parse_hotkey("CTRL+ALT+F12"),
            Ok((MOD_CONTROL | MOD_ALT, 0x7B))
        );
        assert_eq!(parse_hotkey("ctrl+page up"), Ok((MOD_CONTROL, 0x21)));
        assert_eq!(parse_hotkey("alt+esc"), Ok((MOD_ALT, 0x1B)));
        assert_eq!(parse_hotkey("shift+return"), Ok((MOD_SHIFT, 0x0D)));
        assert_eq!(parse_hotkey("ctrl+ctrl+x"), Ok((MOD_CONTROL, b'X' as u32)));
    }

    #[test]
    fn whole_name_table() {
        let table = [
            ("space", 0x20),
            ("enter", 0x0D),
            ("return", 0x0D),
            ("tab", 0x09),
            ("escape", 0x1B),
            ("esc", 0x1B),
            ("backspace", 0x08),
            ("insert", 0x2D),
            ("delete", 0x2E),
            ("home", 0x24),
            ("end", 0x23),
            ("pageup", 0x21),
            ("pagedown", 0x22),
            ("left", 0x25),
            ("up", 0x26),
            ("right", 0x27),
            ("down", 0x28),
            ("rightalt", 0xA5),
            ("ralt", 0xA5),
            ("rightctrl", 0xA3),
            ("rctrl", 0xA3),
            ("rightshift", 0xA1),
            ("rshift", 0xA1),
            ("capslock", 0x14),
            ("pause", 0x13),
            ("scrolllock", 0x91),
        ];
        for (name, vk) in table {
            assert_eq!(parse_hotkey(name), Ok((0, vk)), "{name}");
        }
        for n in 1..=12u32 {
            assert_eq!(parse_hotkey(&format!("f{n}")), Ok((0, 0x6F + n)));
        }
    }

    #[test]
    fn bare_keys_are_valid() {
        assert_eq!(parse_hotkey("rightalt"), Ok((0, 0xA5)));
        assert_eq!(parse_hotkey("RAlt"), Ok((0, 0xA5)));
        assert_eq!(parse_hotkey("f9"), Ok((0, 0x78)));
        assert_eq!(parse_hotkey("g"), Ok((0, b'G' as u32)));
    }

    #[test]
    fn bad_specs_name_the_bad_part() {
        let e = parse_hotkey("").unwrap_err();
        assert!(e.contains("empty"), "{e}");
        assert!(parse_hotkey(" + ").unwrap_err().contains("empty"));
        let e = parse_hotkey("ctrl+alt").unwrap_err();
        assert!(e.contains("only modifiers"), "{e}");
        let e = parse_hotkey("ctrl+a+b").unwrap_err();
        assert!(
            e.contains("two keys") && e.contains("\"a\"") && e.contains("\"b\""),
            "{e}"
        );
        let e = parse_hotkey("ctrl+banana").unwrap_err();
        assert!(e.contains("\"banana\""), "{e}");
        assert!(parse_hotkey("f13").unwrap_err().contains("\"f13\""));
        assert!(parse_hotkey("f0").is_err());
        assert!(parse_hotkey("ctrl+é").is_err());
        assert!(parse_hotkey("ctrl+;").is_err());
    }

    #[test]
    fn add_rejects_bad_spec_and_vk_of_finds_good() {
        let mut h = Hotkeys::new();
        assert!(h.add("bad", "ctrl+nope", || {}).is_err());
        h.add("talk", "rightalt", || {}).unwrap();
        assert_eq!(h.vk_of("talk"), Some(0xA5));
        assert_eq!(h.vk_of("bad"), None);
    }

    #[test]
    fn stop_before_serve_is_harmless_and_idempotent() {
        let h = Hotkeys::new();
        h.stop();
        h.stop_handle().stop();
    }

    /// Registers ctrl+alt+f12 for real, then stops from another thread. Run explicitly:
    /// `cargo test -p platform hotkeys -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_register_and_unregister() {
        use std::sync::mpsc;
        let mut h = Hotkeys::new();
        h.add("probe", "ctrl+alt+f12", || {}).unwrap();
        let stop = h.stop_handle();
        let (tx, rx) = mpsc::channel();
        let pump = std::thread::spawn(move || {
            h.serve(|refused| tx.send(refused).unwrap());
            h
        });
        let refused = rx.recv().unwrap();
        println!("first serve refused: {refused:?}");
        assert!(
            refused.is_empty(),
            "ctrl+alt+f12 is owned by another process"
        );
        std::thread::sleep(Duration::from_millis(200));
        stop.stop();
        stop.stop();
        let mut h = pump.join().unwrap();
        // Released cleanly: registering the same combination again succeeds.
        let (tx, rx) = mpsc::channel();
        let stop = h.stop_handle();
        let pump = std::thread::spawn(move || h.serve(|r| tx.send(r).unwrap()));
        let refused = rx.recv().unwrap();
        println!("second serve refused: {refused:?}");
        assert!(refused.is_empty(), "not released by the first serve");
        stop.stop();
        pump.join().unwrap();
    }
}
