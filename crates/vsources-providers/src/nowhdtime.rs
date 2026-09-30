//! `NowHDTime`: the nhdapi.com JSON API, one CORS-open HLS proxy URL.
//!
//! Ports `src/source/NowHDTime.js` (`nowhdtime.to` — movies, series,
//! anime, K-drama, all TMDB-id keyed). The nhdapi JSON API answers a
//! `playUrl` that proxies the upstream CDN — no extraction needed:
//!
//! 1. `GET https://nhdapi.com/api/movie/{tmdbId}` or
//!    `GET https://nhdapi.com/api/tv/{tmdbId}/{season}/{episode}` with
//!    the `X-API-Key` header →
//!    `{success: true, playUrl: "https://nhdapi.com/api/hls?t=…", kind: "hls"}`.
//! 2. The card is only shipped when the playlist is actually being
//!    served (upstream Task 59): the port probes the `playUrl` and
//!    keeps it when the body carries `#EXTM3U` (parsing the
//!    `RESOLUTION=WxH` height) or the content type is `video/*` /
//!    `audio/*` — the API also proxies progressive downloads as
//!    `kind: "mp4"`. A JSON error body means a born-dead token and
//!    drops the card.
//!
//! The API key ships as a constant (upstream keeps it in the
//! site-secrets registry, env-overridable there); [`NowHDTime::with_api_key`]
//! overrides it for embedders.
//!
//! Cuts for the library port:
//!
//! - `meta.title` has no `StreamMeta` field — the stream label carries
//!   the `${title} (HLS)` form.
//! - Upstream hardcoded `Format.hls` even for `kind: "mp4"` answers;
//!   this port maps `kind` to the format instead (the one deliberate
//!   divergence — the upstream's own probe comment documents the mp4
//!   class).
//! - Result caching is the parent's `CachedSource` domain; the stream
//!   TTL stays short because tokens are time-limited.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

/// The nhdapi API root.
const API_BASE: &str = "https://nhdapi.com/api";
/// The upstream API key (site-secrets registry upstream, constant here —
/// override with [`NowHDTime::with_api_key`]).
const DEFAULT_API_KEY: &str = "7d5239afc1d0a4fa374587d1d3feb1b0";
/// The upstream API timeout (the API can take ~0-9s).
const API_TIMEOUT: Duration = Duration::from_secs(25);
/// The upstream liveness-probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Stream tokens are time-limited — short TTL.
const TTL: Duration = Duration::from_mins(10);

/// The `NowHDTime` provider.
pub struct NowHDTime {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The nhdapi API key.
    api_key: String,
}

impl NowHDTime {
    /// A provider over the shared TMDB client with the upstream API key.
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self::with_api_key(tmdb, DEFAULT_API_KEY)
    }

    /// A provider with an explicit nhdapi API key (the upstream
    /// site-secrets override).
    #[must_use]
    pub fn with_api_key(tmdb: Arc<TmdbClient>, api_key: impl Into<String>) -> Self {
        Self {
            info: SourceInfo {
                id: "nowhdtime".to_string(),
                label: "NowHDTime".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Some(
                    Url::parse("https://www.nowhdtime.to")
                        .unwrap_or_else(|e| panic!("valid NowHDTime base URL: {e}")),
                ),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            api_key: api_key.into(),
        }
    }

    /// The API answer for a media reference.
    async fn api(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
    ) -> Option<ApiResponse> {
        let path = match (media.season, media.episode) {
            (Some(season), Some(episode)) => {
                format!("/tv/{tmdb_id}/{season}/{episode}")
            }
            _ => format!("/movie/{tmdb_id}"),
        };
        let url = Url::parse(&format!("{API_BASE}{path}")).ok()?;
        let request = FetchRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("X-API-Key", self.api_key.as_str())
            .with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let payload: ApiResponse = serde_json::from_str(&response.body).ok()?;
        // `success: false` (or a missing playUrl) is the API's miss answer.
        payload.success.then_some(payload)
    }
}

#[async_trait]
impl Source for NowHDTime {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(&self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;

        let Some(payload) = self.api(ctx, media, tmdb_id).await else {
            return Err(SourceError::NotFound);
        };
        let Some(play_url) = payload.play_url.as_deref().filter(|url| !url.is_empty()) else {
            return Err(SourceError::NotFound);
        };
        let url = Url::parse(play_url).map_err(|_| {
            SourceError::scrape("nowhdtime", "the API served an unparseable playUrl")
        })?;

        // Liveness + resolution probe: only ship when the playlist (or the
        // progressive file) is actually being served.
        let Some((playable, height)) = probe(ctx, &url).await else {
            return Err(SourceError::NotFound);
        };
        if !playable {
            return Err(SourceError::NotFound);
        }

        let title = display_title(&name, year, media.season, media.episode);
        let format = if payload.kind.as_deref() == Some("mp4") {
            Format::Mp4
        } else {
            Format::Hls
        };
        let mut stream = Stream::new(url, format).with_ttl(TTL);
        stream.label = Some(format!("{title} (HLS)"));
        stream.meta.resolution = height;
        stream.meta.languages = vec![CountryCode::Multi];
        stream.meta.source_id = Some("nowhdtime".to_string());
        stream.meta.source_label = Some("NowHDTime".to_string());
        Ok(vec![stream])
    }
}

/// Probe the play URL: `(playable, height)` when the request answered,
/// `None` on transport failure (treated as not playable upstream).
async fn probe(ctx: &ResolveCtx<'_>, url: &Url) -> Option<(bool, Option<u16>)> {
    let request = FetchRequest::get(url.clone()).with_timeout(PROBE_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return Some((false, None));
    }
    if response.body.contains("#EXTM3U") {
        return Some((true, resolution_hint(&response.body)));
    }
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let playable = content_type.starts_with("video/") || content_type.starts_with("audio/");
    Some((playable, None))
}

/// The `RESOLUTION=WxH` height in an HLS master body.
fn resolution_hint(body: &str) -> Option<u16> {
    for (offset, _) in body.char_indices() {
        let rest = &body[offset..];
        if rest.len() >= "resolution=".len()
            && rest[.."resolution=".len()].eq_ignore_ascii_case("resolution=")
        {
            let after = &rest["resolution=".len()..];
            let height_start = after.find('x')? + 1;
            let digits: String = after[height_start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(height) = digits.parse() {
                return Some(height);
            }
        }
    }
    None
}

/// The TMDB id for the reference (IMDb-keyed references resolve through
/// `/find`); TMDB miss pages map to [`SourceError::NotFound`] like the
/// upstream `NotFoundError`.
async fn tmdb_id(tmdb: &TmdbClient, media: &MediaRef) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
    }
}

/// The media name and year, preferring pre-resolved context metadata
/// (the upstream resolver resolved TMDB before calling sources) and
/// falling back to the shared client.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = &ctx.media
        && !resolved.name.is_empty()
    {
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

/// The upstream display title: name + `S01E02` for episodes, name +
/// ` (year)` for movies.
fn display_title(
    name: &str,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
) -> String {
    if season.is_some() {
        format!(
            "{name} S{:02}E{:02}",
            season.unwrap_or(1),
            episode.unwrap_or(1)
        )
    } else {
        let year = year.map(|y| y.to_string()).unwrap_or_default();
        format!("{name} ({year})")
    }
}

/// The nhdapi response envelope.
#[derive(Deserialize)]
struct ApiResponse {
    /// Whether the API resolved the title.
    #[serde(default)]
    success: bool,
    /// The CORS-open HLS proxy URL.
    #[serde(rename = "playUrl", default)]
    play_url: Option<String>,
    /// The proxied container (`hls` or `mp4`).
    #[serde(default)]
    kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;
    use vsources_core::traits::{FetchResponse, Fetcher};

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        status: u16,
        body: String,
        headers: BTreeMap<String, String>,
    }

    impl Scripted {
        /// A 200 JSON body.
        fn json(value: &serde_json::Value) -> Self {
            Self {
                status: 200,
                body: value.to_string(),
                headers: BTreeMap::from([(
                    "content-type".to_string(),
                    "application/json".to_string(),
                )]),
            }
        }

        /// A 200 body with an explicit content type.
        fn body(body: impl Into<String>, content_type: &str) -> Self {
            Self {
                status: 200,
                body: body.into(),
                headers: BTreeMap::from([("content-type".to_string(), content_type.to_string())]),
            }
        }
    }

    /// A fetcher serving scripted pages by host+path (in order, the last
    /// repeating) and recording every request it sees.
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

        /// Serve `key` (host + path) with `response`.
        fn serve(self, key: &str, response: Scripted) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.to_string())
                .or_default()
                .push_back(response);
            self
        }

        /// The value of a header sent to `key`.
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| {
                    let host = request.url.host_str().unwrap_or_default();
                    format!("{host}{}", request.url.path()) == key
                })
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
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let key = format!(
                "{}{}",
                request.url.host_str().unwrap_or_default(),
                request.url.path()
            );
            let mut pages = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let response = pages.get_mut(&key).map(|queue| {
                // The last scripted response repeats.
                let front = queue
                    .front()
                    .cloned()
                    .unwrap_or_else(|| panic!("a scripted page must exist for {key}"));
                if queue.len() > 1 {
                    queue.pop_front();
                }
                front
            });
            match response {
                Some(scripted) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: scripted.headers,
                    body: scripted.body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A resolved Dune movie.
    fn dune_media() -> vsources_core::traits::ResolvedMedia {
        vsources_core::traits::ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: None,
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        }
    }

    /// A Dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::tmdb(438_631, MediaType::Movie)
    }

    /// A provider over the shared mock's TMDB client.
    fn provider(fetcher: &Arc<MockFetcher>) -> NowHDTime {
        NowHDTime::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
    }

    /// A resolve context over the shared mock.
    fn ctx_for(
        fetcher: &MockFetcher,
        media: Option<vsources_core::traits::ResolvedMedia>,
    ) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    #[tokio::test]
    async fn resolves_a_playable_hls() -> Result<(), SourceError> {
        let api_key = "nhdapi.com/api/movie/438631";
        let hls_key = "nhdapi.com/api/hls";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    api_key,
                    Scripted::json(&serde_json::json!({
                        "success": true,
                        "playUrl": "https://nhdapi.com/api/hls?t=abc",
                        "kind": "hls"
                    })),
                )
                .serve(
                    hls_key,
                    Scripted::body(
                        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1920x1080\n720.m3u8",
                        "application/vnd.apple.mpegurl",
                    ),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the HLS fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.url.as_str(), "https://nhdapi.com/api/hls?t=abc");
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.label.as_deref(), Some("Dune (2021) (HLS)"));
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        assert_eq!(stream.meta.source_id.as_deref(), Some("nowhdtime"));
        assert_eq!(
            fetcher.sent_header(api_key, "X-API-Key").as_deref(),
            Some(DEFAULT_API_KEY)
        );
        Ok(())
    }

    #[tokio::test]
    async fn mp4_kind_maps_to_mp4_format() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "nhdapi.com/api/movie/438631",
                    Scripted::json(&serde_json::json!({
                        "success": true,
                        "playUrl": "https://nhdapi.com/api/hls?t=def",
                        "kind": "mp4"
                    })),
                )
                .serve("nhdapi.com/api/hls", Scripted::body("binary", "video/mp4")),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the mp4 fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, Format::Mp4);
        Ok(())
    }

    #[tokio::test]
    async fn a_json_error_body_drops_the_card() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "nhdapi.com/api/movie/438631",
                    Scripted::json(&serde_json::json!({
                        "success": true,
                        "playUrl": "https://nhdapi.com/api/hls?t=expired",
                        "kind": "hls"
                    })),
                )
                .serve(
                    "nhdapi.com/api/hls",
                    Scripted::json(&serde_json::json!({"error": "token expired"})),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a JSON error body must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unsuccessful_api_answer_is_a_miss() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "nhdapi.com/api/movie/438631",
            Scripted::json(&serde_json::json!({"success": false})),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("an unsuccessful answer must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dead_token_is_a_miss() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "nhdapi.com/api/movie/438631",
                    Scripted::json(&serde_json::json!({
                        "success": true,
                        "playUrl": "https://nhdapi.com/api/hls?t=dead",
                        "kind": "hls"
                    })),
                )
                .serve(
                    "nhdapi.com/api/hls",
                    Scripted {
                        status: 403,
                        body: "forbidden".to_string(),
                        headers: BTreeMap::new(),
                    },
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a dead token must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn series_uses_the_season_episode_path() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "nhdapi.com/api/tv/1396/2/3",
                    Scripted::json(&serde_json::json!({
                        "success": true,
                        "playUrl": "https://nhdapi.com/api/hls?t=tv",
                        "kind": "hls"
                    })),
                )
                .serve(
                    "nhdapi.com/api/hls",
                    Scripted::body("#EXTM3U", "application/vnd.apple.mpegurl"),
                ),
        );
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(vsources_core::traits::ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(2),
                episode: Some(3),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 2, 3);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S02E03 (HLS)")
        );
        Ok(())
    }
}
