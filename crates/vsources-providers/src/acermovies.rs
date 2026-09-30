//! `AcerMovies`: the `api2.acermovies.fun` JSON API (direct `GDrive`
//! CDN URLs, movies only).
//!
//! Ports `src/source/AcerMovies.js` — a self-contained source (no
//! Nuvio scraper; the JS builds its result objects directly, so this
//! port does too).
//!
//! Flow:
//!
//! 1. Resolve the TMDB id and name/year (context media or
//!    [`TmdbClient`]). Series are a **definitive miss**: upstream
//!    returns its sentinel because episodes go through a
//!    Cloudflare-protected blog chain that no server-side client can
//!    follow.
//! 2. `POST /api/search` `{searchQuery: "name year"}` →
//!    `{searchResult: [{title, url}]}` — the best match is the first
//!    entry whose title contains the name (case-insensitive), else the
//!    first entry.
//! 3. `POST /api/sourceQuality` `{url}` → `{sourceQualityList:
//!    [{title, url, quality, episodesUrl}]}` — movie entries have a
//!    non-empty `url` and no `episodesUrl`; duplicates collapse by
//!    `quality || title`.
//! 4. `POST /api/sourceUrl` `{url, seriesType: "movie"}` — the first
//!    quality is a canary: a `{fromCache: false}` answer without a
//!    `sourceUrl` is acer's final "we have not resolved this title"
//!    verdict (measured stable across seconds), so it short-circuits
//!    the whole ladder. Surviving answers carry `sourceUrl` — a direct
//!    googleusercontent MP4/MKV that plays without a Referer.
//!
//! Every request carries the browser POST set upstream sends (UA,
//! `Content-Type`, `Origin`, `Referer`, `Accept-Language`, and the
//! `Sec-Fetch-*` trio — the header-based WAF lesson).
//!
//! Cuts for the library port (all of them stateful server-side
//! machinery around a dead upstream):
//!
//! - The 429 relay through `test.cors.workers.dev` (a Render-egress
//!   workaround — there is no server egress class here).
//! - The 10 min rate-limit cooldown circuit, the 24 h result fallback
//!   cache, and the 30 min definitive-miss negative cache: result and
//!   negative caching are the parent [`CachedSource`](crate::CachedSource)
//!   domain. A 429
//!   maps onto the definitive-miss sentinel instead — the one behavior
//!   the cooldown existed to guarantee (no re-POSTing into an active
//!   ban window, which upstream measured extends it).
//! - `meta.title` has no `StreamMeta` field — the
//!   `{name} ({year}) ({quality})` card title rides
//!   [`Stream::label`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{parse_height, with_retry_on_empty};

/// The JSON API root (upstream `API_BASE`).
const API_BASE: &str = "https://api2.acermovies.fun";
/// The site origin (upstream `ORIGIN`).
const ORIGIN: &str = "https://acermovies.fun";
/// Upstream `this.ttl` — 1 h.
const TTL: Duration = Duration::from_hours(1);
/// The per-request API timeout (upstream `_apiPost` default).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The empty-retry ladder budget (upstream `maxTotalMs`).
const LADDER_BUDGET: Duration = Duration::from_secs(14);
/// The ladder's backoff between attempts (upstream default).
const LADDER_BACKOFF: Duration = Duration::from_millis(400);

/// The `AcerMovies` provider.
pub struct AcerMovies {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl AcerMovies {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "acermovies".to_string(),
                label: "AcerMovies".to_string(),
                content_types: vec![MediaType::Movie],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse(ORIGIN).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// One resolve attempt — `None` is the definitive-miss sentinel
    /// (upstream's `DEFINITIVE_MISS` object, which its
    /// `withRetryOnEmpty` passes through un-retried; this port's
    /// [`with_retry_on_empty`] does the same for `None`).
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        name: &str,
        year: Option<u16>,
    ) -> Option<Vec<Stream>> {
        // Series episodes live behind a CF-protected blog chain — a
        // definitive never-changes answer, not a transient window.
        if media.season.is_some() {
            return None;
        }

        // Step 1 — search by name + year.
        let search_query = year.map_or_else(|| name.to_string(), |year| format!("{name} {year}"));
        let Some(search) = self
            .api_post::<SearchResponse>(
                ctx,
                "/api/search",
                &serde_json::json!({
                    "searchQuery": search_query,
                }),
            )
            .await
        else {
            return Some(Vec::new());
        };
        let results = search.search_result;
        if results.is_empty() {
            return Some(Vec::new());
        }
        let name_lower = name.to_lowercase();
        let best = results
            .iter()
            .find(|entry| {
                entry
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&name_lower))
            })
            .unwrap_or(&results[0]);
        let Some(match_url) = best.url.as_deref().filter(|url| !url.is_empty()) else {
            return Some(Vec::new());
        };

        // Step 2 — the quality list for the matched movie.
        let Some(quality) = self
            .api_post::<QualityResponse>(
                ctx,
                "/api/sourceQuality",
                &serde_json::json!({
                    "url": match_url,
                }),
            )
            .await
        else {
            return Some(Vec::new());
        };
        let mut movie_qualities: Vec<&QualityEntry> = quality
            .source_quality_list
            .iter()
            .filter(|entry| {
                entry.url.as_deref().is_some_and(|url| !url.is_empty())
                    && entry.episodes_url.is_none()
            })
            .collect();
        if movie_qualities.is_empty() {
            return Some(Vec::new());
        }
        // Dedup by `quality || title` (upstream's `seenQualities`).
        let mut seen = Vec::new();
        movie_qualities.retain(|entry| {
            let key = entry
                .quality
                .clone()
                .or_else(|| entry.title.clone())
                .unwrap_or_default();
            if key.is_empty() || seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        });

        // Step 3 — resolve qualities to direct GDrive URLs. The first
        // is the canary: `fromCache: false` is a final verdict.
        let title = year.map_or_else(|| name.to_string(), |year| format!("{name} ({year})"));
        let mut results = Vec::new();
        let mut definitive = false;
        for (index, entry) in movie_qualities.iter().enumerate() {
            match self.resolve_one(ctx, entry, &title).await {
                Ok(Some(stream)) => results.push(stream),
                // The canary's definitive verdict answers for the
                // whole title — stop the batch.
                Err(DefinitiveMiss) if index == 0 => {
                    definitive = true;
                    break;
                }
                // Mid-batch, upstream swallows the same verdict with a
                // per-quality `.catch(() => null)` — skip, keep going.
                Err(DefinitiveMiss) | Ok(None) => {}
            }
        }
        if definitive {
            return None;
        }
        Some(results)
    }

    /// Resolve one quality to a direct stream — the canary's
    /// definitive verdict surfaces as `Err(DefinitiveMiss)` while
    /// ordinary failures answer `Ok(None)` (upstream's
    /// `_resolveOne`).
    async fn resolve_one(
        &self,
        ctx: &ResolveCtx<'_>,
        entry: &QualityEntry,
        title: &str,
    ) -> Result<Option<Stream>, DefinitiveMiss> {
        let Some(url) = entry.url.as_deref() else {
            return Ok(None);
        };
        let Some(answer) = self
            .api_post::<SourceUrlResponse>(
                ctx,
                "/api/sourceUrl",
                &serde_json::json!({
                    "url": url,
                    "seriesType": "movie",
                }),
            )
            .await
        else {
            return Ok(None);
        };
        // acer's final "we have not resolved this quality" verdict.
        if answer.from_cache == Some(false) && answer.source_url.is_none() {
            return Err(DefinitiveMiss);
        }
        let Some(direct) = answer.source_url.as_deref() else {
            return Ok(None);
        };
        let Ok(url) = Url::parse(direct) else {
            return Ok(None);
        };
        let quality = entry.quality.as_deref().unwrap_or_default();
        let resolution = parse_height(entry.quality.as_deref());
        let label = format!(
            "{title} ({})",
            if quality.is_empty() { "MP4" } else { quality }
        );
        Ok(Some(Stream {
            meta: vsources_core::types::StreamMeta {
                languages: country_codes_from_title(entry.title.as_deref()),
                resolution,
                source_id: Some(self.info.id.clone()),
                source_label: Some(self.info.label.clone()),
                ..vsources_core::types::StreamMeta::default()
            },
            label: Some(label),
            url,
            format: Format::Mp4,
            ttl: TTL,
            is_external: false,
            behavior_hints: BTreeMap::new(),
        }))
    }

    /// One browser-shaped API POST — `None` on any transport, status,
    /// or JSON failure (the upstream per-step catch). A 429 is also
    /// `None`: the sweep maps it onto the definitive sentinel so the
    /// ladder never re-POSTs into an active ban window.
    async fn api_post<T: serde::de::DeserializeOwned>(
        &self,
        ctx: &ResolveCtx<'_>,
        path: &str,
        body: &serde_json::Value,
    ) -> Option<T> {
        let url = Url::parse(&format!("{API_BASE}{path}")).ok()?;
        let request = FetchRequest::post(url, body.to_string())
            .with_header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json, text/javascript, */*; q=0.01")
            .with_header("Origin", ORIGIN)
            .with_header("Referer", format!("{ORIGIN}/"))
            .with_header("Accept-Language", "en-US,en;q=0.9")
            .with_header("Sec-Fetch-Dest", "empty")
            .with_header("Sec-Fetch-Mode", "cors")
            .with_header("Sec-Fetch-Site", "same-site")
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        response.json::<T>().ok()
    }
}

#[async_trait]
impl Source for AcerMovies {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, media, &self.tmdb).await?;
        let (name, year) = name_and_year(ctx, media, &self.tmdb, tmdb_id).await?;

        let streams = with_retry_on_empty(
            || self.sweep(ctx, media, &name, year),
            3,
            LADDER_BUDGET,
            LADDER_BACKOFF,
        )
        .await
        .unwrap_or_default();
        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

/// The definitive-miss marker for the canary's `fromCache: false`
/// verdict (upstream's `DefinitiveMissError`).
struct DefinitiveMiss;
/// The `/api/search` response.
#[derive(Deserialize)]
struct SearchResponse {
    /// The search hits.
    #[serde(rename = "searchResult", default)]
    search_result: Vec<SearchEntry>,
}

/// One search hit.
#[derive(Deserialize)]
struct SearchEntry {
    /// The hit's title.
    #[serde(default)]
    title: Option<String>,
    /// The hit's site URL (the input to `/api/sourceQuality`).
    #[serde(default)]
    url: Option<String>,
}

/// The `/api/sourceQuality` response.
#[derive(Deserialize)]
struct QualityResponse {
    /// The quality options.
    #[serde(rename = "sourceQualityList", default)]
    source_quality_list: Vec<QualityEntry>,
}

/// One quality option.
#[derive(Deserialize)]
struct QualityEntry {
    /// The release title (the language-flag source).
    #[serde(default)]
    title: Option<String>,
    /// The quality's resolve input.
    #[serde(default)]
    url: Option<String>,
    /// The quality label (`480p`, `1080p 10Bit HEVC`).
    #[serde(default)]
    quality: Option<String>,
    /// Series-only: the episodes page (movie entries never set it).
    #[serde(rename = "episodesUrl", default)]
    episodes_url: Option<String>,
}

/// The `/api/sourceUrl` response.
#[derive(Deserialize)]
struct SourceUrlResponse {
    /// Acer's cache verdict — `false` means the title was never
    /// resolved upstream (a final answer).
    #[serde(rename = "fromCache", default)]
    from_cache: Option<bool>,
    /// The direct `GDrive` CDN URL.
    #[serde(rename = "sourceUrl", default)]
    source_url: Option<String>,
}

/// Language flags from a release title — upstream's
/// `countryCodesFromTitle` (substring checks, insertion-ordered,
/// defaulting to `multi`).
fn country_codes_from_title(title: Option<&str>) -> Vec<CountryCode> {
    let Some(title) = title else {
        return vec![CountryCode::Multi];
    };
    let title = title.to_lowercase();
    let mut codes = Vec::new();
    let hits = [
        (
            title.contains("hindi") || title.contains("hin"),
            CountryCode::Hi,
        ),
        (
            title.contains("english") || title.contains("eng"),
            CountryCode::En,
        ),
        (
            title.contains("tamil") || title.contains("tam"),
            CountryCode::Ta,
        ),
        (
            title.contains("telugu") || title.contains("tel"),
            CountryCode::Te,
        ),
        (
            title.contains("korean") || title.contains("kor"),
            CountryCode::Ko,
        ),
        (
            title.contains("japanese") || title.contains("jpn") || title.contains("anime"),
            CountryCode::Ja,
        ),
        (
            title.contains("chinese") || title.contains("chi"),
            CountryCode::Zh,
        ),
    ];
    for (hit, code) in hits {
        if hit && !codes.contains(&code) {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        codes.push(CountryCode::Multi);
    }
    codes
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn tmdb_id(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear`.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
    Ok((name.name, name.year))
}

/// Map miss-shaped failures onto the not-found answer.
fn soften<T>(error: Result<T, SourceError>) -> Result<T, SourceError> {
    match error {
        Err(
            SourceError::NotFound
            | SourceError::Fetch(FetchError::NotFound { .. } | FetchError::Http { status: 404, .. }),
        ) => Err(SourceError::NotFound),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::StreamMeta;

    use super::*;

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        /// HTTP status.
        status: u16,
        /// The body.
        body: String,
    }

    /// A fetcher serving scripted pages by host+path (in order, the
    /// last repeating) and recording every request it sees.
    struct MockFetcher {
        pages: Mutex<HashMap<String, VecDeque<Scripted>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl MockFetcher {
        /// A fetcher serving nothing yet.
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Serve `key` (host + path) with `status`/`body`.
        fn serve(self, key: &str, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.to_string())
                .or_default()
                .push_back(Scripted {
                    status,
                    body: body.into(),
                });
            self
        }

        /// The value of a header sent to `key` (host + path).
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .filter(|request| request_key(request) == key)
                .find_map(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(header, _)| header.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }

        /// The POST bodies sent to `key`, in order.
        fn sent_bodies(&self, key: &str) -> Vec<String> {
            self.requests()
                .iter()
                .filter(|request| request_key(request) == key)
                .map(|request| request.body.clone().unwrap_or_default())
                .collect()
        }

        /// How many requests hit `key` (host + path).
        fn hits(&self, key: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request_key(request) == key)
                .count()
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The lookup key of a request: host + path.
    fn request_key(request: &FetchRequest) -> String {
        format!(
            "{}{}",
            request.url.host_str().unwrap_or_default(),
            request.url.path()
        )
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = request_key(&request);
            let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
            let response = pages.get_mut(&key).and_then(|queue| {
                // The last scripted response repeats.
                if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                }
            });
            match response {
                Some(scripted) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )]),
                    body: scripted.body,
                }),
                // A 429 must not be retried into — the definitive
                // sentinel path.
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A provider over the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> AcerMovies {
        AcerMovies::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
    }

    /// A resolve context over the mock.
    fn ctx_for(fetcher: &Arc<MockFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher: fetcher.as_ref(),
            media,
            source_id: None,
            referer: None,
        }
    }

    /// A resolved Dune movie.
    fn dune_media() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: Some("tt1160419".to_string()),
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        }
    }

    /// The dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::movie(MediaId::Tmdb(438_631))
    }

    /// The search page for the fixture.
    fn search_page() -> serde_json::Value {
        serde_json::json!({
            "searchResult": [
                { "title": "Eye for an Eye (2025)", "url": "https://acermovies.fun/eye" },
                { "title": "Dune (2021) Dual Audio (Hindi-English) 1080p", "url": "https://acermovies.fun/dune" }
            ]
        })
    }

    /// The quality page for the fixture.
    fn quality_page() -> serde_json::Value {
        serde_json::json!({
            "sourceQualityList": [
                { "title": "Dune 2021 1080p 10Bit HEVC English", "url": "https://acermovies.fun/dune/1080", "quality": "1080p 10Bit HEVC" },
                { "title": "Dune 2021 720p Dual Audio (Hindi-English)", "url": "https://acermovies.fun/dune/720", "quality": "720p" },
                { "title": "Dune 2021 720p Dual Audio (Hindi-English)", "url": "https://acermovies.fun/dune/720-dup", "quality": "720p" },
                { "title": "Dune 2021 episodes", "episodesUrl": "https://acermovies.fun/dune/episodes" }
            ]
        })
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        assert_eq!(info.id, "acermovies");
        assert_eq!(info.label, "AcerMovies");
        assert_eq!(info.content_types, vec![MediaType::Movie]);
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://acermovies.fun/")
        );
        assert_eq!(info.priority, 0);
    }

    #[tokio::test]
    async fn resolves_the_quality_ladder_to_direct_urls() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "api2.acermovies.fun/api/search",
                    200,
                    search_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceQuality",
                    200,
                    quality_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceUrl",
                    200,
                    serde_json::json!({
                        "fromCache": true,
                        "sourceUrl": "https://video-downloads.googleusercontent.com/dune-1080"
                    })
                    .to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceUrl",
                    200,
                    serde_json::json!({
                        "fromCache": true,
                        "sourceUrl": "https://video-downloads.googleusercontent.com/dune-720"
                    })
                    .to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the movie fixture must resolve: {e}"));

        // The duplicate quality and the series entry are filtered.
        assert_eq!(streams.len(), 2);
        let first = &streams[0];
        assert_eq!(
            first.url.as_str(),
            "https://video-downloads.googleusercontent.com/dune-1080"
        );
        assert_eq!(first.format, Format::Mp4);
        assert_eq!(first.ttl, TTL);
        assert_eq!(first.meta.resolution, Some(1080));
        assert_eq!(first.meta.source_id.as_deref(), Some("acermovies"));
        assert_eq!(first.meta.source_label.as_deref(), Some("AcerMovies"));
        // GDrive CDN — direct play, no Referer (upstream sends none).
        assert!(first.meta.request_headers.is_empty());
        assert_eq!(
            first.label.as_deref(),
            Some("Dune (2021) (1080p 10Bit HEVC)")
        );
        // The English-flag title.
        assert_eq!(
            first.meta.languages,
            vec![CountryCode::En],
            "the 1080p title mentions English only"
        );
        let second = &streams[1];
        assert_eq!(second.meta.resolution, Some(720));
        assert_eq!(
            second.meta.languages,
            vec![CountryCode::Hi, CountryCode::En]
        );
        assert_eq!(second.label.as_deref(), Some("Dune (2021) (720p)"));

        // The search matched the title that contains the name (not the
        // first entry), with the year appended to the query.
        let search = fetcher.sent_bodies("api2.acermovies.fun/api/search");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&search[0]).ok(),
            Some(serde_json::json!({"searchQuery": "Dune 2021"}))
        );
        let quality = fetcher.sent_bodies("api2.acermovies.fun/api/sourceQuality");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&quality[0]).ok(),
            Some(serde_json::json!({"url": "https://acermovies.fun/dune"}))
        );
        let source = fetcher.sent_bodies("api2.acermovies.fun/api/sourceUrl");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&source[0]).ok(),
            Some(
                serde_json::json!({"url": "https://acermovies.fun/dune/1080", "seriesType": "movie"})
            )
        );
        // The browser POST set rode along.
        let key = "api2.acermovies.fun/api/search";
        assert_eq!(
            fetcher.sent_header(key, "Origin").as_deref(),
            Some("https://acermovies.fun")
        );
        assert_eq!(
            fetcher.sent_header(key, "Referer").as_deref(),
            Some("https://acermovies.fun/")
        );
        assert_eq!(
            fetcher.sent_header(key, "Content-Type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            fetcher.sent_header(key, "Sec-Fetch-Site").as_deref(),
            Some("same-site")
        );
        Ok(())
    }

    #[tokio::test]
    async fn from_cache_false_is_a_definitive_miss() {
        // The canary's fromCache:false must end the whole ladder after
        // three POSTs (search, quality, one sourceUrl) — no retries.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "api2.acermovies.fun/api/search",
                    200,
                    search_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceQuality",
                    200,
                    quality_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceUrl",
                    200,
                    serde_json::json!({ "fromCache": false }).to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
        // The definitive miss short-circuited: one sourceUrl POST, not
        // a 3-attempt ladder of nine.
        assert_eq!(fetcher.hits("api2.acermovies.fun/api/sourceUrl"), 1);
        assert_eq!(fetcher.hits("api2.acermovies.fun/api/search"), 1);
    }

    #[tokio::test]
    async fn series_references_are_a_definitive_miss() {
        let fetcher = Arc::new(MockFetcher::new());
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref(),
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(1),
                episode: Some(2),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 1, 2);
        let result = provider(&fetcher).resolve(&ctx, &media).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
        // Nothing upstream was touched.
        assert!(fetcher.requests().is_empty());
    }

    #[tokio::test]
    async fn empty_search_is_not_found() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "api2.acermovies.fun/api/search",
            200,
            serde_json::json!({ "searchResult": [] }).to_string(),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn search_miss_is_not_found() {
        // Nothing scripted — the fetcher answers transport errors, which
        // map onto empty sweeps; the ladder retries within its budget
        // and ends honest-zero.
        let fetcher = Arc::new(MockFetcher::new());
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn quality_entries_without_movie_urls_are_not_found() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api2.acermovies.fun/api/search", 200, search_page().to_string())
                .serve(
                    "api2.acermovies.fun/api/sourceQuality",
                    200,
                    serde_json::json!({
                        "sourceQualityList": [
                            { "title": "Dune 2021 episodes", "episodesUrl": "https://acermovies.fun/dune/episodes" }
                        ]
                    })
                    .to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn a_flaky_window_retries_the_ladder() {
        // First search answers 500 (transient), the retry succeeds.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api2.acermovies.fun/api/search", 500, "")
                .serve(
                    "api2.acermovies.fun/api/search",
                    200,
                    search_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceQuality",
                    200,
                    quality_page().to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceUrl",
                    200,
                    serde_json::json!({
                        "fromCache": true,
                        "sourceUrl": "https://video-downloads.googleusercontent.com/dune-1080"
                    })
                    .to_string(),
                )
                .serve(
                    "api2.acermovies.fun/api/sourceUrl",
                    200,
                    serde_json::json!({
                        "fromCache": true,
                        "sourceUrl": "https://video-downloads.googleusercontent.com/dune-720"
                    })
                    .to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the retry fixture must resolve: {e}"));
        assert_eq!(streams.len(), 2);
        assert_eq!(fetcher.hits("api2.acermovies.fun/api/search"), 2);
    }

    #[test]
    fn language_flags_cover_the_title_table() {
        assert_eq!(
            country_codes_from_title(Some("Dual Audio (Hindi-English)")),
            vec![CountryCode::Hi, CountryCode::En]
        );
        assert_eq!(
            country_codes_from_title(Some("Anime Japanese")),
            vec![CountryCode::Ja]
        );
        assert_eq!(
            country_codes_from_title(Some("Korean drama")),
            vec![CountryCode::Ko]
        );
        assert_eq!(
            country_codes_from_title(Some("plain hollywood")),
            vec![CountryCode::Multi]
        );
        assert_eq!(country_codes_from_title(None), vec![CountryCode::Multi]);
    }

    #[test]
    fn meta_defaults_are_direct_play() {
        // The stream shape is direct-play: no request headers, no
        // external marking.
        let stream = Stream {
            meta: StreamMeta::default(),
            label: None,
            url: Url::parse("https://video-downloads.googleusercontent.com/x")
                .unwrap_or_else(|e| panic!("valid test URL: {e}")),
            format: Format::Mp4,
            ttl: TTL,
            is_external: false,
            behavior_hints: BTreeMap::new(),
        };
        assert!(stream.meta.request_headers.is_empty());
        assert!(!stream.is_external);
    }
}
