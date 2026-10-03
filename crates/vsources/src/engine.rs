//! The resolution engine: one call, every provider, merged streams.
//!
//! Ports `PhoeniX` `StreamResolver.getStreams`: a bounded fan-out over the
//! registered providers (one ~35s budget each), TMDB metadata attached
//! when available, URL-deduplicated merge in provider-priority order,
//! release-name enrichment, and a quality-height sort. Providers answer
//! "not found" as empty results, so only a failure of *every* selected
//! provider surfaces as an error.
//!
//! [`Engine::resolve`] returns the finished list in one shot;
//! [`Engine::resolve_progressive`] emits the merged list through a
//! channel as each provider completes, for callers that want results
//! on arrival.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;
use url::Url;
use vsources_core::enrich::enrich_stream;
use vsources_core::error::SourceError;
use vsources_core::mappings::MappingService;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{Fetcher, ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, MediaRef, SourceInfo, Stream};
use vsources_net::ChromeFetcher;
use vsources_providers::SourceRegistry;

use crate::liveness::{ProbeConfig, StreamProbe};

/// One provider's resolve outcome: registry index, provider id, result.
type SourceOutcome = (usize, String, Result<Vec<Stream>, SourceError>);
/// A type-erased per-provider resolve future.
///
/// The boxing is load-bearing — see [`Engine::resolve`].
type SourceJob<'a> = Pin<Box<dyn Future<Output = SourceOutcome> + Send + 'a>>;

/// Per-provider budget before the engine abandons it.
const DEFAULT_SOURCE_TIMEOUT: Duration = Duration::from_secs(35);
/// How many providers scrape concurrently.
const DEFAULT_CONCURRENCY: usize = 6;
const FAST_DUB_PROVIDERS: &[&str] = &["aniwaves", "reanime", "animekai"];
const FAST_DUB_BUDGET: Duration = Duration::from_secs(12);

fn english_dub(stream: &Stream) -> bool {
    !stream.is_external
        && stream.meta.dubbed == Some(true)
        && stream.meta.languages.contains(&CountryCode::En)
        && stream
            .meta
            .audio_selection
            .as_ref()
            .is_none_or(|audio| audio.language == CountryCode::En)
        && !stream.behavior_hints.contains_key("reanimeXorKey")
}

/// The current merged view of `entries`, in final display order:
/// resolution descending, then registry index (provider priority),
/// then arrival order within a provider.
fn ordered_snapshot(entries: &[(usize, u32, Stream)]) -> Vec<Stream> {
    let mut sorted: Vec<&(usize, u32, Stream)> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.2.meta
            .resolution
            .unwrap_or(0)
            .cmp(&a.2.meta.resolution.unwrap_or(0))
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    });
    sorted
        .into_iter()
        .map(|(_, _, stream)| stream.clone())
        .collect()
}

/// Errors produced by the engine facade.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The opt-in fast English-dub race exceeded its total budget.
    #[error("fast English-dub resolution exceeded its 12-second budget")]
    FastDubTimeout,
    /// The default fetcher could not be constructed.
    #[error("the default fetcher could not be built: {0}")]
    Fetcher(String),
    /// Every selected provider failed.
    #[error("all {count} selected providers failed")]
    AllProvidersFailed {
        /// How many providers were selected.
        count: usize,
    },
    /// The default provider set was requested without a TMDB client.
    #[error("the default providers need a TMDB client: set TMDB_API_KEY or pass .tmdb(...)")]
    NoTmdb,
}

/// Builds an [`Engine`].
///
/// The defaults construct a `ChromeFetcher` with browser-like headers,
/// TMDB from the environment (`TMDB_API_KEY`/`TMDB_ACCESS_TOKEN`), and
/// no provider allowlist.
#[derive(Default)]
pub struct EngineBuilder {
    /// A caller-supplied fetcher, bypassing the default one.
    fetcher: Option<Arc<dyn Fetcher>>,
    /// A `FlareSolverr` daemon URL for Cloudflare solving.
    flaresolverr: Option<Url>,
    /// A proxy for the default fetcher's traffic.
    proxy: Option<String>,
    /// An explicit TMDB client.
    tmdb: Option<TmdbClient>,
    /// An explicit TMDB API key, resolved against the final fetcher.
    tmdb_key: Option<String>,
    /// The registered providers.
    sources: Vec<Arc<dyn Source>>,
    /// Provider id allowlist; empty means all.
    allowlist: Vec<String>,
    /// Whether the caller explicitly selected providers, including all providers.
    provider_selection_explicit: bool,
    /// Per-provider resolve budget.
    per_source_timeout: Option<Duration>,
    /// Provider fan-out width.
    concurrency: Option<usize>,
    /// Assemble the wave-1 default provider set at build time.
    default_providers: bool,
    /// Shared liveness probe policy.
    probe_config: ProbeConfig,
}

impl EngineBuilder {
    /// Start with an empty configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Use this fetcher instead of the default `ChromeFetcher`.
    ///
    /// When set, [`Self::flaresolverr`] and [`Self::proxy`] no longer
    /// apply — the fetcher owns its own Cloudflare strategy.
    #[must_use]
    pub fn fetcher(mut self, fetcher: Arc<dyn Fetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    /// Route Cloudflare challenges to the `FlareSolverr` daemon at `url`.
    #[must_use]
    pub fn flaresolverr(mut self, url: Url) -> Self {
        self.flaresolverr = Some(url);
        self
    }

    /// Send the default fetcher's traffic through this proxy.
    #[must_use]
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.proxy = Some(proxy.into());
        self
    }

    /// Use an explicit TMDB client.
    #[must_use]
    pub fn tmdb(mut self, tmdb: TmdbClient) -> Self {
        self.tmdb = Some(tmdb);
        self
    }

    /// Use a TMDB API key (an alternative to [`Self::tmdb`]).
    ///
    /// The client is constructed with the engine's final fetcher during
    /// [`EngineBuilder::build`].
    #[must_use]
    pub fn tmdb_key(mut self, key: impl Into<String>) -> Self {
        self.tmdb_key = Some(key.into());
        self
    }

    /// Register the providers to resolve through.
    #[must_use]
    pub fn sources(mut self, sources: Vec<Arc<dyn Source>>) -> Self {
        self.sources = sources;
        self
    }

    /// Restrict resolution to these provider ids (empty keeps all).
    #[must_use]
    pub fn providers(mut self, ids: &[&str]) -> Self {
        self.allowlist = ids.iter().map(ToString::to_string).collect();
        self.provider_selection_explicit = true;
        self
    }

    /// Override the per-provider resolve budget.
    #[must_use]
    pub fn per_source_timeout(mut self, timeout: Duration) -> Self {
        self.per_source_timeout = Some(timeout);
        self
    }

    /// Override the provider fan-out width.
    #[must_use]
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = Some(concurrency);
        self
    }

    /// Configure shared validation before streams enter result snapshots.
    #[must_use]
    pub fn probe_config(mut self, config: ProbeConfig) -> Self {
        self.probe_config = config;
        self
    }

    /// Resolve through the built-in provider catalog — all 46
    /// English providers (wave 1 + wave 2).
    ///
    /// `VSOURCES_PROVIDERS` optionally supplies a comma-separated allowlist
    /// (e.g. from `.env.generated`). Explicit [`Self::providers`] takes precedence.
    /// The set is assembled with the engine's TMDB client, so
    /// [`Self::sources`] is left empty. Requires TMDB metadata; call
    /// [`Self::tmdb`] or set `TMDB_API_KEY`/`TMDB_ACCESS_TOKEN`.
    #[must_use]
    pub fn with_default_providers(mut self) -> Self {
        self.default_providers = true;
        self
    }

    /// Assemble the engine.
    ///
    /// # Errors
    ///
    /// [`EngineError::Fetcher`] when the default fetcher cannot be
    /// built (TLS or proxy misconfiguration).
    pub fn build(self) -> Result<Engine, EngineError> {
        let fetcher = match self.fetcher {
            Some(fetcher) => {
                if self.flaresolverr.is_some() {
                    tracing::warn!("flaresolverr ignored: a custom fetcher was provided");
                }
                fetcher
            }
            None => Arc::new(build_fetcher(
                self.flaresolverr.as_ref(),
                self.proxy.as_deref(),
            )?),
        };

        let tmdb = self
            .tmdb
            .or_else(|| {
                self.tmdb_key
                    .map(|key| TmdbClient::new(key, Arc::clone(&fetcher)))
            })
            .or_else(|| TmdbClient::from_env(Arc::clone(&fetcher)));

        let sources = if self.default_providers {
            let Some(tmdb) = tmdb.as_ref() else {
                return Err(EngineError::NoTmdb);
            };
            // One mapping service shared by every anime provider — the
            // arm/anilist id lookups are deduped and cached across the
            // whole fan-out.
            let mappings = MappingService::new(Arc::clone(&fetcher));
            let wave1 = vsources_providers::wave1(Arc::new(tmdb.clone()), mappings.clone());
            let wave2 = vsources_providers::wave2(Arc::new(tmdb.clone()), mappings);
            wave1.into_iter().chain(wave2).collect()
        } else {
            self.sources
        };

        // Generated audit profiles apply only to the default catalog. Explicit
        // .providers(...) always wins; .providers(&[]) deliberately selects all.
        let allowlist = if self.default_providers && !self.provider_selection_explicit {
            std::env::var("VSOURCES_PROVIDERS")
                .map_or(self.allowlist, |value| parse_provider_allowlist(&value))
        } else {
            self.allowlist
        };
        let selected: Vec<Arc<dyn Source>> = if allowlist.is_empty() {
            sources
        } else {
            sources
                .into_iter()
                .filter(|source| {
                    let keep = allowlist.iter().any(|id| *id == source.info().id);
                    if !keep {
                        tracing::debug!(provider = source.info().id, "filtered by allowlist");
                    }
                    keep
                })
                .collect()
        };

        Ok(Engine {
            probes: StreamProbe::new(self.probe_config),
            fetcher,
            tmdb,
            registry: SourceRegistry::new(selected),
            per_source_timeout: self.per_source_timeout.unwrap_or(DEFAULT_SOURCE_TIMEOUT),
            concurrency: self.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1),
        })
    }
}

/// Parse a generated provider selection without changing process environment.
fn parse_provider_allowlist(value: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for id in value.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// The default fetcher, with the Cloudflare chain when configured.
fn build_fetcher(
    flaresolverr: Option<&Url>,
    proxy: Option<&str>,
) -> Result<ChromeFetcher, EngineError> {
    let mut builder = ChromeFetcher::builder();
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    if let Some(base) = flaresolverr {
        // The solver daemon is reached through its own plain transport;
        // the scrape traffic goes through the chained fetcher.
        let solver_fetcher = ChromeFetcher::builder()
            .build()
            .map_err(|error| EngineError::Fetcher(error.to_string()))?;
        let client = vsources_cloudflare::FlareSolverr::new(base.clone(), Arc::new(solver_fetcher));
        let chain = vsources_cloudflare::SolverChain::new(vec![Arc::new(
            vsources_cloudflare::FlareSolverrSolver::new(client),
        )]);
        builder = builder.cloudflare(chain);
    }
    builder
        .build()
        .map_err(|error| EngineError::Fetcher(error.to_string()))
}

/// The assembled resolution engine.
pub struct Engine {
    /// The HTTP layer shared by all providers.
    fetcher: Arc<dyn Fetcher>,
    /// TMDB metadata, when configured.
    tmdb: Option<TmdbClient>,
    /// The cached, priority-ordered providers.
    registry: SourceRegistry,
    /// Per-provider resolve budget.
    per_source_timeout: Duration,
    /// Provider fan-out width.
    concurrency: usize,
    /// Shared per-stream verdict cache.
    probes: StreamProbe,
}

impl Engine {
    /// Whether known metadata allows a catalog on this request.
    async fn animation_status(
        &self,
        media: &MediaRef,
        metadata: Option<&ResolvedMedia>,
    ) -> Option<bool> {
        let tmdb_id = metadata?.tmdb_id?;
        self.tmdb
            .as_ref()?
            .cached_is_animation(tmdb_id, media.kind)
            .await
    }

    /// Resolve selected providers for English spoken audio, including verified
    /// embedded track selection when supported. SUB/English subtitle flags alone
    /// do not qualify. Keeps the normal quality-sorted, all-provider behavior.
    pub async fn resolve_english_dub(&self, media: &MediaRef) -> Result<Vec<Stream>, EngineError> {
        let (progress, discarded) = mpsc::unbounded_channel();
        drop(discarded);
        self.resolve_progressive_with_audio(media, progress, true)
            .await
    }

    /// Return the first direct English-dub result from the tested fast shortlist.
    ///
    /// Races `AniWaves`, `ReAnime` and `AnimeKai` within the configured provider
    /// allowlist. Shares metadata/caches and cancels unfinished work on success.
    /// The 12-second total budget includes metadata and bounded liveness checks.
    /// This chooses first resolution completion, not measured player startup or
    /// maximum quality. Reuse the returned stream's required audio selection.
    pub async fn resolve_fast_english_dub(
        &self,
        media: &MediaRef,
    ) -> Result<Option<Stream>, EngineError> {
        tokio::time::timeout(FAST_DUB_BUDGET, self.fast_dub_inner(media))
            .await
            .map_err(|_| EngineError::FastDubTimeout)?
    }

    async fn fast_dub_inner(&self, media: &MediaRef) -> Result<Option<Stream>, EngineError> {
        let mut sources: Vec<_> = self
            .registry
            .all()
            .into_iter()
            .filter(|source| FAST_DUB_PROVIDERS.contains(&source.info().id.as_str()))
            .collect();
        if sources.is_empty() {
            return Ok(None);
        }
        let resolved = match &self.tmdb {
            Some(tmdb) => tmdb.resolve_media(media).await.ok(),
            None => None,
        };
        let animation = self.animation_status(media, resolved.as_ref()).await;
        sources.retain(|source| provider_scope_allows(&source.info().id, animation));
        let jobs: Vec<_> = sources
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                self.resolve_source(
                    index,
                    source,
                    media,
                    resolved.clone(),
                    true,
                    self.per_source_timeout.min(Duration::from_secs(8)),
                )
            })
            .collect();
        let mut pending = stream::iter(jobs).buffer_unordered(self.concurrency.max(1));
        let mut failures = 0;
        let mut answered = false;
        while let Some((_, _, result)) = pending.next().await {
            match result {
                Ok(mut streams) => {
                    answered = true;
                    streams.retain(english_dub);
                    streams.sort_by_key(|stream| {
                        std::cmp::Reverse(stream.meta.resolution.unwrap_or(0))
                    });
                    if let Some(mut stream) = streams.into_iter().next() {
                        let label = stream.label.clone().unwrap_or_default();
                        enrich_stream(&mut stream, &label);
                        return Ok(Some(stream));
                    }
                }
                Err(_) => failures += 1,
            }
        }
        if !answered && failures > 0 {
            Err(EngineError::AllProvidersFailed { count: failures })
        } else {
            Ok(None)
        }
    }

    /// Resolve `media` into a merged, deduplicated, quality-sorted
    /// stream list.
    ///
    /// Every selected provider gets one budget; not-found answers are
    /// empty results, per-source failures are logged and skipped, and
    /// only a universal failure is an error.
    ///
    /// # Errors
    ///
    /// [`EngineError::AllProvidersFailed`] when every selected provider
    /// errored (timed out, was blocked, or failed to scrape).
    pub async fn resolve(&self, media: &MediaRef) -> Result<Vec<Stream>, EngineError> {
        // Discarded receiver: the snapshots go nowhere and the final
        // answer is the return value.
        let (progress, discarded) = mpsc::unbounded_channel();
        drop(discarded);
        self.resolve_progressive(media, progress).await
    }

    /// Resolve `media` incrementally, sending the merged stream list
    /// through `progress` as providers complete.
    ///
    /// Each snapshot is the full current view — deduplicated (the
    /// earliest provider in registry order wins a URL), enriched, and
    /// sorted by resolution descending, then provider priority, then
    /// arrival order — so a receiver can simply replace its table with
    /// the latest message. The final snapshot is always the returned
    /// value. Providers that answer nothing emit no snapshot, and
    /// per-source failures are logged and skipped.
    ///
    /// # Errors
    ///
    /// [`EngineError::AllProvidersFailed`] when every selected provider
    /// errored (timed out, was blocked, or failed to scrape).
    pub async fn resolve_progressive(
        &self,
        media: &MediaRef,
        progress: mpsc::UnboundedSender<Vec<Stream>>,
    ) -> Result<Vec<Stream>, EngineError> {
        self.resolve_progressive_with_audio(media, progress, false)
            .await
    }

    async fn resolve_progressive_with_audio(
        &self,
        media: &MediaRef,
        progress: mpsc::UnboundedSender<Vec<Stream>>,
        english_only: bool,
    ) -> Result<Vec<Stream>, EngineError> {
        let resolved = match &self.tmdb {
            Some(tmdb) => match tmdb.resolve_media(media).await {
                Ok(resolved) => Some(resolved),
                Err(error) => {
                    tracing::warn!(%error, "tmdb metadata unavailable; resolving without it");
                    None
                }
            },
            None => None,
        };

        let animation = self.animation_status(media, resolved.as_ref()).await;

        // Pre-box each job's future over a plain iterator, then feed the
        // boxes to `buffer_unordered`: a closure inside a stream
        // combinator that returns a future over `Arc<dyn Source>` trips
        // the higher-ranked `FnOnce` check once this future is spawned
        // (`tokio::spawn` in the TUI and server fan-outs), while an
        // iterator closure has no such obligation. `buffer_unordered`
        // keeps the sliding-window bound *and* completes outcomes in
        // arrival order — `buffered` would let a slow high-priority
        // provider hold back every later provider's results, which the
        // incremental merge cannot tolerate.
        let jobs: Vec<SourceJob<'_>> = self
            .registry
            .all()
            .into_iter()
            .filter(|source| provider_scope_allows(&source.info().id, animation))
            .enumerate()
            .map(|(index, source)| {
                self.resolve_source(
                    index,
                    source,
                    media,
                    resolved.clone(),
                    english_only,
                    self.per_source_timeout,
                )
            })
            .collect();

        // Incremental merge state: (registry index, arrival sequence,
        // stream) with a URL map, so a late answer from an earlier
        // registry position can take over a URL an earlier arrival
        // claimed — the same winner the index-ordered single-shot merge
        // would pick.
        let mut failures = Vec::new();
        let mut any_answered = false;
        let mut entries: Vec<(usize, u32, Stream)> = Vec::new();
        let mut claimed: HashMap<Url, usize> = HashMap::new();
        let mut sequence = 0_u32;

        let mut outcomes = stream::iter(jobs).buffer_unordered(self.concurrency);
        while let Some((index, id, result)) = outcomes.next().await {
            match result {
                Ok(streams) => {
                    any_answered = true;
                    let mut grew = false;
                    for mut stream in streams {
                        let release_name = stream
                            .label
                            .clone()
                            .unwrap_or_else(|| release_name_of(&stream.url));
                        enrich_stream(&mut stream, &release_name);
                        match claimed.get(&stream.url) {
                            None => {
                                claimed.insert(stream.url.clone(), entries.len());
                                entries.push((index, sequence, stream));
                                sequence += 1;
                                grew = true;
                            }
                            Some(&position) if index < entries[position].0 => {
                                entries[position] = (index, sequence, stream);
                                sequence += 1;
                                grew = true;
                            }
                            Some(_) => {}
                        }
                    }
                    if grew && !progress.is_closed() {
                        let _ = progress.send(ordered_snapshot(&entries));
                    }
                }
                Err(error) => {
                    tracing::warn!(provider = %id, %error, "provider failed");
                    failures.push((id, error));
                }
            }
        }

        if !any_answered && !failures.is_empty() {
            return Err(EngineError::AllProvidersFailed {
                count: failures.len(),
            });
        }
        Ok(ordered_snapshot(&entries))
    }
    /// One provider's bounded resolve, as a type-erased future.
    ///
    /// The boxing is load-bearing: a closure returning an `async` block
    /// over `(usize, Arc<dyn Source>)` jobs fails the higher-ranked
    /// `FnOnce` check once the outer `resolve` future is itself spawned
    /// (`tokio::spawn` in the TUI, or a server) because the returned
    /// future type would mention the trait object's lifetime. A boxed
    /// future erases it.
    fn resolve_source<'a>(
        &'a self,
        index: usize,
        source: Arc<dyn Source>,
        media: &'a MediaRef,
        meta: Option<ResolvedMedia>,
        english_only: bool,
        timeout: Duration,
    ) -> SourceJob<'a> {
        Box::pin(async move {
            let id = source.info().id.clone();
            let ctx = ResolveCtx {
                fetcher: self.fetcher.as_ref(),
                media: meta,
                source_id: Some(id.as_str()),
                referer: None,
            };
            let resolve = async {
                if english_only {
                    source.resolve_english_dub(&ctx, media).await
                } else {
                    source.resolve(&ctx, media).await
                }
            };
            let bounded = tokio::time::timeout(timeout, resolve);
            let result = match bounded.await {
                Ok(Ok(mut streams)) => {
                    if english_only {
                        streams.retain(english_dub);
                    }
                    Ok(self.probes.filter(self.fetcher.as_ref(), streams).await)
                }
                Ok(Err(error)) => Err(error),
                Err(_elapsed) => Err(SourceError::scrape(&id, "resolve timed out")),
            };
            (index, id, result)
        })
    }

    /// The registered providers' descriptors, in priority order.
    #[must_use]
    pub fn providers(&self) -> Vec<&SourceInfo> {
        self.registry.list()
    }

    /// The HTTP layer shared by all providers.
    #[must_use]
    pub fn fetcher(&self) -> &Arc<dyn Fetcher> {
        &self.fetcher
    }

    /// The TMDB client, when configured.
    #[must_use]
    pub fn tmdb(&self) -> Option<&TmdbClient> {
        self.tmdb.as_ref()
    }
}

/// Unknown or animated metadata preserves eligibility. Only definitive non-animation
/// excludes the animation catalogs; general movie/TV providers remain eligible.
fn provider_scope_allows(provider: &str, animation: Option<bool>) -> bool {
    animation != Some(false) || !vsources_providers::ANIME_ONLY_PROVIDER_IDS.contains(&provider)
}

/// The release-name-ish string for a stream with no label: the URL's
/// final path segment without its extension.
fn release_name_of(url: &Url) -> String {
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[..dot].to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use vsources_core::types::{Format, SourceInfo};
    use vsources_extractors::helpers::direct_stream;

    use super::*;

    struct NoopFetcher;
    #[async_trait]
    impl Fetcher for NoopFetcher {
        async fn request(
            &self,
            _: vsources_core::traits::FetchRequest,
        ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError>
        {
            panic!("engine unit tests must not access the network")
        }
    }
    fn test_builder() -> EngineBuilder {
        EngineBuilder::new().fetcher(Arc::new(NoopFetcher))
    }

    /// A parsed test URL; panics like `Url::parse` on bad input.
    fn test_url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|_| panic!("invalid test URL: {s}"))
    }

    /// A provider that answers after an optional delay.
    #[derive(Clone)]
    struct StubSource {
        info: SourceInfo,
        streams: Vec<Stream>,
        error: Option<SourceError>,
        delay: Option<Duration>,
    }

    impl StubSource {
        fn streaming(id: &str, priority: i32, urls: &[&str]) -> Arc<Self> {
            let streams = urls
                .iter()
                .map(|u| {
                    direct_stream(
                        test_url(u),
                        Format::Mp4,
                        Duration::from_secs(300),
                        &test_url("https://origin.example/"),
                    )
                })
                .collect();
            Arc::new(Self {
                info: info(id, priority),
                streams,
                error: None,
                delay: None,
            })
        }

        fn failing(id: &str, error: SourceError) -> Arc<Self> {
            Arc::new(Self {
                info: info(id, 0),
                streams: Vec::new(),
                error: Some(error),
                delay: None,
            })
        }

        /// A provider that answers after `delay`.
        fn delayed(id: &str, priority: i32, urls: &[&str], delay: Duration) -> Arc<Self> {
            let streams = urls
                .iter()
                .map(|u| {
                    direct_stream(
                        test_url(u),
                        Format::Mp4,
                        Duration::from_secs(300),
                        &test_url("https://origin.example/"),
                    )
                })
                .collect();
            Arc::new(Self {
                info: info(id, priority),
                streams,
                error: None,
                delay: Some(delay),
            })
        }
    }

    fn info(id: &str, priority: i32) -> SourceInfo {
        SourceInfo {
            id: id.to_string(),
            label: id.to_string(),
            content_types: vec![vsources_core::types::MediaType::Movie],
            country_codes: Vec::new(),
            base_url: None,
            priority,
            domain_key: None,
        }
    }

    #[async_trait]
    impl Source for StubSource {
        fn info(&self) -> &SourceInfo {
            &self.info
        }

        async fn resolve(
            &self,
            _ctx: &ResolveCtx<'_>,
            _media: &MediaRef,
        ) -> Result<Vec<Stream>, SourceError> {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            match &self.error {
                Some(error) => Err(match error {
                    SourceError::NotFound => SourceError::NotFound,
                    other => SourceError::scrape(&self.info.id, other.to_string()),
                }),
                None => Ok(self.streams.clone()),
            }
        }
    }

    fn media() -> MediaRef {
        MediaRef {
            id: vsources_core::types::MediaId::Tmdb(27_205),
            kind: vsources_core::types::MediaType::Movie,
            season: None,
            episode: None,
        }
    }

    #[test]
    fn generated_provider_profile_trims_and_deduplicates_ids() {
        assert_eq!(
            parse_provider_allowlist(" reanime, vidzee,reanime, ,animekai "),
            ["reanime", "vidzee", "animekai"]
        );
        assert!(parse_provider_allowlist(" , ").is_empty());
        let builder = EngineBuilder::new().providers(&[]);
        assert!(
            builder.provider_selection_explicit,
            "explicit all must override the environment"
        );
    }

    #[tokio::test]
    async fn non_animation_does_not_resolve_through_a_same_title_anime_catalog()
    -> Result<(), EngineError> {
        struct MetadataFetcher {
            genres: serde_json::Value,
        }
        #[async_trait]
        impl Fetcher for MetadataFetcher {
            async fn request(
                &self,
                request: vsources_core::traits::FetchRequest,
            ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError>
            {
                Ok(vsources_core::traits::FetchResponse { url:request.url,status:200,headers:std::collections::BTreeMap::new(),
                    body:serde_json::json!({"title":"Casablanca","release_date":"1943-01-15","genres":self.genres}).to_string() })
            }
        }
        for (genres, expected) in [
            (serde_json::json!([{"id":18}]), 1),
            (serde_json::json!([{"id":16}]), 2),
            (serde_json::Value::Null, 2),
        ] {
            let fetcher: Arc<dyn Fetcher> = Arc::new(MetadataFetcher { genres });
            let tmdb = TmdbClient::new("test-key", fetcher.clone());
            let engine = test_builder()
                .fetcher(fetcher)
                .tmdb(tmdb)
                .sources(vec![
                    StubSource::streaming(
                        "2dhive",
                        0,
                        &["https://cdn.example/anime-music-video.mp4"],
                    ),
                    StubSource::streaming(
                        "vidlink2",
                        0,
                        &["https://cdn.example/requested-film.mp4"],
                    ),
                ])
                .build()?;
            let streams = engine
                .resolve(&MediaRef::movie(vsources_core::types::MediaId::Tmdb(289)))
                .await?;
            assert_eq!(streams.len(), expected);
            if expected == 1 {
                assert_eq!(
                    streams[0].url.as_str(),
                    "https://cdn.example/requested-film.mp4"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn fast_dub_race_does_not_wait_for_sub_or_slow_sources() -> Result<(), EngineError> {
        let mut dub = (*StubSource::delayed(
            "aniwaves",
            0,
            &["https://cdn.example/dub.m3u8"],
            Duration::from_millis(10),
        ))
        .clone();
        dub.streams[0].meta.dubbed = Some(true);
        dub.streams[0].meta.languages = vec![CountryCode::En];
        let engine = test_builder()
            .sources(vec![
                StubSource::streaming("animekai", 0, &["https://cdn.example/sub.m3u8"]),
                StubSource::delayed(
                    "reanime",
                    0,
                    &["https://cdn.example/slow.mp4"],
                    Duration::from_secs(60),
                ),
                Arc::new(dub),
            ])
            .build()?;
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            engine.resolve_fast_english_dub(&media()),
        )
        .await
        .unwrap_or_else(|e| panic!("fast race waited for slow provider: {e}"))?;
        assert_eq!(
            result.map(|stream| stream.url.to_string()).as_deref(),
            Some("https://cdn.example/dub.m3u8")
        );
        Ok(())
    }

    #[tokio::test]
    async fn english_subtitle_and_external_flags_do_not_qualify_as_dub() -> Result<(), EngineError>
    {
        let mut source = (*StubSource::streaming(
            "animekai",
            0,
            &[
                "https://cdn.example/sub.mp4",
                "https://cdn.example/external",
            ],
        ))
        .clone();
        source.streams[0].meta.languages = vec![CountryCode::Ja, CountryCode::En];
        source.streams[1].meta.languages = vec![CountryCode::En];
        source.streams[1].meta.dubbed = Some(true);
        source.streams[1].is_external = true;
        let engine = test_builder().sources(vec![Arc::new(source)]).build()?;
        assert!(engine.resolve_fast_english_dub(&media()).await?.is_none());
        assert_eq!(engine.resolve(&media()).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn merges_and_dedups_by_url_in_priority_order() -> Result<(), EngineError> {
        let engine = test_builder()
            .sources(vec![
                StubSource::streaming("inception-hd", 10, &["https://cdn.example/a.mp4"]),
                StubSource::streaming(
                    "inception-sd",
                    1,
                    &["https://cdn.example/a.mp4", "https://cdn.example/b.mp4"],
                ),
            ])
            .build()?;

        let streams = engine.resolve(&media()).await?;
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].url.as_str(), "https://cdn.example/a.mp4");
        Ok(())
    }

    #[tokio::test]
    async fn progressive_snapshots_land_before_slow_providers_finish() -> Result<(), EngineError> {
        // The low-priority provider answers immediately; the
        // high-priority one takes 200 ms. The first snapshot must carry
        // only the fast provider's stream, proving results stream in as
        // they land rather than after the whole fan-out.
        let engine = test_builder()
            .sources(vec![
                StubSource::delayed(
                    "slow-hd",
                    10,
                    &["https://cdn.example/a.mp4"],
                    Duration::from_millis(200),
                ),
                StubSource::streaming("fast-sd", 1, &["https://cdn.example/b.mp4"]),
            ])
            .build()?;

        let (progress, mut receiver) = mpsc::unbounded_channel();
        let progressive = engine.resolve_progressive(&media(), progress).await?;

        let first = receiver
            .try_recv()
            .unwrap_or_else(|error| panic!("expected an early snapshot: {error:?}"));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].url.as_str(), "https://cdn.example/b.mp4");

        // The final answer matches the single-shot resolve exactly, and
        // provider priority breaks the resolution tie.
        assert_eq!(progressive, engine.resolve(&media()).await?);
        assert_eq!(progressive.len(), 2);
        assert_eq!(progressive[0].url.as_str(), "https://cdn.example/a.mp4");
        Ok(())
    }

    #[tokio::test]
    async fn progressive_late_priority_takeover_keeps_one_stream_per_url() -> Result<(), EngineError>
    {
        // The low-priority provider claims the URL first; the
        // higher-priority provider takes it over when it lands — the
        // same winner the index-ordered single-shot merge picks, and
        // still exactly one row.
        let engine = test_builder()
            .sources(vec![
                StubSource::delayed(
                    "slow-hd",
                    10,
                    &["https://cdn.example/x.mp4"],
                    Duration::from_millis(150),
                ),
                StubSource::streaming("fast-sd", 1, &["https://cdn.example/x.mp4"]),
            ])
            .build()?;

        let (progress, mut receiver) = mpsc::unbounded_channel();
        let progressive = engine.resolve_progressive(&media(), progress).await?;
        assert_eq!(progressive.len(), 1);
        assert_eq!(progressive, engine.resolve(&media()).await?);

        // One snapshot for the claim, one for the takeover.
        let mut snapshots = 0;
        while receiver.try_recv().is_ok() {
            snapshots += 1;
        }
        assert_eq!(snapshots, 2);
        Ok(())
    }

    #[tokio::test]
    async fn not_found_providers_do_not_fail_the_engine() -> Result<(), EngineError> {
        let engine = test_builder()
            .sources(vec![
                StubSource::failing("missing", SourceError::NotFound),
                StubSource::streaming("has-it", 1, &["https://cdn.example/c.mp4"]),
            ])
            .build()?;

        let streams = engine.resolve(&media()).await?;
        assert_eq!(streams.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn all_providers_failing_is_an_error() -> Result<(), EngineError> {
        let engine = test_builder()
            .sources(vec![StubSource::failing(
                "broken",
                SourceError::scrape("broken", "boom"),
            )])
            .build()?;

        let result = engine.resolve(&media()).await;
        assert!(matches!(
            result,
            Err(EngineError::AllProvidersFailed { count: 1 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn slow_providers_are_abandoned_at_their_budget() -> Result<(), EngineError> {
        let engine = test_builder()
            .sources(vec![Arc::new(StubSource {
                info: info("slow", 0),
                streams: vec![direct_stream(
                    test_url("https://cdn.example/d.mp4"),
                    Format::Mp4,
                    Duration::from_secs(300),
                    &test_url("https://origin.example/"),
                )],
                error: None,
                delay: Some(Duration::from_secs(10)),
            })])
            .per_source_timeout(Duration::from_millis(10))
            .build()?;

        let result = engine.resolve(&media()).await;
        assert!(matches!(
            result,
            Err(EngineError::AllProvidersFailed { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn sorts_by_quality_height() {
        let low = direct_stream(
            test_url("https://cdn.example/720p.mp4"),
            Format::Mp4,
            Duration::from_secs(300),
            &test_url("https://origin.example/"),
        );
        let mut high = low.clone();
        high.meta.resolution = Some(2160);
        let mut mid = low.clone();
        mid.meta.resolution = Some(1080);

        let mut merged = vec![low, high, mid];
        for stream in &mut merged {
            enrich_stream(stream, "Inception.2010.2160p.BluRay.x265");
        }
        merged.sort_by_key(|stream| std::cmp::Reverse(stream.meta.resolution.unwrap_or(0)));
        assert_eq!(
            merged.iter().map(|s| s.meta.resolution).collect::<Vec<_>>(),
            vec![Some(2160), Some(1080), None]
        );
    }
    #[tokio::test]
    async fn dead_streams_never_enter_progressive_or_final_results() -> Result<(), EngineError> {
        struct ProbingFetcher;
        #[async_trait]
        impl Fetcher for ProbingFetcher {
            async fn request(
                &self,
                _: vsources_core::traits::FetchRequest,
            ) -> Result<vsources_core::traits::FetchResponse, vsources_core::error::FetchError>
            {
                panic!("only bounded probes expected")
            }
            async fn probe(
                &self,
                request: vsources_core::traits::FetchRequest,
                _: usize,
            ) -> Result<
                Option<vsources_core::traits::ProbeResponse>,
                vsources_core::error::FetchError,
            > {
                Ok(Some(vsources_core::traits::ProbeResponse {
                    status: if request.url.path() == "/dead.mp4" {
                        404
                    } else {
                        403
                    },
                    url: request.url,
                    headers: std::collections::BTreeMap::default(),
                    body: Vec::new(),
                    truncated: false,
                }))
            }
        }
        let engine = test_builder()
            .fetcher(Arc::new(ProbingFetcher))
            .sources(vec![StubSource::streaming(
                "test",
                0,
                &[
                    "https://cdn.example/dead.mp4",
                    "https://cdn.example/blocked.mp4",
                ],
            )])
            .build()?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = engine.resolve_progressive(&media(), tx).await?;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].url.path(), "/blocked.mp4");
        while let Ok(snapshot) = rx.try_recv() {
            assert_eq!(snapshot, result);
        }
        assert_eq!(engine.resolve(&media()).await?, result);
        Ok(())
    }
}
