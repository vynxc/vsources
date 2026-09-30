//! Per-host request queueing.
//!
//! Ports the semaphore map behind `queuedFetch` in `src/utils/Fetcher.js`:
//! every host gets its own counting semaphore so a slow or flooded site
//! cannot starve the pool for every other host, and queueing itself is
//! bounded by a timeout so callers fail fast instead of piling up.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;
use vsources_core::error::FetchError;

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Concurrency limiter with one semaphore per host.
///
/// The first request for a host fixes that host's limit for the process
/// lifetime — exactly like the upstream `semaphores` map, where a
/// per-request `queueLimit` only applies to hosts without a semaphore yet
/// (first wins). Later, smaller limits are ignored rather than shrinking a
/// live semaphore's capacity.
pub struct HostSemaphores {
    /// Default concurrent requests per host.
    limit: usize,
    /// How long a request may wait for a host slot.
    wait: Duration,
    /// One semaphore per host key.
    semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl HostSemaphores {
    /// Build a limiter allowing `limit` concurrent requests per host, with
    /// requests giving up on queueing after `wait`.
    ///
    /// A `limit` of zero is permitted (matching upstream) and means every
    /// request for a new host waits for the full `wait` and then fails:
    /// prefer leaving it at one or higher.
    #[must_use]
    pub fn new(limit: usize, wait: Duration) -> Self {
        Self {
            limit,
            wait,
            semaphores: Mutex::new(HashMap::new()),
        }
    }

    /// The default per-host limit.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Acquire the host's slot, waiting at most the configured `wait`.
    ///
    /// `limit` overrides the default limit for hosts that do not have a
    /// semaphore yet (first-wins, see the type docs). Failing to get a slot
    /// in time surfaces as [`FetchError::Transport`] carrying `url`, the
    /// same way the upstream `withTimeout`-wrapped semaphore rejects.
    pub async fn acquire(
        &self,
        host: &str,
        limit: Option<usize>,
        url: &Url,
    ) -> Result<OwnedSemaphorePermit, FetchError> {
        let semaphore = self.semaphore_for(host, limit);
        let acquisition = tokio::time::timeout(self.wait, semaphore.acquire_owned());
        match acquisition.await {
            Ok(Ok(permit)) => Ok(permit),
            // The semaphore is closed; we never close one, but keep the
            // arm honest instead of unwrapping.
            Ok(Err(_)) => Err(FetchError::Transport {
                url: url.clone(),
                message: "the host's semaphore was closed".to_string(),
            }),
            Err(_) => Err(FetchError::Transport {
                url: url.clone(),
                message: format!(
                    "queue timeout after {:?} waiting for a {host} slot",
                    self.wait
                ),
            }),
        }
    }

    /// The host's semaphore, creating it with the effective limit when
    /// absent.
    fn semaphore_for(&self, host: &str, limit: Option<usize>) -> Arc<Semaphore> {
        let effective = limit.unwrap_or(self.limit);
        let mut semaphores = lock(&self.semaphores);
        semaphores
            .entry(host.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(effective)))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/page").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[tokio::test]
    async fn acquires_and_releases_slots() {
        let semaphores = HostSemaphores::new(2, Duration::from_secs(1));
        let host = "example.com";

        let first = semaphores.acquire(host, None, &url()).await;
        let second = semaphores.acquire(host, None, &url()).await;
        assert!(first.is_ok(), "the first slot must be granted");
        assert!(second.is_ok(), "the second slot must be granted");
        drop(first);
        drop(second);

        // Capacity is restored after release.
        let third = semaphores.acquire(host, None, &url()).await;
        assert!(third.is_ok(), "released slots must be re-acquirable");
    }

    #[tokio::test]
    async fn queue_wait_times_out_when_capacity_is_exhausted() {
        let semaphores = HostSemaphores::new(1, Duration::from_millis(50));
        let host = "example.com";

        let held = semaphores.acquire(host, None, &url()).await;
        assert!(held.is_ok());
        let queued = semaphores.acquire(host, None, &url()).await;
        match queued {
            Err(FetchError::Transport { message, .. }) => {
                assert!(
                    message.contains("queue timeout"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected a queue timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn different_hosts_do_not_share_slots() {
        let semaphores = HostSemaphores::new(1, Duration::from_millis(200));

        let example = semaphores.acquire("example.com", None, &url()).await;
        let other = semaphores.acquire("other.org", None, &url()).await;
        assert!(example.is_ok());
        assert!(other.is_ok(), "a busy host must not block a different host");
    }

    #[tokio::test]
    async fn per_request_limit_only_shapes_new_hosts() {
        let semaphores = HostSemaphores::new(4, Duration::from_millis(100));

        // First request for the host fixes its limit at 4.
        let held = semaphores.acquire("wide.example", None, &url()).await;
        assert!(held.is_ok());

        // A later, smaller override is ignored: one slot is still free
        // out of the original four.
        let queued = semaphores.acquire("wide.example", Some(1), &url()).await;
        assert!(queued.is_ok(), "an existing host keeps its first limit");
        drop(held);

        // A new host picks up the override.
        let narrow = semaphores.acquire("narrow.example", Some(1), &url()).await;
        assert!(narrow.is_ok());
        let blocked = semaphores.acquire("narrow.example", Some(1), &url()).await;
        assert!(blocked.is_err(), "the override must shape the new host");
    }
}
