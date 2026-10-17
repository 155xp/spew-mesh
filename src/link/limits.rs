//! Per-peer limits on incoming calls (5.5 step 2, 5.6) and the dial backoff (5.3).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most `deliver` calls a peer device may make per rolling minute (5.5 step 2).
pub const DELIVER_PER_MINUTE: usize = 120;
/// The rolling window for [`DELIVER_PER_MINUTE`].
pub const DELIVER_WINDOW: Duration = Duration::from_secs(60);
/// Shortest gap between two `presence` calls a receiver takes from one peer (5.6).
pub const PRESENCE_GAP: Duration = Duration::from_secs(1);

/// A rolling-window counter: at most `limit` events in any `window`.
#[derive(Debug)]
pub(crate) struct RateWindow {
    limit: usize,
    window: Duration,
    events: VecDeque<Instant>,
}

impl RateWindow {
    /// A window allowing `limit` events per `window`.
    pub(crate) fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            events: VecDeque::with_capacity(limit.min(256)),
        }
    }

    /// The `deliver` limit of 5.5 step 2.
    pub(crate) fn deliver() -> Self {
        Self::new(DELIVER_PER_MINUTE, DELIVER_WINDOW)
    }

    /// Counts an event at `now` and says whether it is within the limit. Refused events aren't counted, so the
    /// window never holds more than `limit` entries and a peer that keeps retrying gets through as soon as the
    /// oldest counted call ages out.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        while let Some(&oldest) = self.events.front() {
            if now.saturating_duration_since(oldest) >= self.window {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.limit {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

/// At most one event per `gap`; the rest are dropped (5.6).
#[derive(Debug)]
pub(crate) struct Gap {
    gap: Duration,
    last: Option<Instant>,
}

impl Gap {
    /// One event per `gap`.
    pub(crate) fn new(gap: Duration) -> Self {
        Self { gap, last: None }
    }

    /// The presence limit of 5.6.
    pub(crate) fn presence() -> Self {
        Self::new(PRESENCE_GAP)
    }

    /// Whether an event at `now` is taken; a taken event starts a new gap.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.gap => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Dial backoff: starts at `base`, doubles up to `max`, with ±20% jitter, reset when a link comes up (5.3).
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    base: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// A backoff starting at `base` and capped at `max`.
    pub(crate) fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max: max.max(base),
            next: base,
        }
    }

    /// Starts over from `base`.
    pub(crate) fn reset(&mut self) {
        self.next = self.base;
    }

    /// The next delay with jitter from `unit`, a number in `[0, 1)`: the nominal delay times `0.8 + 0.4 * unit`.
    pub(crate) fn next_with(&mut self, unit: f64) -> Duration {
        let nominal = self.next;
        self.next = (self.next * 2).min(self.max);
        nominal.mul_f64(0.8 + 0.4 * unit.clamp(0.0, 1.0))
    }

    /// The next delay with random jitter.
    pub(crate) fn next_delay(&mut self) -> Duration {
        self.next_with(random_unit())
    }
}

/// A number in `[0, 1)` from the system random source; 0.5 (no jitter) if it fails.
fn random_unit() -> f64 {
    let mut bytes = [0u8; 4];
    match getrandom::fill(&mut bytes) {
        Ok(()) => f64::from(u32::from_le_bytes(bytes)) / (f64::from(u32::MAX) + 1.0),
        Err(_) => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_window_rolls() {
        let start = Instant::now();
        let mut w = RateWindow::deliver();
        for i in 0..DELIVER_PER_MINUTE {
            assert!(w.allow(start + Duration::from_millis(i as u64)), "call {i}");
        }
        assert!(
            !w.allow(start + Duration::from_secs(1)),
            "121st in the minute"
        );
        assert!(
            !w.allow(start + Duration::from_secs(59)),
            "still in the window"
        );
        // The first call ages out at 60 s, making room for exactly one more.
        assert!(w.allow(start + Duration::from_secs(60)));
        assert!(!w.allow(start + Duration::from_secs(60)));
        // All of them age out a minute after the last.
        let later = start + Duration::from_secs(120);
        for _ in 0..DELIVER_PER_MINUTE {
            assert!(w.allow(later));
        }
        assert!(!w.allow(later));
    }

    #[test]
    fn refused_calls_are_not_counted() {
        let start = Instant::now();
        let mut w = RateWindow::new(2, Duration::from_secs(10));
        assert!(w.allow(start));
        assert!(w.allow(start));
        for s in 1..10 {
            assert!(!w.allow(start + Duration::from_secs(s)));
        }
        assert!(w.allow(start + Duration::from_secs(10)));
    }

    #[test]
    fn presence_gap() {
        let start = Instant::now();
        let mut g = Gap::presence();
        assert!(g.allow(start));
        assert!(!g.allow(start + Duration::from_millis(999)));
        assert!(g.allow(start + Duration::from_millis(1000)));
        assert!(!g.allow(start + Duration::from_millis(1500)));
        assert!(g.allow(start + Duration::from_secs(5)));
    }

    #[test]
    fn backoff_doubles_with_jitter_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(3), Duration::from_secs(300));
        assert_eq!(b.next_with(0.5), Duration::from_secs(3));
        assert_eq!(b.next_with(0.5), Duration::from_secs(6));
        assert_eq!(
            b.next_with(0.0),
            Duration::from_millis(9600),
            "12 s less 20%"
        );
        assert_eq!(
            b.next_with(1.0),
            Duration::from_millis(28800),
            "24 s plus 20%"
        );
        for _ in 0..10 {
            b.next_with(0.5);
        }
        assert_eq!(
            b.next_with(0.5),
            Duration::from_secs(300),
            "capped at 5 minutes"
        );
        b.reset();
        assert_eq!(b.next_with(0.5), Duration::from_secs(3));
        for _ in 0..100 {
            b.reset();
            let d = b.next_delay();
            assert!(d >= Duration::from_millis(2400) && d <= Duration::from_millis(3600));
        }
    }
}
