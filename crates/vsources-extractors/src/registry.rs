//! Result cache and fallback chain over [`Extractor`]s.
//!
//! Ports `src/extractor/ExtractorRegistry.js`:
//!
//! - **All** matching extractors run as a chain — if the first returns
//!   nothing, the next one gets the URL (so the generic embed resolver
//!   can pick up URLs a dedicated extractor failed to resolve).
//! - Results are cached per canonical URL, keyed by the first matching
//!   extractor's id, its cache version, and the calling source's id, and
//!   live exactly as long as the shortest-lived stream they contain.
//! - Concurrent extractions of the same key coalesce into one upstream
//!   computation (the upstream `inFlight` map, via the cache's
//!   coalescing `try_get_with`).
//! - With media metadata present, a media-keyed fallback extractor (by
//!   default `vidking`) joins the chain — the upstream `meta.vidking`
//!   routing, which reaches for a TMDB-based API when embed scraping
//!   cannot work.
//!
//! The base-class behavior of upstream `Extractor.js` — `NotFound` to
//! empty, any other error to a final external stream — lives here too:
//! host extractors only ever return `Result`, and the registry decides
//! what a failure looks like. When the external fallback is disabled
//! (the upstream default), failures surface as [`ExtractorError`].

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use moka::Expiry;
use moka::future::Cache;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Format;
use vsources_core::types::Stream;

/// How long an unresolved (all-`NotFound`) extraction is remembered.
///
/// Upstream never caches misses; sixty seconds keeps a failing host from
/// being hammered in a tight resolve loop without masking a recovery.
const MISSES_TTL: Duration = Duration::from_secs(60);
/// Upper bound on cached extractions (upstream's eviction threshold).
const CACHE_CAPACITY: u64 = 80;
/// The upstream media fallback id (`meta.vidking` routing).
const DEFAULT_MEDIA_FALLBACK_ID: &str = "vidking";

/// The result cache: min-stream-TTL expiry per entry.
struct MinStreamTtl;

impl Expiry<String, Arc<Vec<Stream>>> for MinStreamTtl {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Arc<Vec<Stream>>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(
            value
                .iter()
                .map(|stream| stream.ttl)
                .min()
                .unwrap_or(MISSES_TTL),
        )
    }
}

/// Caches extraction misses for a short, fixed window.
struct MissesTtl;

impl Expiry<String, Arc<()>> for MissesTtl {
    fn expire_after_create(
        &self,
        _key: &String,
        _value: &Arc<()>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(MISSES_TTL)
    }
}

/// Routes embed URLs through a chain of [`Extractor`]s with caching.
pub struct ExtractorRegistry {
    /// Extractors in priority order (first match wins the chain head).
    extractors: Vec<Arc<dyn Extractor>>,
    /// Successful extractions, keyed canonically.
    results: Cache<String, Arc<Vec<Stream>>>,
    /// Recently failed (all-`NotFound`) keys.
    misses: Cache<String, Arc<()>>,
    /// Emit an external stream when nothing resolves.
    external_fallback: bool,
    /// The media-keyed fallback extractor id, when routing is enabled.
    media_fallback_id: Option<String>,
}

impl ExtractorRegistry {
    /// Build a registry over `extractors` in priority order.
    #[must_use]
    pub fn new(extractors: Vec<Arc<dyn Extractor>>) -> Self {
        Self {
            extractors,
            results: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(MinStreamTtl)
                .build(),
            misses: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(MissesTtl)
                .build(),
            external_fallback: false,
            media_fallback_id: Some(DEFAULT_MEDIA_FALLBACK_ID.to_string()),
        }
    }

    /// Emit the embed URL as an external stream when nothing resolves.
    ///
    /// Ports `showExternalUrls`/`includeExternalUrls` — the upstream
    /// `ExternalUrl` terminal extractor, folded into the registry. Off by
    /// default, exactly like the upstream config default.
    #[must_use]
    pub fn with_external_fallback(mut self, enabled: bool) -> Self {
        self.external_fallback = enabled;
        self
    }

    /// Route unresolved media to the extractor with this id.
    ///
    /// Ports the `meta.vidking.tmdbId` routing: when the context carries
    /// media with a TMDB id and the extractor exists, it joins the end of
    /// every chain.
    #[must_use]
    pub fn with_media_fallback_id(mut self, id: impl Into<String>) -> Self {
        self.media_fallback_id = Some(id.into());
        self
    }

    /// Disable the media fallback routing.
    #[must_use]
    pub fn without_media_fallback(mut self) -> Self {
        self.media_fallback_id = None;
        self
    }

    /// The registered extractors in priority order.
    #[must_use]
    pub fn extractors(&self) -> &[Arc<dyn Extractor>] {
        &self.extractors
    }

    /// Resolve `url` into direct streams, ports `handle`.
    ///
    /// Returns an empty vector when the URL matches no extractor, or when
    /// every matching extractor found nothing; with the external fallback
    /// enabled those cases yield the embed URL as an external stream.
    pub async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let mut matching: Vec<Arc<dyn Extractor>> = self
            .extractors
            .iter()
            .filter(|extractor| extractor.supports(ctx, url))
            .cloned()
            .collect();

        // Media-keyed fallback: join the chain when media context is
        // available and the fallback extractor is not already in it.
        if ctx.media.as_ref().and_then(|media| media.tmdb_id).is_some()
            && let Some(id) = self.media_fallback_id.as_deref()
            && !matching.iter().any(|e| e.id() == id)
            && let Some(fallback) = self.extractors.iter().find(|e| e.id() == id)
        {
            matching.push(fallback.clone());
        }

        if matching.is_empty() {
            return Ok(self.external_or_empty(url));
        }

        let head = matching[0].clone();
        let normalized = head.normalize(url);
        let canonical = head.normalize_async(ctx, &normalized).await;

        let mut key = cache_key(&head, &canonical, ctx.source_id);
        // Fallback results depend on the media, and hotlink-gated extraction
        // depends on Referer. A reusable embed URL must not leak a cached
        // movie/episode or headers into a different request context.
        let _ = write!(key, "|media={:?}|referer={:?}", ctx.media, ctx.referer);
        if self.misses.contains_key(&key) {
            return Ok(self.external_or_empty(url));
        }

        let extraction = self.results.try_get_with(key.clone(), async {
            match run_chain(ctx, &matching, &canonical).await {
                Ok(streams) => Ok(Arc::new(streams)),
                Err(ExtractorError::NotFound) => {
                    // Misses are remembered briefly, successes never see
                    // this branch.
                    self.misses.insert(key, Arc::new(())).await;
                    Err(ExtractorError::NotFound)
                }
                Err(error) => Err(error),
            }
        });

        match extraction.await {
            Ok(streams) => Ok(streams.as_ref().clone()),
            Err(error) => match error.as_ref() {
                ExtractorError::NotFound => Ok(self.external_or_empty(url)),
                _ if self.external_fallback => Ok(vec![external_stream(url)]),
                other => Err(other.clone()),
            },
        }
    }

    /// The external stream when enabled, else nothing.
    fn external_or_empty(&self, url: &Url) -> Vec<Stream> {
        if self.external_fallback {
            vec![external_stream(url)]
        } else {
            Vec::new()
        }
    }
}

/// Try every matching extractor until one yields streams.
///
/// Empty results and `NotFound` both hand the URL to the next extractor —
/// the upstream fallback loop; a hard error is remembered and the chain
/// continues, exactly like the upstream base class converting errors to
/// result-less outcomes.
async fn run_chain(
    ctx: &ResolveCtx<'_>,
    matching: &[Arc<dyn Extractor>],
    canonical: &Url,
) -> Result<Vec<Stream>, ExtractorError> {
    let mut last_error = None;
    for extractor in matching {
        tracing::debug!(
            extractor = extractor.id(),
            url = canonical.as_str(),
            "extracting"
        );
        match extractor.extract(ctx, canonical).await {
            Ok(streams) if !streams.is_empty() => {
                return Ok(decorate(streams, extractor, ctx));
            }
            Ok(_) | Err(ExtractorError::NotFound) => {}
            Err(error) => {
                tracing::warn!(extractor = extractor.id(), "extraction failed: {error}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or(ExtractorError::NotFound))
}

/// The upstream cache key: `{id}_{canonical}[_version][__source]`.
fn cache_key(extractor: &Arc<dyn Extractor>, canonical: &Url, source_id: Option<&str>) -> String {
    let mut key = format!("{}_{}", extractor.id(), canonical.as_str());
    if let Some(version) = extractor.cache_version() {
        key.push('_');
        key.push_str(&version.to_string());
    }
    if let Some(source) = source_id {
        key.push_str("__");
        key.push_str(source);
    }
    key
}

/// Apply the base-class decoration to winning streams.
///
/// Ports the `label`/`ttl`/`extractorLabel` defaults of upstream
/// `Extractor.extract`: every stream is attributed to the extractor that
/// produced it and to the calling source.
fn decorate(
    streams: Vec<Stream>,
    extractor: &Arc<dyn Extractor>,
    ctx: &ResolveCtx<'_>,
) -> Vec<Stream> {
    streams
        .into_iter()
        .map(|mut stream| {
            stream
                .label
                .get_or_insert_with(|| extractor.label().to_string());
            if stream.meta.extractor_label.is_none() {
                stream.meta.extractor_label = Some(extractor.label().to_string());
            }
            if stream.meta.source_id.is_none() {
                stream.meta.source_id = ctx.source_id.map(str::to_string);
            }
            stream
        })
        .collect()
}

/// The external fallback stream (upstream `ExternalUrl`).
fn external_stream(url: &Url) -> Stream {
    let host = url.host_str().unwrap_or("external").to_string();
    Stream::new(url.clone(), Format::Unknown)
        .with_label(host)
        .with_ttl(Duration::from_hours(6))
        .mark_external()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::Fetcher;
    use vsources_core::traits::ResolvedMedia;
    use vsources_core::types::Stream;

    use super::*;

    /// A fetcher that refuses to fetch — the registry tests never touch
    /// the network.
    struct NoFetch;

    #[async_trait]
    impl Fetcher for NoFetch {
        async fn request(
            &self,
            request: vsources_core::traits::FetchRequest,
        ) -> Result<vsources_core::traits::FetchResponse, FetchError> {
            Err(FetchError::Transport {
                url: request.url,
                message: "the registry tests never fetch".to_string(),
            })
        }
    }

    enum Behavior {
        Streams(usize),
        Empty,
        NotFound,
        Fail,
    }

    struct MockExtractor {
        id: &'static str,
        host: &'static str,
        behavior: Behavior,
        calls: AtomicUsize,
    }

    impl MockExtractor {
        fn streams(id: &'static str, host: &'static str, count: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                host,
                behavior: Behavior::Streams(count),
                calls: AtomicUsize::new(0),
            })
        }

        fn miss(id: &'static str, host: &'static str, behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                id,
                host,
                behavior,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Extractor for MockExtractor {
        fn id(&self) -> &'static str {
            self.id
        }

        fn label(&self) -> &'static str {
            "Mock"
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            url.host_str().is_some_and(|host| host.contains(self.host))
        }

        async fn extract(
            &self,
            _ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::Streams(count) => Ok((0..*count)
                    .map(|index| {
                        Stream::new(url.clone(), Format::Hls)
                            .with_label(format!("mock-{index}"))
                            .with_ttl(Duration::from_secs(3600))
                    })
                    .collect()),
                Behavior::Empty => Ok(Vec::new()),
                Behavior::NotFound => Err(ExtractorError::NotFound),
                Behavior::Fail => Err(ExtractorError::extraction(self.id, "boom")),
            }
        }
    }

    fn ctx(media: Option<ResolvedMedia>, source_id: Option<&str>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher: &NoFetch,
            media,
            source_id,
            referer: None,
        }
    }

    fn url() -> Url {
        Url::parse("https://dood.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    fn media_with_tmdb() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(42),
            imdb_id: None,
            name: "Test".to_string(),
            year: None,
            season: None,
            episode: None,
        }
    }

    #[tokio::test]
    async fn extracts_with_the_first_matching_extractor() {
        let extractor = MockExtractor::streams("dood", "dood", 2);
        let registry = ExtractorRegistry::new(vec![extractor.clone()]);

        let streams = registry
            .extract(&ctx(None, Some("mysource")), &url())
            .await
            .unwrap_or_else(|e| panic!("extraction must succeed: {e}"));
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].label.as_deref(), Some("mock-0"));
        assert_eq!(streams[0].meta.extractor_label.as_deref(), Some("Mock"));
        assert_eq!(streams[0].meta.source_id.as_deref(), Some("mysource"));
    }

    #[tokio::test]
    async fn falls_back_when_the_first_extractor_misses() {
        let dedicated = MockExtractor::miss("dood", "dood", Behavior::NotFound);
        let generic = MockExtractor::streams("embed", "dood", 1);
        let registry = ExtractorRegistry::new(vec![dedicated.clone(), generic.clone()]);

        let streams = registry
            .extract(&ctx(None, None), &url())
            .await
            .unwrap_or_else(|e| panic!("the fallback must succeed: {e}"));
        assert_eq!(streams.len(), 1, "the generic extractor must win");
        assert_eq!(dedicated.calls(), 1);
        assert_eq!(generic.calls(), 1);
    }

    #[tokio::test]
    async fn falls_back_when_the_first_extractor_returns_empty() {
        let dedicated = MockExtractor::miss("dood", "dood", Behavior::Empty);
        let generic = MockExtractor::streams("embed", "dood", 1);
        let registry = ExtractorRegistry::new(vec![dedicated.clone(), generic.clone()]);

        let streams = registry
            .extract(&ctx(None, None), &url())
            .await
            .unwrap_or_else(|e| panic!("the empty fallback must succeed: {e}"));
        assert_eq!(streams.len(), 1, "an empty first result must fall through");
        assert_eq!(dedicated.calls(), 1);
        assert_eq!(generic.calls(), 1);
    }

    #[tokio::test]
    async fn all_not_found_yields_no_streams() {
        let extractor = MockExtractor::miss("dood", "dood", Behavior::NotFound);
        let registry = ExtractorRegistry::new(vec![extractor]);

        let streams = registry
            .extract(&ctx(None, None), &url())
            .await
            .unwrap_or_else(|e| panic!("not-found must resolve cleanly: {e}"));
        assert!(streams.is_empty());
    }

    #[tokio::test]
    async fn hard_errors_surface_when_no_fallback_matches() {
        let extractor = MockExtractor::miss("dood", "dood", Behavior::Fail);
        let registry = ExtractorRegistry::new(vec![extractor]);

        match registry.extract(&ctx(None, None), &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("the hard failure must surface, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_fallback_returns_the_embed_url() {
        let extractor = MockExtractor::miss("dood", "dood", Behavior::NotFound);
        let registry = ExtractorRegistry::new(vec![extractor]).with_external_fallback(true);

        let streams = registry
            .extract(&ctx(None, None), &url())
            .await
            .unwrap_or_else(|e| panic!("the external fallback must succeed: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url, url());
        assert!(streams[0].is_external);
        assert_eq!(streams[0].label.as_deref(), Some("dood.to"));
    }

    #[tokio::test]
    async fn unmatched_urls_return_nothing() {
        let extractor = MockExtractor::streams("dood", "dood", 1);
        let registry = ExtractorRegistry::new(vec![extractor]);

        let other =
            Url::parse("https://example.com/watch").unwrap_or_else(|e| panic!("valid URL: {e}"));
        let streams = registry
            .extract(&ctx(None, None), &other)
            .await
            .unwrap_or_else(|e| panic!("no match must resolve cleanly: {e}"));
        assert!(streams.is_empty());
    }

    #[tokio::test]
    async fn media_fallback_joins_the_chain() {
        let vidking = MockExtractor::streams("vidking", "anything", 1);
        let registry = ExtractorRegistry::new(vec![vidking.clone()]);

        // The embed URL matches nothing, but media context is present.
        let streams = registry
            .extract(&ctx(Some(media_with_tmdb()), None), &url())
            .await
            .unwrap_or_else(|e| panic!("the media fallback must succeed: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(vidking.calls(), 1, "the fallback extractor must run");

        // Without media, the same URL resolves to nothing.
        vidking.calls.store(0, Ordering::SeqCst);
        let streams = registry
            .extract(&ctx(None, None), &url())
            .await
            .unwrap_or_else(|e| panic!("no media must resolve cleanly: {e}"));
        assert!(streams.is_empty());
        assert_eq!(vidking.calls(), 0, "the fallback must stay dormant");
    }

    #[tokio::test]
    async fn results_are_cached_per_source() {
        let extractor = MockExtractor::streams("dood", "dood", 1);
        let registry = ExtractorRegistry::new(vec![extractor.clone()]);

        for _ in 0..2 {
            let streams = registry
                .extract(&ctx(None, Some("same")), &url())
                .await
                .unwrap_or_else(|e| panic!("extraction must succeed: {e}"));
            assert_eq!(streams.len(), 1);
        }
        assert_eq!(extractor.calls(), 1, "the second hit must come from cache");

        // A different source id is a different cache key.
        let streams = registry
            .extract(&ctx(None, Some("other")), &url())
            .await
            .unwrap_or_else(|e| panic!("extraction must succeed: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(extractor.calls(), 2, "a new source id must re-extract");
    }

    #[tokio::test]
    async fn misses_are_remembered_briefly() {
        let extractor = MockExtractor::miss("dood", "dood", Behavior::NotFound);
        let registry = ExtractorRegistry::new(vec![extractor.clone()]);

        for _ in 0..2 {
            let streams = registry
                .extract(&ctx(None, None), &url())
                .await
                .unwrap_or_else(|e| panic!("not-found must resolve cleanly: {e}"));
            assert!(streams.is_empty());
        }
        assert_eq!(extractor.calls(), 1, "the miss cache must short-circuit");
    }
    #[tokio::test]
    async fn cache_isolated_by_media_episode_and_referer() -> Result<(), ExtractorError> {
        let extractor = MockExtractor::streams("dood", "dood", 1);
        let registry = ExtractorRegistry::new(vec![extractor.clone()]);
        let mut context = ctx(Some(media_with_tmdb()), Some("source"));
        registry.extract(&context, &url()).await?;
        registry.extract(&context, &url()).await?;
        assert_eq!(extractor.calls(), 1);
        if let Some(media) = context.media.as_mut() {
            media.season = Some(1);
            media.episode = Some(2);
        }
        registry.extract(&context, &url()).await?;
        assert_eq!(extractor.calls(), 2);
        let referer = Url::parse("https://another.example/").unwrap_or_else(|e| panic!("URL: {e}"));
        context.referer = Some(&referer);
        registry.extract(&context, &url()).await?;
        assert_eq!(extractor.calls(), 3);
        Ok(())
    }
}
