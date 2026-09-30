//! `Raflix`: raflixx's embed API plus server-resolved `CinePro` and
//! `VidStorm` stages.
//!
//! Ports `src/source/Raflix.js` (raflixx.vercel.app — movies/TV/anime).
//! Raflix is a SPA whose APIs answer embed URLs, and the JS resolved
//! three independent stages, results concatenated in stage order:
//!
//! 1. **raflixx media sources** — `GET /api/media/sources?type=
//!    {movie|tv}&tmdbId={id}[&season=&episode=]` →
//!    `{ok, sources: [{label, url}]}`. Upstream shipped these embed
//!    URLs as resolver-level cards; this port resolves each embed
//!    through the [`ExtractorRegistry`] inline (the `vidking`/
//!    `cinewave` precedent) and tags the results with the
//!    `[Raflix {label}]` card metadata.
//! 2. **`CinePro`** (the "Anicine Embed" worker): `GET
//!    api.anicine-embed.workers.dev/v1/token` → `{token}`, then
//!    `/v1/movies/{tmdb}` / `/v1/tv/{tmdb}/seasons/{s}/episodes/{e}`
//!    with the bearer token → `{sources: [{url}]}`. Each source URL is
//!    the worker's own `/v1/proxy?data=<percent-encoded JSON>` wrapper;
//!    decoding `data` yields the real upstream m3u8 plus the exact
//!    `Referer`/`User-Agent` it requires (a plain-looking HLS URL
//!    ships as-is). One retry with a fresh token on failure; a
//!    DNS/network-dead answer memoizes the worker for 10 minutes
//!    (upstream `_cineproDeadUntil` — an upstream-liveness mark, not a
//!    result cache).
//! 3. **`VidStorm`** (vidstorm.ru): `GET /api/movie/{tmdb}` /
//!    `/api/tv/{tmdb}/{s}/{e}` → `{server: {url: <AES-256-GCM token>,
//!    type, language}}`. Tokens decrypt through
//!    [`vidstorm_decrypt`]; each candidate is playlist-validated (200
//!    + `#EXTM3U`, max `RESOLUTION=` height clamped to 360..2160) so
//!      dead servers drop honestly. Upstream routed these through its
//!      `/proxy` with `origin=`/`referer=`; the port attaches the same
//!      `Origin`/`Referer: vidstorm.ru` headers to the direct stream.
//!
//! Cuts for the library port:
//!
//! - The anime path (`/api/anime/sources` sub/dub sweep) is unreachable
//!   without the TMDB genre probe that gated it — the probe is cut
//!   (the `TmdbClient` precedent, `imdbplay`/`itachi`), so anime
//!   references ride the media path like any other series.
//! - The `raflixnuvio`/`raflixvidstorm` pseudo source-ids existed only
//!   so the server's `NuvioExtractor` claimed those cards for
//!   `/proxy` routing; with no server, every stage carries plain
//!   `raflix` provenance and its own request headers.
//! - The JS ran the stages concurrently (`Promise.allSettled`, Task
//!   71); the port runs them sequentially in stage order — the
//!   engine's fan-out is the concurrency domain (`cinewave`
//!   precedent). The `gotJson` h2-GOAWAY retry is the net layer's.
//! - `meta.title` becomes [`Stream::label`]; `serverName`/
//!   `isMultiAudio` have no `StreamMeta` fields (the label carries the
//!   server name).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

use crate::nuvio::vidstorm::vidstorm_decrypt;

/// The provider id, upstream `this.id`.
const ID: &str = "raflix";
/// The display label, upstream `this.label`.
const LABEL: &str = "Raflix";
/// The SPA origin, upstream `BASE_URL`/`this.baseUrl`.
const BASE_URL: &str = "https://raflixx.vercel.app";
/// The dead `CinePro` worker API.
const CINEPRO_WORKER: &str = "https://api.anicine-embed.workers.dev";
/// The `VidStorm` API root.
const VIDSTORM_API: &str = "https://vidstorm.ru/api";
/// The Origin VidStorm-gated CDNs require.
const VIDSTORM_ORIGIN: &str = "https://vidstorm.ru";
/// Upstream `this.ttl`.
const TTL: Duration = Duration::from_mins(10);
/// One API call (upstream `gotJson` timeout / `cineproFetch`).
const API_TIMEOUT: Duration = Duration::from_secs(12);
/// One `VidStorm` playlist-validation probe.
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a network-dead `CinePro` worker is memoized.
const CINEPRO_DEAD_TTL: Duration = Duration::from_mins(10);
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// The `Raflix` provider.
pub struct Raflix {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The embed resolution chain for the raflixx media sources.
    extractors: Arc<ExtractorRegistry>,
    /// When a network-dead `CinePro` worker stops being memoized
    /// (upstream `_cineproDeadUntil`).
    cinepro_dead_until: Mutex<Option<Instant>>,
}

impl Raflix {
    /// A provider over the shared TMDB client and extractor registry.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, extractors: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![
                    CountryCode::Multi,
                    CountryCode::Hi,
                    CountryCode::En,
                    CountryCode::Ja,
                ],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            extractors,
            cinepro_dead_until: Mutex::new(None),
        }
    }

    /// Whether a network-dead `CinePro` worker is still memoized.
    fn cinepro_dead(&self) -> bool {
        self.cinepro_dead_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|until| Instant::now() < until)
    }

    /// Memoize the `CinePro` worker as network-dead.
    fn mark_cinepro_dead(&self) {
        *self
            .cinepro_dead_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Instant::now() + CINEPRO_DEAD_TTL);
    }

    /// Stage 1 — the raflixx media sources, each embed resolved
    /// through the registry — the port of `fetchMediaSources` plus the
    /// resolver-level handling upstream did afterwards.
    async fn media_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        title: &str,
    ) -> Vec<Stream> {
        let mut target = format!(
            "{BASE_URL}/api/media/sources?type={}&tmdbId={tmdb_id}",
            if media.season.is_some() {
                "tv"
            } else {
                "movie"
            },
        );
        if let (Some(season), Some(episode)) = (media.season, media.episode) {
            let _ = write!(target, "&season={season}&episode={episode}");
        }
        let Ok(url) = Url::parse(&target) else {
            return Vec::new();
        };
        let request = FetchRequest::get(url)
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_timeout(API_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(payload) = serde_json::from_str::<MediaSourcesResponse>(&response.body) else {
            return Vec::new();
        };
        if !payload.ok {
            return Vec::new();
        }

        let mut out = Vec::new();
        for source in payload.sources {
            let Some(raw) = source.url.as_deref() else {
                continue;
            };
            if !raw.starts_with("http") {
                continue;
            }
            let Ok(url) = Url::parse(raw) else {
                continue;
            };
            let embed_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: None,
                source_id: Some(ID),
                referer: None,
            };
            // A failing embed is dropped, like the upstream
            // per-extractor catch.
            if let Ok(streams) = self.extractors.extract(&embed_ctx, &url).await {
                for stream in streams {
                    out.push(tagged(stream, title, &source.label, "x264"));
                }
            }
        }
        out
    }

    /// Stage 2 — the `CinePro` worker chain, with the fresh-token retry
    /// and the network-dead memo — the port of `resolveCinePro`.
    async fn cinepro_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        title: &str,
    ) -> Vec<Stream> {
        if self.cinepro_dead() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for attempt in 0..2 {
            match self.cinepro_sources(ctx, media, tmdb_id).await {
                Ok(sources) => {
                    for (index, raw) in sources.iter().enumerate() {
                        if let Some(stream) = cinepro_stream(raw, index + 1, title) {
                            out.push(stream);
                        }
                    }
                    return out;
                }
                Err(error) => {
                    // Any failure retries once with a fresh token (the
                    // JS catch falls through to attempt 2 — the 401
                    // path and the plain fetch failures alike); the
                    // second failure memoizes the DNS-dead class for
                    // 10 minutes.
                    if attempt == 0 {
                        continue;
                    }
                    if error.is_network_dead() {
                        self.mark_cinepro_dead();
                    }
                    return out;
                }
            }
        }
        out
    }

    /// One `CinePro` token + sources round.
    async fn cinepro_sources(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
    ) -> Result<Vec<String>, CineproError> {
        let token_url =
            Url::parse(&format!("{CINEPRO_WORKER}/v1/token")).map_err(|_| CineproError::Retry)?;
        let request = FetchRequest::get(token_url)
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await?;
        if !response.is_success() {
            // A 401-class answer means the token flow failed — retry
            // with a forced refresh.
            return Err(CineproError::Retry);
        }
        let payload: TokenResponse = response.json().map_err(|_| CineproError::Retry)?;
        let Some(token) = payload.token.filter(|token| !token.is_empty()) else {
            return Err(CineproError::Retry);
        };

        let path = match (media.season, media.episode) {
            (Some(season), episode) => format!(
                "/v1/tv/{tmdb_id}/seasons/{season}/episodes/{}",
                episode.unwrap_or(1)
            ),
            _ => format!("/v1/movies/{tmdb_id}"),
        };
        let sources_url =
            Url::parse(&format!("{CINEPRO_WORKER}{path}")).map_err(|_| CineproError::Retry)?;
        let request = FetchRequest::get(sources_url)
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_header("Authorization", format!("Bearer {token}"))
            .with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await?;
        if response.status == 401 {
            return Err(CineproError::Retry);
        }
        if !response.is_success() {
            return Err(CineproError::Retry);
        }
        let payload: Value = response.json().map_err(|_| CineproError::Retry)?;
        let sources = payload
            .get("sources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(sources
            .iter()
            .filter_map(|source| source.get("url")?.as_str().map(str::to_string))
            .collect())
    }

    /// Stage 3 — the `VidStorm` decrypt + playlist validation — the port
    /// of `resolveVidStorm`.
    async fn vidstorm_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        title: &str,
    ) -> Vec<Stream> {
        let path = match (media.season, media.episode) {
            (Some(season), episode) => {
                format!("/tv/{tmdb_id}/{season}/{}", episode.unwrap_or(1))
            }
            _ => format!("/movie/{tmdb_id}"),
        };
        let Ok(url) = Url::parse(&format!("{VIDSTORM_API}{path}")) else {
            return Vec::new();
        };
        let request = FetchRequest::get(url)
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_timeout(API_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(servers) = serde_json::from_str::<Value>(&response.body) else {
            return Vec::new();
        };
        let Some(servers) = servers.as_object() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (name, server) in servers {
            let Some(entry) = server.as_object() else {
                continue;
            };
            let Some(token) = entry.get("url").and_then(Value::as_str) else {
                continue;
            };
            // mp4-class servers are observed dead upstream — hls only.
            let kind = entry
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !kind.to_ascii_lowercase().contains("hls") {
                continue;
            }
            let Some(real) = vidstorm_decrypt(token) else {
                continue;
            };
            if !real.starts_with("http") {
                continue;
            }
            // Playlist validation with the exact request the player
            // makes — dead/blocked servers drop honestly.
            let Some(height) = validate_playlist(ctx, &real).await else {
                continue;
            };
            let label = capitalize(name);
            let language = entry
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or("English")
                .to_string();
            let mut stream = tagged(
                Stream::new(
                    Url::parse(&real).unwrap_or_else(|e| panic!("validated VidStorm URL: {e}")),
                    Format::Hls,
                )
                .with_ttl(TTL),
                title,
                &format!("VidStorm {label}"),
                "h264",
            );
            // The /proxy's origin/referer pair rides request headers.
            stream.meta = stream
                .meta
                .with_header("Origin", VIDSTORM_ORIGIN)
                .with_header("Referer", format!("{VIDSTORM_ORIGIN}/"));
            stream.meta.audio = vec![language];
            stream.meta.resolution = Some(height);
            out.push(stream);
        }
        out
    }
}

#[async_trait]
impl Source for Raflix {
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

        // The three stages, in the upstream result order.
        let mut streams = self.media_streams(ctx, media, tmdb_id, &title).await;
        streams.extend(self.cinepro_streams(ctx, media, tmdb_id, &title).await);
        streams.extend(self.vidstorm_streams(ctx, media, tmdb_id, &title).await);

        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(streams)
    }
}

/// The raflixx media-sources response.
#[derive(Deserialize)]
struct MediaSourcesResponse {
    /// The API's ok marker.
    #[serde(default)]
    ok: bool,
    /// The embed sources.
    #[serde(default)]
    sources: Vec<MediaSource>,
}

/// One raflixx embed source.
#[derive(Deserialize)]
struct MediaSource {
    /// The display label.
    #[serde(default)]
    label: String,
    /// The embed URL.
    #[serde(default)]
    url: Option<String>,
}

/// The `CinePro` token response.
#[derive(Deserialize)]
struct TokenResponse {
    /// The bearer token.
    #[serde(default)]
    token: Option<String>,
}

/// Why one `CinePro` round failed.
#[derive(Debug)]
enum CineproError {
    /// A failure worth one fresh-token retry (the JS 401 path, plus
    /// the plain HTTP/JSON failures its loop retried once too).
    Retry,
    /// The DNS/network-dead class.
    NetworkDead,
}

impl CineproError {
    /// Whether this is the DNS/network-dead class that memoizes the
    /// worker (upstream: `fetch failed|ENOTFOUND|ECONNRESET|EAI_AGAIN
    /// |timeout|abort`).
    #[must_use]
    fn is_network_dead(&self) -> bool {
        matches!(self, Self::NetworkDead)
    }
}

impl From<FetchError> for CineproError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Transport { .. }
            | FetchError::Timeout { .. }
            | FetchError::TooManyTimeouts { .. } => Self::NetworkDead,
            _ => Self::Retry,
        }
    }
}

/// Build one `CinePro` stream from a worker source URL — decoding the
/// signed `/v1/proxy?data=` blob to the real upstream URL + headers
/// when possible (the port's decode of the worker's wrapper).
fn cinepro_stream(raw: &str, index: usize, title: &str) -> Option<Stream> {
    let url = Url::parse(raw).ok()?;
    let blob = url
        .query_pairs()
        .find(|(key, _)| key == "data")
        .and_then(|(_, value)| serde_json::from_str::<Value>(&value).ok());
    let (real, referer, user_agent) = if let Some(blob) = blob.as_ref().and_then(Value::as_object) {
        let real = blob.get("url")?.as_str()?;
        if !real.starts_with("http") {
            return None;
        }
        let headers = blob.get("headers").and_then(Value::as_object);
        (
            real.to_string(),
            headers
                .and_then(|headers| headers.get("Referer"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            headers
                .and_then(|headers| headers.get("User-Agent"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    } else {
        // Fallback: ship the worker's proxy URL as-is only if it looks
        // like HLS.
        let path = url.path().to_ascii_lowercase();
        if !(path.contains(".m3u8") || path.contains("/m3u8") || path.contains("/playlist")) {
            return None;
        }
        (raw.to_string(), String::new(), String::new())
    };

    let mut stream = Stream::new(Url::parse(&real).ok()?, Format::Hls).with_ttl(TTL);
    if !referer.is_empty() {
        stream.meta = stream.meta.with_header("Referer", referer);
    }
    if !user_agent.is_empty() {
        stream.meta = stream.meta.with_header("User-Agent", user_agent);
    }
    Some(tagged(stream, title, &format!("CinePro {index}"), "h264"))
}

/// Validate a `VidStorm` playlist from this egress — `Some(height)`
/// means a live `#EXTM3U` master (the max variant height clamped to
/// 360..2160), `None` drops the server.
async fn validate_playlist(ctx: &ResolveCtx<'_>, url: &str) -> Option<u16> {
    let url = Url::parse(url).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Origin", VIDSTORM_ORIGIN)
        .with_timeout(VALIDATION_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    if !response.body.contains("#EXTM3U") {
        return None;
    }
    let mut max_height = 0u16;
    for line in response.body.lines() {
        let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF") else {
            continue;
        };
        if let Some((_, height)) = parse_resolution(rest)
            && max_height < height
        {
            max_height = height;
        }
    }
    Some(max_height.clamp(360, 2160))
}

/// The `RESOLUTION=(\d+)x(\d+)` pair of a variant line.
fn parse_resolution(line: &str) -> Option<(u16, u16)> {
    let (width, height) = line.split_once("RESOLUTION=")?;
    let _ = width;
    let dimensions = height.split(['x', ' ', ',', ':']).collect::<Vec<_>>();
    let width: u16 = dimensions.first()?.parse().ok()?;
    let height: u16 = dimensions.get(1)?.parse().ok()?;
    Some((width, height))
}

/// Tag a resolved stream with the `[Raflix {server}]` card metadata —
/// the JS meta mapping (`title`, height 1080, `WebDL`, codec,
/// `English` audio).
fn tagged(mut stream: Stream, title: &str, server: &str, codec: &str) -> Stream {
    stream.label = Some(format!("{title} — [Raflix {server}]"));
    stream.ttl = TTL;
    stream.meta.languages = vec![CountryCode::Multi, CountryCode::En];
    stream.meta.audio = vec!["English".to_string()];
    stream.meta.quality = Some("WebDL".to_string());
    stream.meta.codec = Some(codec.to_string());
    stream.meta.resolution = stream.meta.resolution.or(Some(1080));
    stream.meta.source_id = Some(ID.to_string());
    stream.meta.source_label = Some(LABEL.to_string());
    stream
}

/// `name` → `Name`.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
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
    use vsources_core::error::{ExtractorError, FetchError};
    use vsources_core::traits::{Extractor, FetchRequest, FetchResponse, Fetcher, ResolveCtx};
    use vsources_core::types::{Format, MediaId};

    use super::*;

    // -- the scripted fetcher ------------------------------------------------

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

        /// How many requests hit `path`.
        fn hits(&self, path: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request.url.path() == path)
                .count()
        }
    }

    /// The lookup key of a URL: `path?query` when a query is present.
    fn key_of(url: &Url) -> String {
        match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = key_of(&request.url);
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
            // A scripted status of 0 answers a transport error — the
            // DNS-dead class.
            if status == 0 {
                return Err(FetchError::Transport {
                    url: request.url,
                    message: "scripted transport failure".to_string(),
                });
            }
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    // -- the stub extractor --------------------------------------------------

    /// An extractor that records its calls and answers one direct HLS
    /// stream for claimed hosts.
    struct StubExtractor {
        /// The extractor id.
        id: &'static str,
        /// The hosts this stub claims.
        hosts: &'static [&'static str],
        /// The URLs this stub saw.
        calls: Mutex<Vec<String>>,
    }

    impl StubExtractor {
        /// A stub claiming `hosts`.
        fn build(id: &'static str, hosts: &'static [&'static str]) -> Arc<Self> {
            Arc::new(Self {
                id,
                hosts,
                calls: Mutex::new(Vec::new()),
            })
        }

        /// The URLs this stub saw.
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Extractor for StubExtractor {
        fn id(&self) -> &str {
            self.id
        }

        fn label(&self) -> &str {
            self.id
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            self.hosts
                .iter()
                .any(|host| url.host_str().is_some_and(|h| h.ends_with(host)))
        }

        async fn extract(
            &self,
            _ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(url.to_string());
            Ok(vec![Stream::new(
                Url::parse("https://vidlink-cdn.example/hls/movie/master.m3u8")
                    .unwrap_or_else(|error| panic!("valid test URL: {error}")),
                Format::Hls,
            )])
        }
    }

    // -- fixtures ------------------------------------------------------------

    /// The `VidStorm` ground-truth token from the `vidstorm` helper port
    /// (decrypts with the site's derived key).
    /// The ground-truth token from `nuvio::vidstorm`'s test —
    /// decrypts to [`VIDSTORM_REAL`].
    const VIDSTORM_TOKEN: &str = "ABEiM0RVZneImaq7FDvPJlEEEmCLEe0DVeKWS9pzGmAf_JJjapyyo5CCaIb3Or4zdOAh-Kmud81fyw6r4HmicL1ewPjCMCoFLgT2GiNw9dobJRMhxo6OqPP47jopNKwbfQ";
    /// The URL the token decrypts to.
    const VIDSTORM_REAL: &str =
        "https://dreadnought.example.workers.dev/_v7/abc/master.m3u8?token=jwt";

    /// The fixture media (Dune, TMDB 693134).
    const TMDB_ID: u64 = 693_134;

    /// The provider over a registry of one stub extractor.
    fn provider(mock: &Arc<ScriptedFetcher>) -> Raflix {
        let stub: Arc<dyn Extractor> = StubExtractor::build("vidlink", &["vidlink.pro"]);
        Raflix::new(
            Arc::new(TmdbClient::new("test-key", mock.clone())),
            Arc::new(ExtractorRegistry::new(vec![stub])),
        )
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

    /// TMDB details for the fixture media.
    fn tmdb_mock() -> ScriptedFetcher {
        ScriptedFetcher::default().page(
            format!("/3/movie/{TMDB_ID}"),
            200,
            r#"{"title":"Dune: Part Two","release_date":"2024-02-27"}"#,
        )
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "raflix");
        assert_eq!(info.label, "Raflix");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![
                CountryCode::Multi,
                CountryCode::Hi,
                CountryCode::En,
                CountryCode::Ja
            ]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://raflixx.vercel.app/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_media_sources_through_the_registry() -> Result<(), SourceError> {
        let mock = Arc::new(tmdb_mock().page(
            "/api/media/sources",
            200,
            r#"{"ok":true,"sources":[
                {"id":"1","label":"VidLink","kind":"embed","url":"https://vidlink.pro/movie/693134"},
                {"id":"2","label":"Dead","kind":"embed","url":"/relative-not-http"}
            ]}"#,
        ));
        let stub = StubExtractor::build("vidlink", &["vidlink.pro"]);
        let provider = Raflix::new(
            Arc::new(TmdbClient::new("test-key", mock.clone())),
            Arc::new(ExtractorRegistry::new(vec![stub.clone()])),
        );
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // Only the http embed resolved; the relative URL was skipped.
        assert_eq!(streams.len(), 1);
        // The embed went through the extractor registry.
        assert_eq!(
            stub.calls(),
            vec!["https://vidlink.pro/movie/693134".to_string()]
        );
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://vidlink-cdn.example/hls/movie/master.m3u8"
        );
        assert_eq!(stream.format, Format::Hls);
        assert!(
            stream
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Dune: Part Two (2024) — [Raflix VidLink]"))
        );
        assert_eq!(
            stream.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(stream.meta.audio, vec!["English".to_string()]);
        assert_eq!(stream.meta.quality.as_deref(), Some("WebDL"));
        assert_eq!(stream.meta.codec.as_deref(), Some("x264"));
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.source_id.as_deref(), Some("raflix"));
        assert_eq!(stream.ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_and_validates_vidstorm_servers() -> Result<(), SourceError> {
        let mock = Arc::new(
            tmdb_mock()
                .page(
                    "/api/movie/693134",
                    200,
                    format!(r#"{{"lithium":{{"url":"{VIDSTORM_TOKEN}","type":"hls","language":"English"}},"helium":{{"url":"{VIDSTORM_TOKEN}","type":"hls","language":"Hindi"}},"carbon":{{"url":"{VIDSTORM_TOKEN}","type":"mp4"}}}}"#),
                )
                // The playlist validates for the decrypted URL.
                .page(
                    "/_v7/abc/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nvariant.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // Two hls servers survive (the mp4-class carbon is filtered);
        // the upstream loop emits one card per server — no URL dedupe.
        assert_eq!(streams.len(), 2);
        for stream in &streams {
            assert_eq!(stream.url.as_str(), VIDSTORM_REAL);
            assert_eq!(stream.format, Format::Hls);
            // The probed variant height from the playlist.
            assert_eq!(stream.meta.resolution, Some(1080));
            // The proxy's origin/referer pair rides request headers.
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Origin")
                    .map(String::as_str),
                Some("https://vidstorm.ru")
            );
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://vidstorm.ru/")
            );
            assert_eq!(stream.meta.codec.as_deref(), Some("h264"));
            assert_eq!(stream.meta.source_id.as_deref(), Some("raflix"));
        }
        // One card per language-carrying server.
        let helium = streams
            .iter()
            .find(|stream| {
                stream
                    .label
                    .as_deref()
                    .is_some_and(|label| label.contains("Helium"))
            })
            .unwrap_or_else(|| panic!("the helium card exists"));
        assert_eq!(helium.meta.audio, vec!["Hindi".to_string()]);
        let lithium = streams
            .iter()
            .find(|stream| {
                stream
                    .label
                    .as_deref()
                    .is_some_and(|label| label.contains("Lithium"))
            })
            .unwrap_or_else(|| panic!("the lithium card exists"));
        assert_eq!(lithium.meta.audio, vec!["English".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn drops_vidstorm_servers_whose_playlist_is_dead() -> Result<(), SourceError> {
        // The playlist 404s — the server drops honestly.
        let mock = Arc::new(
            tmdb_mock()
                .page(
                    "/api/movie/693134",
                    200,
                    format!(r#"{{"lithium":{{"url":"{VIDSTORM_TOKEN}","type":"hls","language":"English"}}}}"#),
                )
                .page("/_v7/abc/master.m3u8", 404, ""),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        Ok(())
    }

    #[tokio::test]
    async fn resolves_cinepro_sources_from_the_signed_blob() -> Result<(), SourceError> {
        // The worker's /v1/proxy?data= wrapper decodes to the real URL
        // plus its Referer/UA requirements.
        let blob = serde_json::json!({
            "url": "https://upstream.example.com/hls/master.m3u8",
            "headers": {"Referer": "https://ww2.yesmovies.ag/"}
        });
        let wrapped = format!(
            "https://api.anicine-embed.workers.dev/v1/proxy?data={}",
            urlencode(&blob.to_string())
        );
        let mock = Arc::new(
            tmdb_mock()
                .page("/v1/token", 200, r#"{"token":"tok-1"}"#)
                .page(
                    "/v1/movies/693134",
                    200,
                    format!(r#"{{"sources":[{{"url":"{wrapped}"}}]}}"#),
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://upstream.example.com/hls/master.m3u8"
        );
        assert!(
            stream
                .label
                .as_deref()
                .is_some_and(|label| label.contains("[Raflix CinePro 1]"))
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://ww2.yesmovies.ag/")
        );
        // The bearer token was sent.
        let sources_request = mock
            .requests()
            .into_iter()
            .find(|request| request.url.path() == "/v1/movies/693134")
            .unwrap_or_else(|| panic!("the CinePro sources endpoint was queried"));
        assert_eq!(
            sources_request
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_network_dead_cinepro_worker_is_memoized() -> Result<(), SourceError> {
        // Status 0 in the mock answers a transport error — the
        // DNS-dead class.
        let mock = Arc::new(tmdb_mock().page("/v1/token", 0, ""));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        // First resolve: the two in-round attempts hit the token
        // endpoint, then the worker is memoized dead.
        let first = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;
        assert!(matches!(first, Err(SourceError::NotFound)));
        let token_hits = mock.hits("/v1/token");
        assert_eq!(token_hits, 2, "one retry inside the round");

        // Second resolve: the memo skips the dead worker entirely.
        let second = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;
        assert!(matches!(second, Err(SourceError::NotFound)));
        assert_eq!(
            mock.hits("/v1/token"),
            token_hits,
            "the memo skipped the dead worker"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_resolve_answers_not_found() {
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
    fn capitalizes_server_names() {
        assert_eq!(capitalize("lithium"), "Lithium");
        assert_eq!(capitalize(""), "");
    }

    /// Percent-encode a `data=` blob like `encodeURIComponent`.
    fn urlencode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(char::from(byte));
                }
                _ => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
        out
    }
}
