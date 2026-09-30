//! Host-scoped failure cooldown.
//!
//! Ports Kototoro's `CloudflareHostCooldown`: a single failed challenge
//! cools the whole host for a short window, after which requests are
//! attempted again so a recovering network can succeed. This replaces the
//! per-URL negative caches that would lock sources out for minutes —
//! 30 seconds of quiet is enough to stop hammering a failing edge
//! without punishing a host that comes back.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The default cooldown window (Kototoro's `DEFAULT_COOLDOWN_MS`).
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// Per-host Cloudflare failure cooldown.
///
/// Host-scoped and short-lived by design; shared freely between the HTTP
/// layer and the solver chain.
#[derive(Debug)]
pub struct HostCooldown {
    /// Cooldown duration applied per failure.
    duration: Duration,
    /// Host → the instant its cooldown ends.
    until: Mutex<HashMap<String, Instant>>,
}

impl HostCooldown {
    /// Create a cooldown with the default 30-second window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_duration(DEFAULT_COOLDOWN)
    }

    /// Create a cooldown with a custom window.
    #[must_use]
    pub fn with_duration(duration: Duration) -> Self {
        Self {
            duration,
            until: Mutex::new(HashMap::new()),
        }
    }

    /// The configured cooldown window.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Start (or restart) the cooldown for `host`.
    ///
    /// A zero duration clears the host instead, mirroring the upstream
    /// `duration == 0` semantics.
    pub fn cool_down(&self, host: &str) {
        if host.is_empty() {
            return;
        }
        let mut until = lock(&self.until);
        if self.duration.is_zero() {
            until.remove(host);
        } else {
            until.insert(host.to_string(), Instant::now() + self.duration);
        }
    }

    /// Whether `host` is cooling down; expired entries are dropped on
    /// read so the map never grows unbounded for quiet hosts.
    pub fn is_in_cooldown(&self, host: &str) -> bool {
        if host.is_empty() {
            return false;
        }
        let mut until = lock(&self.until);
        match until.get(host) {
            Some(deadline) if Instant::now() < *deadline => true,
            Some(_) => {
                until.remove(host);
                false
            }
            None => false,
        }
    }

    /// Forget all cooldowns.
    pub fn clear(&self) {
        lock(&self.until).clear();
    }
}

impl Default for HostCooldown {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cools_down_and_recovers() {
        let cooldown = HostCooldown::with_duration(Duration::from_millis(50));
        cooldown.cool_down("example.com");
        assert!(cooldown.is_in_cooldown("example.com"));
        assert!(!cooldown.is_in_cooldown("other.example"));

        // After the window elapses the host is retried.
        std::thread::sleep(Duration::from_millis(60));
        assert!(!cooldown.is_in_cooldown("example.com"));
    }

    #[test]
    fn zero_duration_clears() {
        let cooldown = HostCooldown::with_duration(Duration::ZERO);
        cooldown.cool_down("example.com");
        assert!(!cooldown.is_in_cooldown("example.com"));
    }

    #[test]
    fn clear_forgets_everything() {
        let cooldown = HostCooldown::new();
        cooldown.cool_down("a.example");
        cooldown.cool_down("b.example");
        cooldown.clear();
        assert!(!cooldown.is_in_cooldown("a.example"));
        assert!(!cooldown.is_in_cooldown("b.example"));
    }

    #[test]
    fn blank_hosts_are_ignored() {
        let cooldown = HostCooldown::new();
        cooldown.cool_down("");
        assert!(!cooldown.is_in_cooldown(""));
    }
}
