//! Paste-burst detection for terminals without bracketed paste.
//!
//! A run of printable characters arriving a few milliseconds apart is
//! a paste flowing through a slow pipe, not a human typing: at human
//! speed the inter-key gap is tens of milliseconds. While a burst is
//! confirmed and for a short tail afterwards, Enter is rewritten to
//! Shift+Enter, so a pasted multi-line draft lands as newlines in the
//! editor instead of submitting mid-paste. Terminals that report
//! bracketed paste natively never need this and the detector stays
//! disabled there.

use std::time::{Duration, Instant};

use crate::keys::{Key, KeyEvent};

/// Printable keystrokes this close together continue a burst run.
const MAX_KEY_GAP: Duration = Duration::from_millis(8);
/// Printable keystrokes in a run before it is called a paste.
const MIN_RUN: usize = 8;
/// Enter keeps being rewritten this long after the last burst key.
const ENTER_TAIL: Duration = Duration::from_millis(120);

/// Rewrites Enter inside a detected paste burst.
#[derive(Debug)]
pub struct PasteBurst {
    enabled: bool,
    /// Printable keys in the current fast run.
    run: usize,
    /// When the last run key arrived.
    last_key: Option<Instant>,
    /// Until when Enter is rewritten (a rolling window extended by
    /// every burst key).
    enter_until: Option<Instant>,
}

impl PasteBurst {
    /// A detector; `enabled` should be false when the terminal does
    /// bracketed paste itself.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            run: 0,
            last_key: None,
            enter_until: None,
        }
    }

    /// Feed one raw key at time `now`; returns the key to feed the UI
    /// (identical, except Enter inside a burst gains Shift).
    pub fn observe(&mut self, event: KeyEvent, now: Instant) -> KeyEvent {
        if !self.enabled {
            return event;
        }
        match event.key {
            Key::Char(c) if !c.is_control() && event.mods == crate::keys::Mods::NONE => {
                let fast = self
                    .last_key
                    .is_some_and(|t| now.duration_since(t) <= MAX_KEY_GAP);
                self.run = if fast { self.run + 1 } else { 1 };
                self.last_key = Some(now);
                if self.run >= MIN_RUN {
                    self.enter_until = Some(now + ENTER_TAIL);
                }
                event
            }
            Key::Enter => {
                if self.enter_until.is_some_and(|t| now <= t) {
                    KeyEvent {
                        mods: crate::keys::Mods {
                            shift: true,
                            ..event.mods
                        },
                        ..event
                    }
                } else {
                    event
                }
            }
            _ => {
                self.run = 0;
                event
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn disabled_detector_passes_everything_through() {
        let mut burst = PasteBurst::new(false);
        let t = at(0);
        for ms in 0..20 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms),
            );
        }
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(100));
        assert!(!enter.mods.shift, "disabled: Enter must stay untouched");
    }

    #[test]
    fn short_fast_run_does_not_suppress_enter() {
        let mut burst = PasteBurst::new(true);
        let t = at(0);
        for ms in 0..6 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms * 5),
            );
        }
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(50));
        assert!(!enter.mods.shift, "6 fast keys are under the threshold");
    }

    #[test]
    fn confirmed_burst_rewrites_enter_to_shift_enter() {
        let mut burst = PasteBurst::new(true);
        let t = at(0);
        for ms in 0..10 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms),
            );
        }
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(50));
        assert!(enter.mods.shift, "Enter inside a burst becomes Shift+Enter");
    }

    #[test]
    fn human_speed_typing_never_confirms_a_burst() {
        let mut burst = PasteBurst::new(true);
        let t = at(0);
        for ms in 0..30 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms * 40),
            );
        }
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(1300));
        assert!(!enter.mods.shift);
    }

    #[test]
    fn enter_tail_expires_after_the_burst_goes_quiet() {
        let mut burst = PasteBurst::new(true);
        let t = at(0);
        for ms in 0..10 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms),
            );
        }
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(500));
        assert!(!enter.mods.shift, "the rewrite window has closed");
    }

    #[test]
    fn non_printable_keys_break_the_run() {
        let mut burst = PasteBurst::new(true);
        let t = at(0);
        for ms in 0..7 {
            let _ = burst.observe(
                KeyEvent::plain(Key::Char('a')),
                t + Duration::from_millis(ms),
            );
        }
        let _ = burst.observe(KeyEvent::plain(Key::Left), t + Duration::from_millis(7));
        let _ = burst.observe(
            KeyEvent::plain(Key::Char('a')),
            t + Duration::from_millis(8),
        );
        let _ = burst.observe(
            KeyEvent::plain(Key::Char('a')),
            t + Duration::from_millis(9),
        );
        let enter = burst.observe(KeyEvent::plain(Key::Enter), t + Duration::from_millis(20));
        assert!(!enter.mods.shift, "the arrow key reset the run to 2");
    }
}
