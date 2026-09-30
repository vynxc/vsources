//! `CinebyRocks`: cineby.rocks via vidbolt's VidRock/FastVa scraper API.
//!
//! Ports `src/source/CinebyRocks.js` + `src/nuvio/cineby_rocks.cjs`
//! (movies, TV, anime with multi-server HLS up to 4K). cineby.rocks is
//! an SPA aggregating 8 embed player servers; the only one exposing a
//! public stream API is vidbolt (Cipher), whose `/api/scraper`
//! endpoint proxies the `VidRock` extractor family:
//!
//! 1. Resolve the TMDB id, name, and year.
//! 2. `GET https://vidbolt.xyz/api/scraper?path=/scrape/{FastVa|VidRock}
//!    /{movie|tv}/{tmdbId}?tmdbId=&title=&year=[&season=&episode=]`
//!    (Cloudflare-protected — the fetcher's browser fingerprint
//!    handles that; `Referer`/`Origin: cineby.rocks` attached) →
//!    `{ sources: [{url, quality, type, name, language, headers}] }`.
//!    `FastVa` is tried first (the current working backend), `VidRock` as
//!    the fallback in case vidbolt revives it.
//! 3. Each direct source becomes one card through
//!    [`build_stream_results`] with the wrapper's enrichment: quality
//!    normalization (`4K` → `2160p`, resolution fallbacks), the
//!    `1920x1080`-style title scan, `2160p → HEVC / else x264` codec
//!    markers, the audio language detected from the server name
//!    (`VidRock • Hindi` → `hi`), and the source's own hotlink
//!    headers (defaulting the UA).
//!
//! Cuts for the library port:
//!
//! - The scraper's 8 **iframe embed cards** are cut entirely — the
//!   upstream wrapper already filtered them out (Stremio cannot run
//!   cross-origin SPA iframes), so only direct playable streams ship.
//! - The scraper's resolution probe (m3u8 parse + an `ffprobe`
//!   subprocess over a downloaded segment) is cut — no binary
//!   dependencies in a library. Quality falls back to the API's label
//!   exactly like the JS's no-probe path (`4K` → `2160p`, else the
//!   label, else `720p`/`1080p` defaults).
//! - The TMDB anime probe (`original_language` + genre 16) is cut (the
//!   `TmdbClient` precedent), so the source-level codes stay
//!   `[multi, en]` and the audio default is `English`.
//! - `meta.title` becomes [`Stream::label`]; the per-stream
//!   `countryCodes` override is subsumed by `build_stream_results`'
//!   own title scan (the JS override and the scan produce the same
//!   set). No `/proxy`: source headers ride request headers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{BuildParams, NuvioStream, build_stream_results};

/// The provider id, upstream `this.id`.
const ID: &str = "cinebyrocks";
/// The display label, upstream `this.label`.
const LABEL: &str = "CinebyRocks";
/// The catalog origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://cineby.rocks";
/// The vidbolt scraper API.
const VIDBOLT_API: &str = "https://vidbolt.xyz/api/scraper";
/// Upstream `this.ttl` — stream URLs may have short-lived tokens.
const TTL: Duration = Duration::from_mins(10);
/// The scraper API timeout (upstream: 25 s).
const API_TIMEOUT: Duration = Duration::from_secs(25);
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// The scraper backends in priority order — `FastVa` is the current
/// working one; `VidRock` is the revival fallback.
const SCRAPERS: [&str; 2] = ["FastVa", "VidRock"];

/// `(\d{3,4})x(\d{3,4})` — the resolution inside a server name.
static TITLE_RESOLUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})x(\d{3,4})").unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// The `CinebyRocks` provider.
pub struct CinebyRocks {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl CinebyRocks {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// Fetch the direct sources from vidbolt, trying each scraper
    /// backend in order — the port of `fetchVidRockStreams`.
    async fn vidbolt_sources(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        name: &str,
        year: Option<u16>,
    ) -> Vec<VidRockSource> {
        let is_movie = media.season.is_none();
        let media_type = if is_movie { "movie" } else { "tv" };
        for scraper in SCRAPERS {
            // The inner scrape path carries its own query params.
            let mut scrape_path =
                format!("/scrape/{scraper}/{media_type}/{tmdb_id}?tmdbId={tmdb_id}&title={name}");
            if let Some(year) = year {
                let _ = write!(scrape_path, "&year={year}");
            }
            if let (Some(season), Some(episode)) = (media.season, media.episode) {
                let _ = write!(scrape_path, "&season={season}&episode={episode}");
            }
            let Ok(url) = Url::parse(VIDBOLT_API) else {
                continue;
            };
            let mut url = url;
            url.query_pairs_mut().append_pair("path", &scrape_path);
            let request = FetchRequest::get(url)
                .with_header("User-Agent", UA)
                .with_header("Accept", "application/json")
                .with_header("Referer", "https://cineby.rocks/")
                .with_header("Origin", "https://cineby.rocks")
                .with_timeout(API_TIMEOUT);
            let Ok(response) = ctx.fetcher.request(request).await else {
                continue;
            };
            if !response.is_success() {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<VidRockResponse>(&response.body) else {
                continue;
            };
            if !payload.sources.is_empty() {
                return payload
                    .sources
                    .into_iter()
                    .filter(|source| source.url.starts_with("http"))
                    .collect();
            }
        }
        Vec::new()
    }
}

#[async_trait]
impl Source for CinebyRocks {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let title = display_title(&name, year, media);

        let sources = self.vidbolt_sources(ctx, media, tmdb_id, &name, year).await;
        if sources.is_empty() {
            return Err(SourceError::NotFound);
        }

        // The wrapper's enrichment over each direct source: the
        // `Cineby - Cipher {name}` server label, the quality/codec/audio
        // markers, and the source's hotlink headers.
        let streams: Vec<NuvioStream> = sources.iter().map(enriched_stream).collect();
        let country_codes = vec![CountryCode::Multi, CountryCode::En];
        Ok(build_stream_results(&BuildParams {
            streams: &streams,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &country_codes,
            ttl: TTL,
        }))
    }
}

/// One direct source of the vidbolt API.
#[derive(Debug, Clone, Deserialize)]
struct VidRockSource {
    /// The direct m3u8/mp4 URL.
    url: String,
    /// The API's quality label (`4K`, `1080p`, `1920x1080`, …).
    #[serde(default)]
    quality: Option<String>,
    /// The stream type (`hls`, `mp4`).
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// The server name (`VidRock • Hindi`, `FastVa • English`).
    #[serde(default)]
    name: Option<String>,
    /// Per-source hotlink headers.
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

/// The vidbolt API response envelope.
#[derive(Deserialize)]
struct VidRockResponse {
    /// The direct sources.
    #[serde(default)]
    sources: Vec<VidRockSource>,
}

/// Build the wrapper's enriched card for one direct source — the
/// scraper's `convertVidRockSource`/`buildStream` collapsed with the
/// wrapper's re-enrichment (both produce the same fields).
fn enriched_stream(source: &VidRockSource) -> NuvioStream {
    let server = source.name.as_deref().unwrap_or_default();
    let server_label = format!("Cipher {server}");

    let is_mp4 = source.kind.as_deref() == Some("mp4") || source.url.contains(".mp4");
    // The probe is cut (see the module docs) — the no-probe fallbacks.
    let quality = source
        .quality
        .clone()
        .or_else(|| resolution_from_title(server))
        .unwrap_or_else(|| {
            if is_mp4 {
                "720p".to_string()
            } else {
                "1080p".to_string()
            }
        });
    let codec = if quality == "2160p" { "HEVC" } else { "x264" };

    let audio_lang = detect_audio_lang(server);
    let audio_label = audio_lang.map_or_else(|| "English".to_string(), title_case);

    let title = format!("[CinebyRocks {server_label}] {quality} WEB-DL {codec} {audio_label}");
    let mut stream = NuvioStream::new(source.url.clone())
        .with_quality(quality)
        .with_name(format!("CinebyRocks - {server_label}"))
        .with_title(title);
    // The source's hotlink headers — UA defaulted like the scraper.
    for (header, value) in &source.headers {
        stream = stream.with_header(header.clone(), value.clone());
    }
    if !source
        .headers
        .keys()
        .any(|header| header.eq_ignore_ascii_case("User-Agent"))
    {
        stream = stream.with_header("User-Agent", UA);
    }
    stream
}

/// The `1920x1080`-style resolution inside a server name → the
/// wrapper's height-bucketed quality label.
fn resolution_from_title(text: &str) -> Option<String> {
    let captures = TITLE_RESOLUTION.captures(text).ok().flatten()?;
    let height: u32 = captures.get(2)?.as_str().parse().ok()?;
    Some(
        if height >= 2160 {
            "2160p"
        } else if height >= 1080 {
            "1080p"
        } else if height >= 720 {
            "720p"
        } else if height >= 480 {
            "480p"
        } else {
            return None;
        }
        .to_string(),
    )
}

/// The wrapper's audio-language sniff from a server name —
/// `detectAudioLang`.
fn detect_audio_lang(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("hindi") {
        Some("hindi")
    } else if lower.contains("tamil") {
        Some("tamil")
    } else if lower.contains("telugu") {
        Some("telugu")
    } else if lower.contains("bengali") {
        Some("bengali")
    } else if lower.contains("english") {
        Some("english")
    } else if lower.contains("japanese") {
        Some("japanese")
    } else if lower.contains("korean") {
        Some("korean")
    } else if lower.contains("chinese") {
        Some("chinese")
    } else {
        None
    }
}

/// `hindi` → `Hindi` (the wrapper's capitalized label).
fn title_case(lang: &str) -> String {
    let mut chars = lang.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the display title.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn tmdb_id(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
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
    tmdb: &TmdbClient,
    media: &MediaRef,
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
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher};
    use vsources_core::types::{Format, MediaId};

    use super::*;

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare path,
    /// so TMDB requests (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<(u16, String)>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `status`/`body`; earlier registrations pop
        /// first.
        fn page(self, key: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push((status, body.into()));
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// The `path` query parameter of a request (decoded).
        fn path_param(request: &FetchRequest) -> String {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "path")
                .map(|(_, value)| value.to_string())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = match request.url.query() {
                Some(query) => format!("{}?{query}", request.url.path()),
                None => request.url.path().to_string(),
            };
            let entry = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let entry = match entry {
                Some(entry) => Some(entry),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_mut(request.url.path())
                    .map(|bodies| {
                        if bodies.len() > 1 {
                            bodies.remove(0)
                        } else {
                            bodies[0].clone()
                        }
                    }),
            };
            let Some((status, body)) = entry else {
                return Err(FetchError::NotFound { url: request.url });
            };
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> CinebyRocks {
        CinebyRocks::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
    }

    /// A context over the scripted fetcher.
    fn ctx_for(mock: &Arc<ScriptedFetcher>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = mock.as_ref();
        ResolveCtx {
            fetcher,
            media: None,
            source_id: None,
            referer: None,
        }
    }

    /// The fixture media (Inception, TMDB 27205).
    const TMDB_ID: u64 = 27205;

    /// TMDB details for the fixture media.
    fn tmdb_mock() -> ScriptedFetcher {
        ScriptedFetcher::default().page(
            format!("/3/movie/{TMDB_ID}"),
            200,
            r#"{"title":"Inception","release_date":"2010-07-16"}"#,
        )
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "cinebyrocks");
        assert_eq!(info.label, "CinebyRocks");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://cineby.rocks/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_direct_streams_with_enrichment() -> Result<(), SourceError> {
        let mock = Arc::new(
            tmdb_mock().page(
                "/api/scraper",
                200,
                r#"{"sources":[
                    {"url":"https://gigle432ski.com/hls/movie/master.m3u8","quality":"1080p","type":"hls","name":"VidRock • English"},
                    {"url":"https://ngcorp.dad/hls/movie/hindi/master.m3u8","quality":"4K","type":"hls","name":"VidRock • Hindi","headers":{"Referer":"https://ngcorp.dad/"}}
                ]}"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 2);
        // No sort upstream — the API order stands. The English card.
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert!(streams[0].meta.languages.contains(&CountryCode::En));
        assert_eq!(streams[0].ttl, TTL);
        // The `4K` label passes through raw — `parse_height` still
        // resolves it to 2160, and the codec falls to x264 like the JS.
        assert_eq!(streams[1].meta.resolution, Some(2160));
        assert_eq!(streams[1].format, Format::Hls);
        assert_eq!(
            streams[1].url.as_str(),
            "https://ngcorp.dad/hls/movie/hindi/master.m3u8"
        );
        // The wrapper's title with quality/WEB-DL/codec/audio markers.
        assert!(streams[1].label.as_deref().is_some_and(|label| {
            label.contains("[CinebyRocks Cipher VidRock • Hindi] 4K WEB-DL x264 Hindi")
        }));
        // The Hindi audio flag arrives via the title scan.
        assert!(streams[1].meta.languages.contains(&CountryCode::Hi));
        // The source's Referer rides the request headers.
        assert_eq!(
            streams[1]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://ngcorp.dad/")
        );
        // The default UA when the source ships none.
        assert_eq!(
            streams[1]
                .meta
                .request_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(UA)
        );
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("cinebyrocks"))
        );
        // The FastVa backend was queried first.
        let first = mock
            .requests()
            .into_iter()
            .find(|request| request.url.path() == "/api/scraper")
            .unwrap_or_else(|| panic!("the scraper API was queried"));
        assert!(ScriptedFetcher::path_param(&first).contains("FastVa"));
        Ok(())
    }

    #[tokio::test]
    async fn falls_back_to_vidrock_when_fastva_is_empty() -> Result<(), SourceError> {
        let mock = Arc::new(
            tmdb_mock()
                .page("/api/scraper", 200, r#"{"sources":[]}"#)
                .page(
                    "/api/scraper",
                    200,
                    r#"{"sources":[{"url":"https://dolphin-55.workers.dev/hls/master.m3u8","quality":"720p","type":"hls","name":"VidRock • English"}]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        let scraper_calls: Vec<String> = mock
            .requests()
            .iter()
            .filter(|request| request.url.path() == "/api/scraper")
            .map(ScriptedFetcher::path_param)
            .collect();
        assert_eq!(scraper_calls.len(), 2);
        assert!(scraper_calls[0].contains("FastVa"));
        assert!(scraper_calls[1].contains("VidRock"));
        Ok(())
    }

    #[tokio::test]
    async fn a_series_reference_carries_season_and_episode() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Dark","first_air_date":"2017-12-01"}"#,
                )
                .page(
                    "/api/scraper",
                    200,
                    r#"{"sources":[{"url":"https://gigle432ski.com/hls/tv/master.m3u8","quality":"1080p","type":"hls","name":"VidRock • English"}]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        let scrape = mock
            .requests()
            .iter()
            .find(|request| request.url.path() == "/api/scraper")
            .map(ScriptedFetcher::path_param)
            .unwrap_or_default();
        assert!(scrape.contains("/scrape/FastVa/tv/"));
        assert!(scrape.contains("season=1"));
        assert!(scrape.contains("episode=2"));
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Dark S01E02"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_scraper_api_failure_answers_not_found() {
        let mock = Arc::new(tmdb_mock());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(404)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[test]
    fn parses_resolutions_and_audio_from_titles() {
        assert_eq!(
            resolution_from_title("VidRock • Hindi 1920x1080"),
            Some("1080p".to_string())
        );
        assert_eq!(
            resolution_from_title("VidRock 3840x2160"),
            Some("2160p".to_string())
        );
        assert_eq!(resolution_from_title("VidRock • English"), None);
        assert_eq!(detect_audio_lang("VidRock • Hindi"), Some("hindi"));
        assert_eq!(detect_audio_lang("VidRock • Tamil"), Some("tamil"));
        assert_eq!(detect_audio_lang("VidRock • Lyra"), None);
        assert_eq!(title_case("hindi"), "Hindi".to_string());
    }
}
