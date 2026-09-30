//! Consecutive-timeout tracking per host.
//!
//! Ports the intent of `timeoutsCount` / `DEFAULT_TIMEOUTS_COUNT_THROW`
//! from `src/utils/Fetcher.js`: a host that times out over and over is a
//! lost cause for this run, so it gets evicted instead of burning a
//! request slot and ten seconds per attempt. The upstream field was never
//! wired up in JS; here it works as a time-decaying circuit breaker — a
//! success clears the host, and an eviction naturally lifts itself once
//! the host has been quiet for the cooloff window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Running state for one host.
#[derive(Clone, Copy)]
struct Entry {
    /// Consecutive timeouts observed.
    timeouts: u32,
    /// When the most recent timeout happened.
    last: Instant,
}

/// Time-decaying per-host timeout circuit breaker.
pub struct TimeoutLedger {
    /// Consecutive timeouts before a host is evicted.
    threshold: u32,
    /// How long an eviction holds without a success.
    cooloff: Duration,
    /// One entry per host key.
    entries: Mutex<HashMap<String, Entry>>,
}

impl TimeoutLedger {
    /// Build a ledger that evicts a host after `threshold` consecutive
    /// timeouts and holds the eviction for `cooloff` (unless a success
    /// clears it first).
    #[must_use]
    pub fn new(threshold: u32, cooloff: Duration) -> Self {
        Self {
            threshold,
            cooloff,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Record another timeout for `host`.
    ///
    /// Returns `true` exactly once per climb — when the host just reached
    /// the eviction threshold — so callers can log the eviction.
    pub fn record_timeout(&self, host: &str) -> bool {
        let mut entries = lock(&self.entries);
        let entry = entries.entry(host.to_string()).or_insert(Entry {
            timeouts: 0,
            last: Instant::now(),
        });
        entry.timeouts = entry.timeouts.saturating_add(1);
        entry.last = Instant::now();
        entry.timeouts == self.threshold
    }

    /// Record a success for `host`, clearing any timeout streak.
    pub fn record_success(&self, host: &str) {
        if host.is_empty() {
            return;
        }
        lock(&self.entries).remove(host);
    }

    /// Whether `host` is currently evicted.
    ///
    /// An eviction whose cooloff has elapsed is lifted on read, so a host
    /// that went quiet gets exactly one fresh chance to succeed.
    #[must_use]
    pub fn is_evicted(&self, host: &str) -> bool {
        if host.is_empty() {
            return false;
        }
        let mut entries = lock(&self.entries);
        let Some(entry) = entries.get(host) else {
            return false;
        };
        if entry.timeouts < self.threshold {
            return false;
        }
        if Instant::now().duration_since(entry.last) >= self.cooloff {
            entries.remove(host);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_after_the_threshold() {
        let ledger = TimeoutLedger::new(3, Duration::from_secs(60));

        assert!(
            !ledger.record_timeout("h"),
            "the first timeout is below the threshold"
        );
        assert!(
            !ledger.record_timeout("h"),
            "the second timeout is still below"
        );
        assert!(!ledger.is_evicted("h"), "two of three is not eviction");
        assert!(
            ledger.record_timeout("h"),
            "the third timeout must trip the eviction"
        );
        assert!(ledger.is_evicted("h"), "the host must now be evicted");
    }

    #[test]
    fn a_success_clears_the_streak() {
        let ledger = TimeoutLedger::new(2, Duration::from_secs(60));

        ledger.record_timeout("h");
        assert!(ledger.record_timeout("h"));
        assert!(ledger.is_evicted("h"));

        ledger.record_success("h");
        assert!(!ledger.is_evicted("h"), "a success must clear the eviction");
        assert!(
            !ledger.record_timeout("h"),
            "the count restarts from zero after a success"
        );
    }

    #[test]
    fn the_eviction_lifts_after_the_cooloff() {
        // A zero cooloff makes every eviction instantly stale, which keeps
        // this test deterministic without sleeping.
        let ledger = TimeoutLedger::new(1, Duration::ZERO);

        assert!(ledger.record_timeout("h"));
        assert!(
            !ledger.is_evicted("h"),
            "an elapsed cooloff must lift the eviction on read"
        );
    }

    #[test]
    fn blank_hosts_are_ignored() {
        let ledger = TimeoutLedger::new(1, Duration::from_secs(60));
        ledger.record_timeout("");
        assert!(!ledger.is_evicted(""));
    }
}
