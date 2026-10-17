//! The conversation guard (5.5 step 7): at most 24 messages per conversation per 15 minutes on this device, all
//! senders together.

use std::collections::{HashMap, VecDeque};

/// Messages allowed per conversation in one window.
pub const LIMIT: usize = 24;
/// The window, ms.
pub const WINDOW_MS: u64 = 15 * 60 * 1000;

/// Recent message times per conversation.
#[derive(Debug, Default)]
pub struct Guard {
    seen: HashMap<String, VecDeque<u64>>,
}

impl Guard {
    /// Whether one more message in `conversation` fits at `now`.
    pub fn allows(&mut self, conversation: &str, now: u64) -> bool {
        match self.seen.get_mut(conversation) {
            Some(times) => {
                prune(times, now);
                times.len() < LIMIT
            }
            None => true,
        }
    }

    /// Counts a stored message.
    pub fn record(&mut self, conversation: &str, now: u64) {
        let times = self.seen.entry(conversation.to_string()).or_default();
        prune(times, now);
        times.push_back(now);
    }

    /// Forgets conversations quiet for a whole window.
    pub fn sweep(&mut self, now: u64) {
        self.seen.retain(|_, times| {
            prune(times, now);
            !times.is_empty()
        });
    }
}

fn prune(times: &mut VecDeque<u64>, now: u64) {
    while times
        .front()
        .is_some_and(|t| now.saturating_sub(*t) >= WINDOW_MS)
    {
        times.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twenty_four_per_fifteen_minutes() {
        let mut g = Guard::default();
        for i in 0..LIMIT as u64 {
            assert!(g.allows("c", i));
            g.record("c", i);
        }
        assert!(!g.allows("c", 100));
        assert!(g.allows("other", 100));
        // The first message leaves the window.
        assert!(g.allows("c", WINDOW_MS));
        g.sweep(WINDOW_MS * 3);
        assert!(g.seen.is_empty());
    }
}
