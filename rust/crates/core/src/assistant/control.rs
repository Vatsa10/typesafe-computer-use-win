//! Double-Escape stops an acting run.
//!
//! The method: no keyboard hook and no registered hotkey. A hotkey or a low-level hook would take
//! Escape away from every other app, and a hook that misbehaves can freeze the user's typing. The
//! watcher only polls the physical state of the Escape key (`GetAsyncKeyState`, which the app
//! already uses for push-to-talk) every few milliseconds, and only while Pointer is acting in Act
//! mode. Escape therefore always reaches the focused app as usual; Pointer merely notices two
//! presses within [`WINDOW`]. In Dry mode, or when nothing is running, it does not even look, so
//! the user's Escape presses are never observed then.

use std::time::{Duration, Instant};

/// Two presses closer together than this count as the stop gesture.
pub const WINDOW: Duration = Duration::from_millis(1500);

#[derive(Debug, Default)]
pub struct DoubleEscape {
    was_down: bool,
    first: Option<Instant>,
}

impl DoubleEscape {
    pub fn new() -> Self {
        Self::default()
    }

    /// One poll. `active` is "Act mode and something is running"; `down` is the key's physical
    /// state now. True exactly when this poll completes a double press.
    pub fn poll(&mut self, active: bool, down: bool, now: Instant) -> bool {
        if !active {
            // Not looking: forget everything, so a press made while idle never counts later.
            self.was_down = false;
            self.first = None;
            return false;
        }
        let pressed = down && !self.was_down;
        self.was_down = down;
        if !pressed {
            return false;
        }
        match self.first {
            Some(t) if now.duration_since(t) <= WINDOW => {
                self.first = None;
                true
            }
            _ => {
                self.first = Some(now);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Press and release at `ms`: two polls.
    fn tap(d: &mut DoubleEscape, active: bool, base: Instant, ms: u64) -> bool {
        let hit = d.poll(active, true, at(base, ms));
        d.poll(active, false, at(base, ms + 40));
        hit
    }

    #[test]
    fn two_presses_inside_the_window_stop_it() {
        let (mut d, t) = (DoubleEscape::new(), Instant::now());
        assert!(!tap(&mut d, true, t, 0));
        assert!(tap(&mut d, true, t, 1400));
    }

    #[test]
    fn two_presses_too_far_apart_do_not() {
        let (mut d, t) = (DoubleEscape::new(), Instant::now());
        assert!(!tap(&mut d, true, t, 0));
        assert!(!tap(&mut d, true, t, 1600));
        // The second press became the first of a new pair.
        assert!(tap(&mut d, true, t, 2500));
    }

    #[test]
    fn holding_the_key_is_one_press() {
        let (mut d, t) = (DoubleEscape::new(), Instant::now());
        for ms in (0..1000).step_by(50) {
            assert!(!d.poll(true, true, at(t, ms)));
        }
    }

    #[test]
    fn nothing_counts_outside_act_mode_or_while_idle() {
        let (mut d, t) = (DoubleEscape::new(), Instant::now());
        assert!(!tap(&mut d, false, t, 0));
        assert!(!tap(&mut d, false, t, 300));
        // A press made while inactive does not pair with one made once active.
        assert!(!tap(&mut d, false, t, 500));
        assert!(!tap(&mut d, true, t, 700));
        assert!(tap(&mut d, true, t, 900));
    }

    #[test]
    fn it_resets_after_firing() {
        let (mut d, t) = (DoubleEscape::new(), Instant::now());
        tap(&mut d, true, t, 0);
        assert!(tap(&mut d, true, t, 300));
        assert!(!tap(&mut d, true, t, 600), "a third press starts over");
    }
}
