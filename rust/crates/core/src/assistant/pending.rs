//! A question put to the shell that a later request answers: a confirm, a camera capture.
//!
//! The asker opens an id, emits the event carrying it, then waits. The reply method looks the id
//! up. Opening before emitting means a reply that races the emit is never lost.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use api::chat::Cancel;

pub struct Pending<T> {
    slots: Mutex<HashMap<String, Option<T>>>,
    changed: Condvar,
    next: AtomicU64,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
            next: AtomicU64::new(1),
        }
    }
}

impl<T> Pending<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new question and return its id.
    pub fn open(&self, prefix: &str) -> String {
        let id = format!("{prefix}-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.lock().insert(id.clone(), None);
        id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<T>>> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Answer an open question. False when the id is unknown or already answered.
    pub fn reply(&self, id: &str, value: T) -> bool {
        let mut slots = self.lock();
        match slots.get_mut(id) {
            Some(slot) if slot.is_none() => {
                *slot = Some(value);
                self.changed.notify_all();
                true
            }
            _ => false,
        }
    }

    /// Wait for the answer. None on timeout or cancel; either way the id is closed, so a late
    /// reply is refused.
    pub fn wait(&self, id: &str, timeout: Duration, cancel: &Cancel) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut slots = self.lock();
        loop {
            if let Some(Some(_)) = slots.get(id) {
                return slots.remove(id).flatten();
            }
            if cancel.is_cancelled() || Instant::now() >= deadline {
                slots.remove(id);
                return None;
            }
            let step = Duration::from_millis(25).min(deadline - Instant::now());
            slots = self
                .changed
                .wait_timeout(slots, step)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    pub fn open_count(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_reply_reaches_the_waiter_once() {
        let p = Arc::new(Pending::<bool>::new());
        let id = p.open("c");
        let (p2, id2) = (p.clone(), id.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            assert!(p2.reply(&id2, true));
            assert!(!p2.reply(&id2, false), "a second reply is refused");
        });
        assert_eq!(
            p.wait(&id, Duration::from_secs(5), &Cancel::new()),
            Some(true)
        );
        assert_eq!(p.open_count(), 0);
    }

    #[test]
    fn timeout_and_cancel_close_the_id() {
        let p = Pending::<bool>::new();
        let id = p.open("c");
        assert_eq!(p.wait(&id, Duration::from_millis(60), &Cancel::new()), None);
        assert!(!p.reply(&id, true), "late replies are refused");
        let id = p.open("c");
        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(p.wait(&id, Duration::from_secs(5), &cancel), None);
        assert!(!p.reply("nope", true));
    }
}
