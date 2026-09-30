//! `Netlio`: direct HLS from the Watchout2025 GitHub API.
//!
//! Ports `src/source/Netlio.js` (`netlio.vercel.app` — movies, TV,
//! anime, K-drama with direct HLS streams):
//!
//! - **Movies**: `…/hls/movie/{tmdbId}` answers the master playlist URL
//!   as plain text.
//! - **Series**: `…/hls/tv/{tmdbId}/S{season}.json` answers a
//!   `{"episode": url}` map. The API numbers episodes absolutely within
//!   a season file (S12 can start at 244), so the port re-implements
//!   the upstream mapping: keep the `http` values, sort by episode key,
//!   pick the Nth entry where N is the reference's episode number.
//! - `rpmhub.site` URLs are skipped — they need browser-side JS
//!   decryption (upstream comment).
//! - Every candidate is liveness-probed before shipping: a `Range` GET
//!   with the site `Referer`, dropping non-2xx answers and HTML gates —
//!   Netlio is a one-stream source and a born-dead card is worse than
//!   no card.
//!
//! All HLS URLs need `Referer: https://netlio.vercel.app/` to play; the
//! master playlist carries Hindi and English audio tracks (hence the
//! `multi/hi/en` languages).
//!
//! Cuts for the library port:
//!
//! - Upstream probed the CDN through the addon's own `/proxy` egress to
//!   choose between a proxy-wrapped and a direct stream; there is no
//!   server here, so the probe runs through the context fetcher and the
//!   stream ships direct with the site `Referer` (and the same probe
//!   headers the upstream attached: Chrome UA, `Range: bytes=0-1023`).
//! - `meta.title` has no `StreamMeta` field — the stream label carries
//!   the `${title} (Hindi + English)` form.
//! - Result caching is the parent's `CachedSource` domain.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

/// The GitHub raw API root.
const API_BASE: &str = "https://raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls";
/// The Referer every Netlio CDN playback needs.
const REFERER: &str = "https://netlio.vercel.app/";
/// The Chrome UA the upstream probe (and only the probe) sent.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The upstream probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
/// The upstream API timeout.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Upstream result lifetime: the source default 12h.
const TTL: Duration = Duration::from_hours(12);

/// The `Netlio` provider.
pub struct Netlio {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl Netlio {
    /// A provider over the shared TMDB client.
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "netlio".to_string(),
                label: "Netlio".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En],
                base_url: Some(
                    Url::parse("https://netlio.vercel.app")
                        .unwrap_or_else(|e| panic!("valid Netlio base URL: {e}")),
                ),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// The movie branch: the master URL as plain text.
    async fn movie_hls(&self, ctx: &ResolveCtx<'_>, tmdb_id: u64) -> Option<Url> {
        let url = Url::parse(&format!("{API_BASE}/movie/{tmdb_id}")).ok()?;
        let request = FetchRequest::get(url).with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let body = response.body.trim();
        // A raw "404" marker in the text is the API's miss answer.
        if body.is_empty() || body.contains("404") {
            return None;
        }
        let candidate = Url::parse(body).ok()?;
        if candidate.as_str().contains("rpmhub.site") {
            return None;
        }
        hls_alive(ctx, &candidate).await.then_some(candidate)
    }

    /// The series branch: the episode map, absolutely numbered.
    async fn episode_hls(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
    ) -> Option<Url> {
        let season = media.season.unwrap_or(1);
        let url = Url::parse(&format!("{API_BASE}/tv/{tmdb_id}/S{season}.json")).ok()?;
        let request = FetchRequest::get(url).with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        let map: serde_json::Value = serde_json::from_str(&response.body).ok()?;

        // Keep the http entries, sorted by episode key; non-numeric keys
        // (never observed) sort last.
        let mut episodes: Vec<(u64, String)> = map
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        value
                            .as_str()
                            .filter(|url| url.starts_with("http"))
                            .map(|url| (key.parse::<u64>().unwrap_or(u64::MAX), url.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        episodes.sort_by_key(|(episode, _)| *episode);

        // Stremio's per-season episode number maps to the Nth entry.
        let requested = u64::from(media.episode.unwrap_or(1));
        let index = usize::try_from(requested.checked_sub(1)?).ok()?;
        let (_, candidate) = episodes.get(index)?.clone();
        if candidate.contains("rpmhub.site") {
            return None;
        }
        let candidate = Url::parse(&candidate).ok()?;
        hls_alive(ctx, &candidate).await.then_some(candidate)
    }
}

#[async_trait]
impl Source for Netlio {
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

        let hls = if media.season.is_some() {
            self.episode_hls(ctx, media, tmdb_id).await
        } else {
            self.movie_hls(ctx, tmdb_id).await
        };
        let Some(url) = hls else {
            return Err(SourceError::NotFound);
        };

        let title = display_title(&name, year, media.season, media.episode);
        let mut stream = Stream::new(url, Format::Hls)
            .with_ttl(TTL)
            .with_referer(REFERER);
        stream.label = Some(format!("{title} (Hindi + English)"));
        stream.meta.languages = vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En];
        stream.meta.source_id = Some("netlio".to_string());
        stream.meta.source_label = Some("Netlio".to_string());
        Ok(vec![stream])
    }
}

/// The liveness probe: a `Range` GET with the site Referer — non-2xx
/// answers and HTML gates are dead (upstream `hlsAlive`).
async fn hls_alive(ctx: &ResolveCtx<'_>, url: &Url) -> bool {
    let request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Referer", REFERER)
        .with_header("Range", "bytes=0-1023")
        .with_timeout(PROBE_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return false;
    };
    if !response.is_success() {
        return false;
    }
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    !content_type.contains("text/html")
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
        /// A 200 text body with a content type.
        fn text(body: impl Into<String>, content_type: &str) -> Self {
            Self {
                status: 200,
                body: body.into(),
                headers: BTreeMap::from([("content-type".to_string(), content_type.to_string())]),
            }
        }

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

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
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
    fn provider(fetcher: &Arc<MockFetcher>) -> Netlio {
        Netlio::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
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
    async fn resolves_a_movie_with_liveness_probe() -> Result<(), SourceError> {
        let movie_key =
            "raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls/movie/438631";
        let cdn_key = "cdn.netlio.example/master.m3u8";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    movie_key,
                    Scripted::text("https://cdn.netlio.example/master.m3u8\n", "text/plain"),
                )
                .serve(
                    cdn_key,
                    Scripted::text(
                        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1",
                        "application/vnd.apple.mpegurl",
                    ),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the movie fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://cdn.netlio.example/master.m3u8"
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(
            stream.label.as_deref(),
            Some("Dune (2021) (Hindi + English)")
        );
        assert_eq!(
            stream.meta.languages,
            vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En]
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://netlio.vercel.app/")
        );
        // The probe sent the upstream header set.
        assert_eq!(
            fetcher.sent_header(cdn_key, "Referer").as_deref(),
            Some("https://netlio.vercel.app/")
        );
        assert_eq!(
            fetcher.sent_header(cdn_key, "Range").as_deref(),
            Some("bytes=0-1023")
        );
        assert_eq!(
            fetcher.sent_header(cdn_key, "User-Agent").as_deref(),
            Some(UA)
        );
        Ok(())
    }

    #[tokio::test]
    async fn maps_absolute_episode_numbers() -> Result<(), SourceError> {
        let season_key =
            "raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls/tv/1396/S12.json";
        let map = serde_json::json!({
            "5": "https://cdn.netlio.example/ep5.m3u8",
            "6": "https://cdn.netlio.example/ep6.m3u8",
            "7": "https://cdn.netlio.example/ep7.m3u8"
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(season_key, Scripted::json(&map))
                .serve(
                    "cdn.netlio.example/ep6.m3u8",
                    Scripted::text("#EXTM3U", "application/vnd.apple.mpegurl"),
                ),
        );
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(vsources_core::traits::ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(12),
                episode: Some(2),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 12, 2);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the episode fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        // Episode 2 maps to the second entry of the sorted map.
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.netlio.example/ep6.m3u8"
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S12E02 (Hindi + English)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn skips_rpmhub_urls() {
        let movie_key =
            "raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls/movie/438631";
        let fetcher = Arc::new(MockFetcher::new().serve(
            movie_key,
            Scripted::text("https://multimovies.rpmhub.site/file/x", "text/plain"),
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("an rpmhub URL must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn html_gate_drops_the_stream() {
        let movie_key =
            "raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls/movie/438631";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    movie_key,
                    Scripted::text("https://cdn.netlio.example/master.m3u8", "text/plain"),
                )
                .serve(
                    "cdn.netlio.example/master.m3u8",
                    Scripted::text("<html>gate</html>", "text/html"),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("an HTML gate must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_movie_404_marker_is_a_miss() {
        let movie_key =
            "raw.githubusercontent.com/Watchout2025/api/refs/heads/main/hls/movie/438631";
        let fetcher = Arc::new(
            MockFetcher::new().serve(movie_key, Scripted::text("404: Not Found", "text/plain")),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        match provider(&fetcher).resolve(&ctx, &dune_movie()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a 404 marker must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_season_file_is_a_miss() {
        let fetcher = Arc::new(MockFetcher::new());
        let ctx = ResolveCtx {
            fetcher: &*fetcher,
            media: Some(vsources_core::traits::ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(1),
                episode: Some(1),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 1, 1);

        match provider(&fetcher).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing season file must be a NotFound, got {other:?}"),
        }
    }
}
