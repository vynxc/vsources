//! Per-provider resolve caching.
//!
//! Ports the caching half of the upstream `StreamResolver`: results are
//! remembered for five minutes (never longer than the shortest-lived
//! stream they contain), and "not on this provider" answers are
//! remembered for fifteen seconds. Concurrent resolves of the same media
//! coalesce into one upstream scrape via the result cache's in-flight
//! joining.
//!
//! [`SourceError::NotFound`] is an answer, not a failure: fan-out callers
//! merge provider outputs and only propagate real errors, so the cached
//! wrapper converts it to an empty result — the upstream
//! "NotFound-to-empty" semantics.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::Expiry;
use moka::future::Cache;
use vsources_core::error::SourceError;
use vsources_core::traits::{ResolveCtx, Source};
use vsources_core::types::{MediaRef, SourceInfo, Stream};

/// How long a non-empty result set is remembered.
const RESULTS_TTL: Duration = Duration::from_mins(5);
/// How long an empty outcome is remembered.
///
/// Long enough that a missing title is not hammered in a tight resolve
/// loop, short enough that a just-published upload is found quickly.
const EMPTY_TTL: Duration = Duration::from_secs(15);
/// Upper bound on cached entries, the upstream eviction threshold.
const CACHE_CAPACITY: u64 = 256;

/// Expire a cached result at its shortest stream TTL, bounded by
/// [`RESULTS_TTL`].
struct MinStreamTtl;

impl Expiry<CacheKey, Arc<Vec<Stream>>> for MinStreamTtl {
    fn expire_after_create(
        &self,
        _key: &CacheKey,
        value: &Arc<Vec<Stream>>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(
            value
                .iter()
                .map(|stream| stream.ttl)
                .min()
                .unwrap_or(RESULTS_TTL)
                .min(RESULTS_TTL),
        )
    }
}

/// Expire a negative entry after [`EMPTY_TTL`].
struct EmptyTtl;

impl Expiry<CacheKey, ()> for EmptyTtl {
    fn expire_after_create(
        &self,
        _key: &CacheKey,
        _value: &(),
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(EMPTY_TTL)
    }
}

/// A provider id paired with the media it resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    provider: Arc<str>,
    media: MediaRef,
}

/// A [`Source`] whose resolve outcomes are cached.
///
/// The wrapper is transparent: `info` delegates to the wrapped source,
/// and only `resolve` gains the caching behavior.
pub struct CachedSource {
    /// The wrapped provider.
    inner: Arc<dyn Source>,
    /// Successful, non-empty extractions.
    results: Cache<CacheKey, Arc<Vec<Stream>>>,
    /// Recently-missed keys (not-found or empty).
    empty: Cache<CacheKey, ()>,
}

impl CachedSource {
    /// Wrap `source` with the standard caching behavior.
    #[must_use]
    pub fn new(source: Arc<dyn Source>) -> Self {
        Self {
            inner: source,
            results: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(MinStreamTtl)
                .build(),
            empty: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(EmptyTtl)
                .build(),
        }
    }

    /// The wrapped source.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn Source> {
        &self.inner
    }
}

#[async_trait]
impl Source for CachedSource {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let key = CacheKey {
            provider: Arc::from(self.inner.info().id.as_str()),
            media: media.clone(),
        };
        // A recent miss answers immediately, without touching the
        // provider.
        if self.empty.contains_key(&key) {
            return Ok(Vec::new());
        }

        let inner = Arc::clone(&self.inner);
        let empty = self.empty.clone();
        let outcome = self
            .results
            .try_get_with(key.clone(), async move {
                match inner.resolve(ctx, media).await {
                    // An empty answer is a miss: it belongs in the
                    // fifteen-second negative window, not the
                    // five-minute result cache, so it is reported as a
                    // `NotFound` sentinel that the caller converts back.
                    Ok(streams) if streams.is_empty() => {
                        empty.insert(key, ()).await;
                        Err(SourceError::NotFound)
                    }
                    Ok(streams) => Ok(Arc::new(streams)),
                    Err(SourceError::NotFound) => {
                        empty.insert(key, ()).await;
                        Err(SourceError::NotFound)
                    }
                    // Real errors are never cached; the next call
                    // retries the provider.
                    Err(error) => Err(error),
                }
            })
            .await;

        match outcome {
            Ok(streams) => Ok(streams.as_ref().clone()),
            Err(error) => match error.as_ref() {
                SourceError::NotFound => Ok(Vec::new()),
                other => Err(other.clone()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use vsources_core::types::{MediaId, MediaType};

    use super::*;
    use crate::testing::{CountingSource, Outcome, stub_ctx, stub_media};

    fn cached(outcome: Outcome) -> (Arc<CountingSource>, CachedSource) {
        let source = Arc::new(CountingSource {
            info: crate::testing::stub_info("counter", 0),
            calls: AtomicUsize::new(0),
            outcome,
        });
        let calls = Arc::clone(&source);
        (calls, CachedSource::new(source))
    }

    #[tokio::test]
    async fn not_found_becomes_empty_and_is_negative_cached() -> Result<(), SourceError> {
        let (calls, cached) = cached(Outcome::NotFound);
        let ctx = stub_ctx();
        let media = stub_media();

        let first = cached.resolve(&ctx, &media).await?;
        let second = cached.resolve(&ctx, &media).await?;
        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(calls.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn empty_results_are_negative_cached() -> Result<(), SourceError> {
        let (calls, cached) = cached(Outcome::Empty);
        let ctx = stub_ctx();
        let media = stub_media();

        assert!(cached.resolve(&ctx, &media).await?.is_empty());
        assert!(cached.resolve(&ctx, &media).await?.is_empty());
        assert_eq!(calls.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn results_are_cached_until_ttl() -> Result<(), SourceError> {
        let (calls, cached) = cached(Outcome::OneStream);
        let ctx = stub_ctx();
        let media = stub_media();

        let first = cached.resolve(&ctx, &media).await?;
        let second = cached.resolve(&ctx, &media).await?;
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].url, second[0].url);
        assert_eq!(calls.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let (calls, cached) = cached(Outcome::Error);
        let ctx = stub_ctx();
        let media = stub_media();

        assert!(cached.resolve(&ctx, &media).await.is_err());
        assert!(cached.resolve(&ctx, &media).await.is_err());
        assert_eq!(calls.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn distinct_media_resolve_separately() -> Result<(), SourceError> {
        let (calls, cached) = cached(Outcome::OneStream);
        let ctx = stub_ctx();

        cached.resolve(&ctx, &stub_media()).await?;
        let other = MediaRef {
            id: MediaId::Tmdb(1396),
            kind: MediaType::Series,
            season: Some(1),
            episode: Some(3),
        };
        cached.resolve(&ctx, &other).await?;
        assert_eq!(calls.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }
}
