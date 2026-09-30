//! `PlayIMDb`: the `streamdata.vaplayer.ru` HLS/MP4 backend.
//!
//! Ports `src/source/PlayImdb.js` + its Nuvio scraper
//! `src/nuvio/playimdb.cjs` (obfuscated upstream; the flow below was
//! recovered by driving the live module with scripted responses).
//!
//! Flow:
//!
//! 1. Resolve the TMDB id and name/year (context media or
//!    [`TmdbClient`] — the upstream `getTmdbId`/`getTmdbNameAndYear`).
//! 2. `GET https://streamdata.vaplayer.ru/api.php?tmdb={id}&type={movie|tv}`
//!    (series append `&season={s}&episode={e}`) with
//!    `Origin`/`Referer: https://nextgencloudfabric.com` and a browser
//!    `User-Agent` — the scraper's exact header set.
//! 3. The answer must be `{status_code: 200, data: {stream_urls: [url…]}}`;
//!    every other `status_code` (or a missing `data`) is an honest zero.
//!    Each URL becomes one card: quality is hardcoded `1080p` (the
//!    scraper never inspects the stream), the label is the scraper's
//!    emoji card, and the hotlink headers
//!    (`Origin`/`Referer`/`User-Agent`) ride the stream.
//! 4. The cards run through
//!    `build_stream_results` —
//!    non-http entries (the scraper emits them verbatim) are dropped
//!    there, exactly like the upstream wrapper.
//!
//! Cuts for the library port:
//!
//! - `callNuvioProvider`'s 25 s timeout race becomes
//!   `with_deadline` around the sweep.
//! - The scraper's own TMDB details fetch (for the runtime in the card
//!   label) is cut: the shared [`TmdbClient`] exposes no runtime, so
//!   the label carries the scraper's 90-minute fallback, and the
//!   name/year come from the context/client instead.
//! - `meta.title` has no `StreamMeta` field — the emoji card title
//!   rides [`Stream::label`], composed by `build_stream_results`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::FetchError;
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{BuildParams, NuvioStream, build_stream_results, with_deadline};
/// The stream data API (upstream `STREAMDATA_API`).
const API: &str = "https://streamdata.vaplayer.ru/api.php";
/// The origin the API gates on.
const ORIGIN: &str = "https://nextgencloudfabric.com";
/// The hotlink Referer (the origin with a trailing slash, like the JS).
const REFERER: &str = "https://nextgencloudfabric.com/";
/// The browser `User-Agent` the scraper sends.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// Upstream `this.ttl` — 10 min.
const TTL: Duration = Duration::from_mins(10);
/// `callNuvioProvider`'s default timeout.
const DEADLINE: Duration = Duration::from_secs(25);
/// The scraper's runtime fallback when TMDB details lack one.
const DEFAULT_RUNTIME_MIN: u32 = 90;

/// The `PlayIMDb` provider.
pub struct PlayImdb {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl PlayImdb {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "playimdb".to_string(),
                label: "PlayIMDb".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse("https://playimdb.com").ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// The API sweep — one GET, the response-shape guard, and the card
    /// fan. An empty sweep is the upstream honest zero; the deadline
    /// wrap produces the same answer on timeout.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        name: &str,
        year: Option<u16>,
    ) -> Vec<Stream> {
        let Ok(target) = api_url(media) else {
            return Vec::new();
        };
        let request = FetchRequest::get(target)
            .with_header("Origin", ORIGIN)
            .with_header("Referer", REFERER)
            .with_header("User-Agent", USER_AGENT)
            .with_header("Accept", "application/json")
            .with_timeout(DEADLINE);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(payload) = response.json::<ApiResponse>() else {
            return Vec::new();
        };
        let Some(urls) = payload.stream_urls() else {
            return Vec::new();
        };

        let raw: Vec<NuvioStream> = urls
            .iter()
            .enumerate()
            .map(|(index, url)| card(url, name, year, media, index + 1))
            .collect();
        let params = BuildParams {
            streams: &raw,
            title: &title_line(name, year, media),
            source_id: &self.info.id,
            source_label: &self.info.label,
            country_codes: &self.info.country_codes,
            ttl: TTL,
        };
        build_stream_results(&params)
    }
}

#[async_trait]
impl Source for PlayImdb {
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

        let streams = with_deadline(self.sweep(ctx, media, &name, year), DEADLINE)
            .await
            .unwrap_or_default();
        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

/// The API response envelope — `{status_code, data: {stream_urls}}`.
#[derive(Deserialize)]
struct ApiResponse {
    /// The API's own status gate; only `200` carries data.
    #[serde(default)]
    status_code: Option<Value>,
    /// The stream URLs container.
    #[serde(default)]
    data: Option<ApiData>,
}

/// The `data` object.
#[derive(Deserialize)]
struct ApiData {
    /// The raw stream URL list (strings, possibly non-http — the
    /// scraper emits them verbatim and the wrapper drops them).
    #[serde(rename = "stream_urls", default)]
    stream_urls: Option<Vec<String>>,
}

impl ApiResponse {
    /// The stream URLs, but only for the success status — any other
    /// `status_code` (or a missing envelope) is the scraper's zero.
    fn stream_urls(&self) -> Option<&[String]> {
        if self.status_code.as_ref().and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) != Some(200)
        {
            return None;
        }
        self.data
            .as_ref()?
            .stream_urls
            .as_deref()
            .filter(|urls| !urls.is_empty())
    }
}

/// The API URL for a reference — `?tmdb={id}&type=…[&season&episode]`.
fn api_url(media: &MediaRef) -> Result<Url, url::ParseError> {
    let id = match &media.id {
        MediaId::Tmdb(id) => id.to_string(),
        MediaId::Imdb(imdb) => imdb.clone(),
    };
    let base = if media.season.is_some() {
        format!(
            "{API}?tmdb={id}&type=tv&season={}&episode={}",
            media.season.unwrap_or(1),
            media.episode.unwrap_or(1)
        )
    } else {
        format!("{API}?tmdb={id}&type=movie")
    };
    Url::parse(&base)
}

/// One card — the scraper's fixed `1080p` label, emoji title, and
/// hotlink headers.
fn card(url: &str, name: &str, year: Option<u16>, media: &MediaRef, server: usize) -> NuvioStream {
    let format = if url.contains(".m3u8") {
        "M3U8"
    } else if url.contains(".mp4") {
        "MP4"
    } else {
        "MKV"
    };
    let title = format!(
        "🎬 {media_line}\n💎 1080P | 🌍 Original-Audio\n🎞️ {format} | ⏱️ {DEFAULT_RUNTIME_MIN} min | 📌 Server {server}",
        media_line = media_line(name, year, media),
    );
    NuvioStream::new(url)
        .with_name("🟡 PlayIMDb | 1080p FHD | Original-Audio")
        .with_title(title)
        .with_quality("1080p")
        .with_kind("direct")
        .with_header("Origin", ORIGIN)
        .with_header("Referer", REFERER)
        .with_header("User-Agent", USER_AGENT)
}

/// `🎬 Name - S1E2 (2008)` for series, `🎬 Name - 2008` for movies —
/// the scraper's two label shapes (series pads the year, movies do
/// not).
fn media_line(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    match (media.season, media.episode, year) {
        (Some(season), Some(episode), Some(year)) => {
            format!("{name} - S{season}E{episode} ({year})")
        }
        (Some(season), Some(episode), None) => format!("{name} - S{season}E{episode}"),
        (_, _, Some(year)) => format!("{name} - {year}"),
        (_, _, None) => name.to_string(),
    }
}

/// The base display title for
/// `build_stream_results` —
/// upstream `name + (season ? ' S01E02' : ' (year)')`.
fn title_line(name: &str, year: Option<u16>, media: &MediaRef) -> String {
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
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;

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
                .find(|request| request_key(request) == key)
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(header, _)| header.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
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
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A provider over the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> PlayImdb {
        PlayImdb::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
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

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        assert_eq!(info.id, "playimdb");
        assert_eq!(info.label, "PlayIMDb");
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
            Some("https://playimdb.com/")
        );
        assert_eq!(info.priority, 0);
    }

    #[tokio::test]
    async fn resolves_movie_cards_with_hotlink_headers() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "status_code": 200,
            "data": { "stream_urls": [
                "https://scalableimpactgroup.site/hls/master.m3u8",
                "https://strategicgrowthpartners.site/file.mp4",
                "not-a-url",
            ]}
        });
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            payload.to_string(),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await
            .unwrap_or_else(|e| panic!("the movie fixture must resolve: {e}"));

        // The non-http entry is dropped by build_stream_results.
        assert_eq!(streams.len(), 2);
        let first = &streams[0];
        assert_eq!(
            first.url.as_str(),
            "https://scalableimpactgroup.site/hls/master.m3u8"
        );
        assert_eq!(first.format, Format::Hls);
        assert_eq!(first.meta.resolution, Some(1080));
        assert_eq!(first.meta.source_id.as_deref(), Some("playimdb"));
        assert_eq!(first.meta.source_label.as_deref(), Some("PlayIMDb"));
        assert_eq!(first.ttl, TTL);
        // The hotlink headers the upstream proxy carried upstream.
        assert_eq!(
            first
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(REFERER)
        );
        assert_eq!(
            first.meta.request_headers.get("Origin").map(String::as_str),
            Some(ORIGIN)
        );
        assert!(
            first
                .meta
                .request_headers
                .get("User-Agent")
                .is_some_and(|ua| ua.contains("Chrome/124"))
        );
        // The label: base title — emoji card.
        let label = first.label.as_deref().unwrap_or_default();
        assert!(label.starts_with("Dune (2021) — 🎬 Dune - 2021"), "{label}");
        assert!(label.contains("M3U8 | ⏱️ 90 min | 📌 Server 1"), "{label}");
        let second = &streams[1];
        assert_eq!(second.format, Format::Mp4);
        assert!(
            second
                .label
                .as_deref()
                .is_some_and(|l| l.contains("Server 2"))
        );

        // The API saw the vidking-style hotlink headers.
        assert_eq!(
            fetcher
                .sent_header("streamdata.vaplayer.ru/api.php", "Referer")
                .as_deref(),
            Some(REFERER)
        );
        Ok(())
    }

    #[tokio::test]
    async fn series_appends_season_and_episode() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "status_code": 200,
            "data": { "stream_urls": ["https://a.example/e1.m3u8"] }
        });
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            payload.to_string(),
        ));
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

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.host_str() == Some("streamdata.vaplayer.ru"))
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(query.contains("tmdb=1396"), "{query}");
        assert!(query.contains("type=tv"), "{query}");
        assert!(query.contains("season=1"), "{query}");
        assert!(query.contains("episode=2"), "{query}");
        // The series label carries S1E2 in both title lines.
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(label.contains("S01E02"), "{label}");
        assert!(label.contains("S1E2 (2008)"), "{label}");
        Ok(())
    }

    #[tokio::test]
    async fn wrong_status_code_is_not_found() {
        let payload = serde_json::json!({ "status_code": 34, "status_message": "not found" });
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            payload.to_string(),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn empty_stream_list_is_not_found() {
        let payload = serde_json::json!({ "status_code": 200, "data": { "stream_urls": [] } });
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            payload.to_string(),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn api_miss_is_not_found() {
        // Nothing scripted → the fetcher answers NotFound for the API.
        let fetcher = Arc::new(MockFetcher::new());
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn bad_json_is_not_found() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            "<html>not json</html>",
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn language_flags_come_from_the_stream_text() {
        // The country codes ride through build_stream_results: the
        // source defaults [multi, en] plus flags found in the stream
        // text. The card title mentions no other language, so the
        // defaults hold.
        let payload = serde_json::json!({
            "status_code": 200,
            "data": { "stream_urls": ["https://a.example/english.movie.mkv"] }
        });
        let fetcher = Arc::new(MockFetcher::new().serve(
            "streamdata.vaplayer.ru/api.php",
            200,
            payload.to_string(),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(438_631)))
            .await
            .unwrap_or_else(|e| panic!("the fixture must resolve: {e}"));
        assert_eq!(
            streams[0].meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
    }

    #[test]
    fn stream_construction_is_silent_on_url_shape() {
        // The helper functions must not panic on odd inputs (the
        // scraper emits entries like "not-a-url" verbatim).
        let media = MediaRef::series(MediaId::Tmdb(1), 2, 3);
        assert_eq!(media_line("A", Some(1999), &media), "A - S2E3 (1999)");
        assert_eq!(media_line("A", None, &media), "A - S2E3");
        let movie = MediaRef::movie(MediaId::Tmdb(1));
        assert_eq!(media_line("A", Some(1999), &movie), "A - 1999");
        let card = card("weird://host", "A", None, &movie, 7);
        assert_eq!(card.url, "weird://host");
    }
    #[test]
    fn accepts_the_live_string_status_code() {
        for status in [serde_json::json!(200), serde_json::json!("200")] {
            let payload: ApiResponse = serde_json::from_value(serde_json::json!({"status_code":status,"data":{"stream_urls":["https://cdn.example/master.m3u8"]}})).unwrap_or_else(|e| panic!("API response: {e}"));
            assert_eq!(payload.stream_urls().map(<[String]>::len), Some(1));
        }
        let denied: ApiResponse = serde_json::from_value(serde_json::json!({"status_code":"403","data":{"stream_urls":["https://cdn.example/master.m3u8"]}})).unwrap_or_else(|e| panic!("API response: {e}"));
        assert!(denied.stream_urls().is_none());
    }
}
