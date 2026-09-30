//! Short-term negative cache for Cloudflare-blocked hosts.
//!
//! Ports `cfProtectedDomains` and `CF_DOMAIN_CACHE_TTL` from
//! `src/utils/Fetcher.js`: once a host proves it is serving a challenge we
//! cannot get past, every request for it fails immediately for the TTL
//! window instead of re-fetching the same interstitial. The entry
//! self-expires so a recovering host is retried later, and the stored
//! reason is preserved for the caller.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vsources_core::error::BlockedReason;

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hosts that recently blocked us, with the reason and expiry.
pub struct BlockedHosts {
    /// How long a block is remembered.
    ttl: Duration,
    /// Host key → (why it blocked us, until when).
    hosts: Mutex<HashMap<String, (BlockedReason, Instant)>>,
}

impl BlockedHosts {
    /// Build a cache that remembers blocks for `ttl`.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// The reason `host` is blocked, when a recent block is still in
    /// force.
    ///
    /// Expired entries are dropped on read, so a host whose TTL elapsed
    /// is retried on the next request.
    #[must_use]
    pub fn check(&self, host: &str) -> Option<BlockedReason> {
        if host.is_empty() {
            return None;
        }
        let mut hosts = lock(&self.hosts);
        match hosts.get(host) {
            Some(&(reason, until)) => {
                if Instant::now() >= until {
                    hosts.remove(host);
                    None
                } else {
                    Some(reason)
                }
            }
            None => None,
        }
    }

    /// Remember that `host` blocked us for `reason`, starting the TTL
    /// window now.
    pub fn mark(&self, host: &str, reason: BlockedReason) {
        if host.is_empty() {
            return;
        }
        lock(&self.hosts).insert(host.to_string(), (reason, Instant::now() + self.ttl));
    }

    /// Forget every cached block.
    pub fn clear(&self) {
        lock(&self.hosts).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_checks_hosts() {
        let blocked = BlockedHosts::new(Duration::from_secs(60));
        blocked.mark("example.com", BlockedReason::CloudflareChallenge);

        assert_eq!(
            blocked.check("example.com"),
            Some(BlockedReason::CloudflareChallenge)
        );
        assert_eq!(
            blocked.check("other.org"),
            None,
            "unrelated hosts are clean"
        );
    }

    #[test]
    fn entries_expire_after_the_ttl() {
        // A zero TTL expires immediately (now >= until), which keeps the
        // test deterministic without sleeping.
        let blocked = BlockedHosts::new(Duration::ZERO);
        blocked.mark("example.com", BlockedReason::CloudflareCensor);

        assert_eq!(
            blocked.check("example.com"),
            None,
            "an elapsed TTL must let the host be retried"
        );
    }

    #[test]
    fn clear_forgets_everything() {
        let blocked = BlockedHosts::new(Duration::from_secs(60));
        blocked.mark("a.com", BlockedReason::Unknown);
        blocked.mark("b.com", BlockedReason::FlareSolverrFailed);
        blocked.clear();

        assert_eq!(blocked.check("a.com"), None);
        assert_eq!(blocked.check("b.com"), None);
    }

    #[test]
    fn blank_hosts_are_ignored() {
        let blocked = BlockedHosts::new(Duration::from_secs(60));
        blocked.mark("", BlockedReason::Unknown);
        assert_eq!(blocked.check(""), None);
    }
}
