//! The global hotkeys as the daemon owns them: which spec each name has, the checks a new set must
//! pass before anything is written, and the pump thread that can be re-registered without a
//! restart.
//!
//! `RegisterHotKey(NULL, ..)` delivers only to the thread that registered, so every registration
//! happens on the one pump thread. A change of keys therefore never registers from the caller: it
//! hands the new specs to the pump, stops the running serve loop, and the pump registers the new
//! set itself, on its own thread, feeding the same channel the input worker already drains.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use platform::hotkeys::{parse_hotkey, Hotkeys, StopHandle};
use serde_json::{Map, Value};
use wcore::config;

use crate::emit::Emitter;

/// How long a rebind waits for the pump to report what it registered.
const REBIND_TIMEOUT: Duration = Duration::from_secs(5);

/// Every hotkey name, in the order the settings show them.
pub fn names() -> Vec<&'static str> {
    config::DEFAULT_HOTKEYS.iter().map(|(n, _)| *n).collect()
}

/// The `.env` key behind one name.
pub fn env_key(name: &str) -> String {
    format!("CLICKER_HOTKEY_{}", name.to_uppercase())
}

/// (name, spec) pairs, or (name, reason) pairs for a refusal.
pub type Specs = Vec<(String, String)>;

/// Every name with the spec in force now (environment, else default).
pub fn current() -> Vec<(String, String)> {
    names()
        .into_iter()
        .filter_map(|n| config::hotkey(n).map(|spec| (n.to_string(), spec)))
        .collect()
}

/// Every name with its default spec.
pub fn defaults() -> Vec<(String, String)> {
    config::DEFAULT_HOTKEYS
        .iter()
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect()
}

/// The virtual key of one name's spec, for push-to-talk polling.
pub fn vk_of(specs: &[(String, String)], name: &str) -> Option<u32> {
    let spec = &specs.iter().find(|(n, _)| n == name)?.1;
    parse_hotkey(spec).ok().map(|(_, vk)| vk)
}

/// Merge `values` over `current` and check the result. Ok is the full new set, in `current`'s
/// order; Err is one (name, reason) per bad name, and then nothing may be written.
///
/// Refused: a name that is not a hotkey, a value that is not a string, a spec that does not
/// parse, and two names on the same keys (compared parsed, so "Ctrl+Alt+X" equals "ctrl+alt+x").
pub fn validate(values: &Map<String, Value>, current: &[(String, String)]) -> Result<Specs, Specs> {
    let mut errors = Vec::new();
    let mut merged: Vec<(String, String)> = current.to_vec();
    for (name, raw) in values {
        let Some(slot) = merged.iter_mut().find(|(n, _)| n == name) else {
            errors.push((name.clone(), "not a hotkey name".to_string()));
            continue;
        };
        let Some(spec) = raw.as_str().map(str::trim) else {
            errors.push((
                name.clone(),
                "needs a string like \"ctrl+alt+x\"".to_string(),
            ));
            continue;
        };
        match parse_hotkey(spec) {
            Ok(_) => slot.1 = spec.to_string(),
            Err(e) => errors.push((name.clone(), e)),
        }
    }
    if errors.is_empty() {
        let parsed: Vec<(&str, Option<(u32, u32)>)> = merged
            .iter()
            .map(|(n, s)| (n.as_str(), parse_hotkey(s).ok()))
            .collect();
        for (i, (name, keys)) in parsed.iter().enumerate() {
            let Some(keys) = keys else { continue };
            let clash = parsed
                .iter()
                .enumerate()
                .find(|(j, (_, other))| *j != i && other.as_ref() == Some(keys));
            if let Some((_, (other, _))) = clash {
                errors.push((name.to_string(), format!("uses the same keys as {other}")));
            }
        }
    }
    if errors.is_empty() {
        Ok(merged)
    } else {
        Err(errors)
    }
}

/// The per-name errors as one reply error: "hotkeys not saved: bar: ...; talk: ...".
pub fn describe(errors: &[(String, String)]) -> String {
    let parts: Vec<String> = errors.iter().map(|(n, e)| format!("{n}: {e}")).collect();
    format!("hotkeys not saved: {}", parts.join("; "))
}

/// Re-registers the hotkeys. Returns the names another app already owns.
pub trait Rebind: Send + Sync {
    fn rebind(&self, specs: &[(String, String)]) -> Result<Vec<String>, String>;
}

/// No pump (CLICKER_NO_HOTKEYS=1): the keys are saved and take effect when hotkeys next run.
pub struct NoPump;

impl Rebind for NoPump {
    fn rebind(&self, _: &[(String, String)]) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// What the pump sends the input worker.
pub enum Pumped {
    Fired(String),
    Refused(Vec<String>),
}

struct Rebinding {
    specs: Vec<(String, String)>,
    reply: mpsc::Sender<Vec<String>>,
}

/// The running pump: a way to hand it new specs, and the stop handle of the loop it is serving.
pub struct Pump {
    control: Mutex<mpsc::Sender<Rebinding>>,
    current: Arc<Mutex<Option<StopHandle>>>,
}

/// The registration table for `specs`, each callback only sending its name on `fired`.
fn table(specs: &[(String, String)], fired: &mpsc::Sender<Pumped>, out: &Emitter) -> Hotkeys {
    let mut keys = Hotkeys::new();
    for (name, spec) in specs {
        let sender = fired.clone();
        let owned = name.clone();
        if let Err(e) = keys.add(name, spec, move || {
            let _ = sender.send(Pumped::Fired(owned.clone()));
        }) {
            out.line(format!("hotkey for {name} does not parse ({spec}): {e}"));
        }
    }
    keys
}

impl Pump {
    /// Start the pump thread serving `specs`. The first serve's refusals go to `fired` as
    /// `Pumped::Refused`; a rebind's come back to the caller instead.
    pub fn start(
        specs: Vec<(String, String)>,
        fired: mpsc::Sender<Pumped>,
        out: Emitter,
    ) -> Result<Pump, String> {
        let (control_tx, control_rx) = mpsc::channel::<Rebinding>();
        // The table is built here so its stop handle is known before the thread runs: a rebind
        // that comes in at once still has a loop to stop.
        let first = table(&specs, &fired, &out);
        let current = Arc::new(Mutex::new(Some(first.stop_handle())));
        let shared = current.clone();
        std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || serve_loop(first, fired, control_rx, shared, out))
            .map_err(|e| format!("the hotkey pump would not start: {e}"))?;
        Ok(Pump {
            control: Mutex::new(control_tx),
            current,
        })
    }
}

/// Serve, and when stopped with new specs waiting, register those and serve again. Stopped with
/// nothing waiting, return.
fn serve_loop(
    mut keys: Hotkeys,
    fired: mpsc::Sender<Pumped>,
    control: mpsc::Receiver<Rebinding>,
    current: Arc<Mutex<Option<StopHandle>>>,
    out: Emitter,
) {
    let mut answer: Option<mpsc::Sender<Vec<String>>> = None;
    loop {
        let reply = answer.take();
        let ready = fired.clone();
        keys.serve(move |refused| match reply {
            Some(r) => {
                let _ = r.send(refused);
            }
            None => {
                let _ = ready.send(Pumped::Refused(refused));
            }
        });
        let Ok(next) = control.try_recv() else {
            return;
        };
        keys = table(&next.specs, &fired, &out);
        *current.lock().unwrap_or_else(|p| p.into_inner()) = Some(keys.stop_handle());
        answer = Some(next.reply);
    }
}

impl Rebind for Pump {
    fn rebind(&self, specs: &[(String, String)]) -> Result<Vec<String>, String> {
        // One rebind at a time: the next one must find the loop this one started.
        let control = self.control.lock().unwrap_or_else(|p| p.into_inner());
        let (tx, rx) = mpsc::channel();
        control
            .send(Rebinding {
                specs: specs.to_vec(),
                reply: tx,
            })
            .map_err(|_| "the hotkey pump has stopped".to_string())?;
        // The specs are queued before the stop, so the loop finds them as soon as it returns.
        let handle = self
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(h) = handle {
            h.stop();
        }
        rx.recv_timeout(REBIND_TIMEOUT)
            .map_err(|_| "the hotkey pump did not answer".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> Vec<(String, String)> {
        defaults()
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_valid_change_merges_over_the_current_set_in_order() {
        let got = validate(
            &map(json!({ "bar": " Ctrl+Alt+B ", "abort": "f9" })),
            &now(),
        )
        .unwrap();
        assert_eq!(got.len(), 7);
        assert_eq!(
            got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            names()
        );
        let spec = |n: &str| got.iter().find(|(k, _)| k == n).unwrap().1.clone();
        assert_eq!(spec("bar"), "Ctrl+Alt+B");
        assert_eq!(spec("abort"), "f9");
        assert_eq!(spec("talk"), "ctrl+alt+space");
    }

    #[test]
    fn every_bad_name_is_reported_and_nothing_is_merged() {
        let errors = validate(
            &map(json!({ "bar": "ctrl+banana", "talk": "ctrl+alt", "fly": "f2", "goal": 3 })),
            &now(),
        )
        .unwrap_err();
        let named: Vec<&str> = errors.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(named, vec!["bar", "fly", "goal", "talk"]);
        let text = describe(&errors);
        assert!(text.starts_with("hotkeys not saved: "));
        assert!(text.contains("bar: ") && text.contains("banana"), "{text}");
        assert!(text.contains("fly: not a hotkey name"), "{text}");
    }

    #[test]
    fn two_names_on_the_same_keys_are_refused_however_spelled() {
        let errors = validate(&map(json!({ "goal": "CTRL + ALT + X" })), &now()).unwrap_err();
        let named: Vec<&str> = errors.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(named, vec!["goal", "abort"]);
        assert!(errors[0].1.contains("abort"));
        // Swapping two keys in one request is not a clash.
        let ok = validate(
            &map(json!({ "goal": "ctrl+alt+x", "abort": "ctrl+alt+g" })),
            &now(),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn the_held_keys_come_from_the_specs() {
        let specs = vec![
            ("talk".to_string(), "ctrl+alt+t".to_string()),
            ("dictate".to_string(), "rightctrl".to_string()),
        ];
        assert_eq!(vk_of(&specs, "talk"), Some(b'T' as u32));
        assert_eq!(vk_of(&specs, "dictate"), Some(0xA3));
        assert_eq!(vk_of(&specs, "bar"), None);
    }
}
