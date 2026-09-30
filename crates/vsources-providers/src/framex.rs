//! `FrameX`: the 20-provider api.framextv.tech sweep.
//!
//! Ports `src/source/FrameX.js` + `src/nuvio/framextv.cjs`
//! (framextv.tech — movies, TV, anime with direct HLS up to 4K). The
//! `FrameX` API answers one provider per request, so the scraper sweeps
//! **all 20 provider backends** (the `provider=<p>` param is where the
//! 4K sources live): batches of 5 with a 500 ms delay between batches
//! (the API throttles bursts), one retry per provider on transient
//! failures, and a 22 s internal deadline that returns partial
//! results rather than letting the source-level timeout discard
//! everything.
//!
//! Per source the API carries its own required headers (moon
//! .peakstorm.top wants `Referer: player.videasy.to/`, Vuflix wants
//! `ww2.yesmovies.ag/`) — they ride
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! through [`build_stream_results`]. Title-level subtitles (deduped by
//! language, first per language) attach to every stream; per-source
//! `audioTracks`/`hasMultipleAudio` flow verbatim into the language
//! flags and dual/multi-audio labels.
//!
//! The JS wrapper then enriches each stream with
//! `WEB-DL {HEVC HDR|x264} {audio}` title markers (FrameX.js) before
//! `buildStreamResults`; quality is normalized (`2160p`/`4K` → `4K`,
//! `dcloud`/`ipcloud` → `Auto`, …) and cards sort 4K-first.
//!
//! Cuts for the library port:
//!
//! - The upstream `framextv.cjs` **delegates to streamxtv.cjs's shared
//!   90 s-cached sweep** (both hit the same backend). The Rust port
//!   runs framextv.cjs's documented fallback — its own sweep with
//!   `FrameX` branding — since cross-provider result sharing is the
//!   parent `CachedSource`'s domain, and a streamxtv module may not
//!   exist in this crate.
//! - The TMDB anime/original-language probe that chose the injected
//!   audio track is cut (the `TmdbClient` precedent) — when the API
//!   omits `audioTracks`, the injected track is always `English`.
//! - `meta.title` becomes [`Stream::label`]; the scraper's
//!   `bingeGroup`/`notWebReady` behavior hints have no stream-level
//!   equivalent in the shared `build_stream_results` port. No
//!   `/proxy`: the Referer headers ride request headers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{
    BuildParams, NuvioStream, NuvioSubtitle, build_audio_label, build_stream_results,
    normalize_audio_tracks, parse_height,
};

/// The provider id, upstream `this.id`.
const ID: &str = "framextv";
/// The display label, upstream `this.label`.
const LABEL: &str = "FrameX";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://framextv.tech";
/// The stream API root.
const API_BASE: &str = "https://api.framextv.tech";
/// Upstream `this.ttl`.
const TTL: Duration = Duration::from_mins(10);
/// The API's same-site Referer (the header set that gets through its
/// per-IP throttle).
const API_REFERER: &str = "https://framextv.tech/";
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The whole-sweep deadline — must stay below the source-level race so
/// partial results survive (upstream `SWEEP_DEADLINE_MS`).
const SWEEP_DEADLINE: Duration = Duration::from_secs(22);
/// Providers per batch (the API throttles bursts).
const BATCH_SIZE: usize = 5;
/// Delay between batches (upstream `BATCH_DELAY_MS`).
const BATCH_DELAY: Duration = Duration::from_millis(500);
/// One provider request (upstream `REQUEST_TIMEOUT_MS`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Retries per provider (upstream `REQUEST_RETRIES`).
const REQUEST_RETRIES: u32 = 1;
/// Backoff before the retry (upstream `1500 * (attempt + 1)`).
const RETRY_BACKOFF: Duration = Duration::from_millis(1500);
/// Subtitles are deduped by language, capped (upstream
/// `MAX_SUBTITLES`).
const MAX_SUBTITLES: usize = 20;
/// Stop starting new batches once this much of the deadline remains
/// (the JS `SWEEP_DEADLINE_MS - 8000` gate).
const BATCH_STOP_MARGIN: Duration = Duration::from_secs(8);

/// All 20 provider backends, ordered by typical quality (4K-capable
/// first) — verbatim from the scraper.
const ALL_PROVIDERS: [&str; 20] = [
    "barbarian",
    "goblin",
    "super_barbarian",
    "electro_wizard",
    "lavahound",
    "headhunter",
    "pekka",
    "super_pekka",
    "valkyrie",
    "dragon",
    "witch",
    "giant",
    "golem",
    "super_dragon",
    "yeti",
    "wizard",
    "miner",
    "bowler",
    "ice_golem",
    "pekka_x",
];

/// The sort order of normalized qualities (4K first).
fn quality_sort_key(quality: &str) -> u8 {
    match quality {
        "4K" | "2160p" => 0,
        "1440p" => 1,
        "1080p" => 2,
        "720p" => 3,
        "480p" => 4,
        "360p" => 5,
        "Auto" | "HLS" => 6,
        _ => 9,
    }
}

/// The `FrameX` provider.
pub struct FrameX {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl FrameX {
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
}

#[async_trait]
impl Source for FrameX {
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

        let mut streams = self.sweep(ctx, media, tmdb_id).await;
        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        // The wrapper's enrichment: audio markers + WEB-DL/HEVC/HDR
        // tags on every title.
        enrich(&mut streams);
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

impl FrameX {
    /// The 20-provider batched sweep — the port of `ownSweep`.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
    ) -> Vec<NuvioStream> {
        let is_tv = media.season.is_some();
        let mut streams: Vec<NuvioStream> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut shared_subs: Option<Vec<NuvioSubtitle>> = None;
        let started = Instant::now();

        let mut batch_start = 0;
        while batch_start < ALL_PROVIDERS.len() {
            if batch_start > 0 {
                tokio::time::sleep(BATCH_DELAY).await;
            }
            let stop_after = SWEEP_DEADLINE
                .checked_sub(BATCH_STOP_MARGIN)
                .unwrap_or_default();
            if started.elapsed() > stop_after {
                // Deadline approaching — partial results are better
                // than a timeout discarding everything.
                break;
            }
            let batch =
                &ALL_PROVIDERS[batch_start..(batch_start + BATCH_SIZE).min(ALL_PROVIDERS.len())];
            let runs = futures::future::join_all(batch.iter().map(|provider| {
                fetch_provider(ctx, provider, is_tv, tmdb_id, media.season, media.episode)
            }))
            .await;
            for (provider, run) in batch.iter().zip(runs) {
                let Some(json) = run else {
                    continue;
                };
                let sources = json.sources.as_deref().unwrap_or_default();
                if json.success == Some(false) || sources.is_empty() {
                    continue;
                }
                // Title-level subtitles — captured once (identical
                // across providers).
                if shared_subs.is_none() {
                    shared_subs = Some(map_subtitles(json.subtitles.as_ref()));
                }
                for source in sources {
                    let Some(url) = source.url.as_deref() else {
                        continue;
                    };
                    if !url.starts_with("http") || seen.iter().any(|seen| seen == url) {
                        continue;
                    }
                    seen.push(url.to_string());

                    let quality = normalize_quality(source.quality.as_deref());
                    let server = source
                        .server
                        .clone()
                        .unwrap_or_else(|| provider.to_string());
                    // When the server name equals the provider name the
                    // title would duplicate it — display-only dedup.
                    let server_tag = if server == *provider {
                        String::new()
                    } else {
                        format!(" {server}")
                    };
                    // Per-source headers, normalized to the casing
                    // `build_stream_results` reads; harmless keys
                    // dropped.
                    let mut stream = NuvioStream::new(url)
                        .with_quality(quality.clone())
                        .with_name(format!("FrameX - {quality} {server} ({provider})"))
                        .with_title(format!("FrameX {provider} {quality}{server_tag}"));
                    if let Some(headers) = source.headers.as_ref() {
                        if let Some(referer) = headers.referer() {
                            stream = stream.with_header("Referer", referer);
                        }
                        if let Some(user_agent) = headers.user_agent() {
                            stream = stream.with_header("User-Agent", user_agent);
                        }
                    }
                    if let Some(subs) = shared_subs.as_ref().filter(|subs| !subs.is_empty()) {
                        for subtitle in subs {
                            stream = stream.with_subtitle(subtitle.clone());
                        }
                    }
                    // Per-source audio metadata, passed through
                    // verbatim whenever present.
                    if let Some(audio) = source.audio_tracks.as_ref() {
                        stream.audio_tracks = Some(audio.clone());
                    }
                    if let Some(multiple) = source.has_multiple_audio {
                        stream.has_multiple_audio = Some(multiple);
                    }
                    streams.push(stream);
                }
            }
            batch_start += BATCH_SIZE;
        }

        // Sort by quality (4K first), then by name for stable order.
        streams.sort_by(|a, b| {
            let key = quality_sort_key(a.quality.as_deref().unwrap_or(""))
                .cmp(&quality_sort_key(b.quality.as_deref().unwrap_or("")));
            key.then_with(|| a.name.cmp(&b.name))
        });
        streams
    }
}

/// One provider JSON fetch with the retry — the port of `fetchJson`.
async fn fetch_provider(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    is_tv: bool,
    tmdb_id: u64,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<ProviderResponse> {
    let mut url = Url::parse(&format!("{API_BASE}/api/stream"))
        .unwrap_or_else(|e| panic!("valid FrameX API URL: {e}"));
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("type", if is_tv { "tv" } else { "movie" });
        pairs.append_pair("id", &tmdb_id.to_string());
        if is_tv && let Some(season) = season {
            pairs.append_pair("season", &season.to_string());
            pairs.append_pair("episode", &episode.unwrap_or(1).to_string());
        }
        pairs.append_pair("provider", provider);
    }
    for attempt in 0..=REQUEST_RETRIES {
        let request = FetchRequest::get(url.clone())
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_header("Referer", API_REFERER)
            .with_timeout(REQUEST_TIMEOUT);
        if let Ok(response) = ctx.fetcher.request(request).await
            && response.is_success()
            && let Ok(payload) = serde_json::from_str::<ProviderResponse>(&response.body)
        {
            return Some(payload);
        }
        if attempt < REQUEST_RETRIES {
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
    }
    None
}

/// One provider's API response.
#[derive(Deserialize)]
struct ProviderResponse {
    /// `false` marks the API's failure answer.
    #[serde(default)]
    success: Option<bool>,
    /// The direct sources.
    #[serde(default)]
    sources: Option<Vec<ProviderSource>>,
    /// Title-level subtitles (same for all providers).
    #[serde(default)]
    subtitles: Option<Vec<ApiSubtitle>>,
}

/// One source of a provider response.
#[derive(Deserialize)]
struct ProviderSource {
    /// The stream URL.
    #[serde(default)]
    url: Option<String>,
    /// The API's quality label.
    #[serde(default)]
    quality: Option<String>,
    /// The backend server name, when it differs from the provider.
    #[serde(default)]
    server: Option<String>,
    /// Per-source hotlink headers.
    #[serde(default)]
    headers: Option<ApiHeaders>,
    /// Per-source audio languages.
    #[serde(rename = "audioTracks", default)]
    audio_tracks: Option<Value>,
    /// The multi-audio flag.
    #[serde(rename = "hasMultipleAudio", default)]
    has_multiple_audio: Option<bool>,
}

/// The headers the scraper reads (case-tolerant).
#[derive(Deserialize)]
struct ApiHeaders {
    /// The hotlink Referer.
    #[serde(rename = "Referer", default)]
    referer: Option<String>,
    /// The hotlink User-Agent.
    #[serde(rename = "referer", default)]
    referer_lower: Option<String>,
    /// The hotlink User-Agent (canonical casing).
    #[serde(rename = "User-Agent", default)]
    user_agent: Option<String>,
    /// The hotlink User-Agent (lowercase casing).
    #[serde(rename = "user-agent", default)]
    user_agent_lower: Option<String>,
}

impl ApiHeaders {
    /// The Referer, either casing.
    fn referer(&self) -> Option<&str> {
        self.referer.as_deref().or(self.referer_lower.as_deref())
    }

    /// The User-Agent, either casing.
    fn user_agent(&self) -> Option<&str> {
        self.user_agent
            .as_deref()
            .or(self.user_agent_lower.as_deref())
    }
}

/// One title-level subtitle of the API.
#[derive(Deserialize)]
struct ApiSubtitle {
    /// The subtitle URL.
    #[serde(default)]
    url: Option<String>,
    /// The language.
    #[serde(default)]
    language: Option<String>,
    /// The label fallback.
    #[serde(default)]
    label: Option<String>,
    /// The lang fallback.
    #[serde(default)]
    lang: Option<String>,
}

impl ApiSubtitle {
    /// The dedup key — `language || lang || label || "en"`, capped at
    /// 12 chars like the JS.
    fn language(&self) -> String {
        let language = self
            .language
            .as_deref()
            .or(self.lang.as_deref())
            .or(self.label.as_deref())
            .unwrap_or("en");
        language.chars().take(12).collect()
    }
}

/// Map the API's subtitles to stream tracks, deduped by language —
/// the port of `mapSubtitles`.
fn map_subtitles(raw: Option<&Vec<ApiSubtitle>>) -> Vec<NuvioSubtitle> {
    let Some(subtitles) = raw else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for subtitle in subtitles {
        let Some(url) = subtitle.url.as_deref() else {
            continue;
        };
        let language = subtitle.language();
        if seen.contains(&language) {
            continue;
        }
        seen.push(language.clone());
        out.push(NuvioSubtitle {
            id: Some(language.chars().take(8).collect()),
            url: Some(url.to_string()),
            lang: Some(language),
            ..NuvioSubtitle::default()
        });
        if out.len() >= MAX_SUBTITLES {
            break;
        }
    }
    out
}

/// Normalize an API quality label — the port of `normalizeQuality`
/// (`"2160p"`/`"4k"` → `4K`, bare `480` → `480p`, multi-language
/// labels like `"720p | English"` → the resolution, `PrimeVids`
/// placeholders → `Auto`).
fn normalize_quality(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "HLS".to_string();
    };
    let quality = raw.trim();
    let lower = quality.to_ascii_lowercase();
    match lower.as_str() {
        "2160p" | "4k" => return "4K".to_string(),
        "1440p" => return "1440p".to_string(),
        "1080p" => return "1080p".to_string(),
        "720p" => return "720p".to_string(),
        "480p" | "480" => return "480p".to_string(),
        "360p" | "360" => return "360p".to_string(),
        _ => {}
    }
    if let Some(height) = parse_height(Some(&lower)) {
        return match height {
            h if h >= 2160 => "4K",
            h if h >= 1080 => "1080p",
            h if h >= 720 => "720p",
            h if h >= 480 => "480p",
            h if h >= 360 => "360p",
            _ => "Auto",
        }
        .to_string();
    }
    "Auto".to_string()
}

/// The wrapper's title enrichment — FrameX.js's markers: `WEB-DL`,
/// `HEVC HDR`/`x264` by height, and the audio label (the API's own
/// `audioTracks` when present, else the injected `English` — the
/// anime/original-language probe is cut).
fn enrich(streams: &mut [NuvioStream]) {
    for stream in streams {
        let height = parse_height(stream.quality.as_deref());
        let mut tracks = normalize_audio_tracks(stream.audio_tracks.as_ref());
        if tracks.is_empty() {
            tracks = vec!["English".to_string()];
            stream.audio_tracks = Some(json!(tracks));
        }
        let mut markers = vec![
            "WEB-DL".to_string(),
            match height {
                Some(2160) => "HEVC HDR".to_string(),
                _ => "x264".to_string(),
            },
        ];
        let audio = build_audio_label(&tracks, stream.has_multiple_audio)
            .unwrap_or_else(|| "English".to_string());
        markers.push(audio);
        let title = stream.title.clone().unwrap_or_default();
        stream.title = Some(format!("{title} {}", markers.join(" ")));
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
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> FrameX {
        FrameX::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// The fixture media (Dune, TMDB 693134).
    const TMDB_ID: u64 = 693_134;

    /// TMDB details + every provider answering an empty success (only
    /// `provider` gets real sources).
    fn mock_with(provider: &str, provider_body: &str) -> ScriptedFetcher {
        let mut mock = ScriptedFetcher::default().page(
            format!("/3/movie/{TMDB_ID}"),
            200,
            r#"{"title":"Dune: Part Two","release_date":"2024-02-27"}"#,
        );
        for name in ALL_PROVIDERS {
            let body = if name == provider {
                provider_body.to_string()
            } else {
                r#"{"success":true,"sources":[]}"#.to_string()
            };
            mock = mock.page(
                format!("/api/stream?type=movie&id={TMDB_ID}&provider={name}"),
                200,
                body,
            );
        }
        mock
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "framextv");
        assert_eq!(info.label, "FrameX");
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
            Some("https://framextv.tech/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn sweeps_providers_and_enriches_streams() -> Result<(), SourceError> {
        let body = r#"{
            "success": true,
            "sources": [
                {"url":"https://moon.peakstorm.top/hls/movie/master.m3u8","quality":"1080p","server":"Videasy","headers":{"Referer":"https://player.videasy.to/"}},
                {"url":"https://cineplay.example/hls/movie/2160.m3u8","quality":"2160p","audioTracks":["Hindi","English"],"hasMultipleAudio":true}
            ],
            "subtitles": [
                {"url":"https://subs5.strem.io/en.vtt","language":"English"},
                {"url":"https://subs5.strem.io/en2.vtt","language":"English"},
                {"url":"https://subs5.strem.io/es.vtt","language":"Spanish"}
            ]
        }"#;
        let mock = Arc::new(mock_with("barbarian", body));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 2);
        // 4K first after the sort.
        assert_eq!(streams[0].meta.resolution, Some(2160));
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cineplay.example/hls/movie/2160.m3u8"
        );
        // The dual-audio label arrives via the enriched title markers
        // (the scraper's title embeds the `4K` quality, the wrapper
        // appends `WEB-DL HEVC HDR` for 2160p) and drives the language
        // flags.
        assert!(streams[0].label.as_deref().is_some_and(|label| {
            label.contains("FrameX barbarian 4K WEB-DL HEVC HDR Dual Audio (Hindi + English)")
        }));
        assert!(streams[0].meta.languages.contains(&CountryCode::Hi));
        assert!(streams[0].meta.languages.contains(&CountryCode::En));
        // Shared subtitles — deduped by language (2 English → 1).
        assert_eq!(streams[0].meta.subtitles.len(), 2);
        // The per-source Referer rides the request headers.
        assert_eq!(
            streams[1]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://player.videasy.to/")
        );
        // The injected English audio track when the API omits one.
        assert!(
            streams[1]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("WEB-DL x264 English"))
        );
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("framextv"))
        );
        assert_eq!(streams[0].ttl, TTL);
        // All 20 providers were swept.
        assert_eq!(mock.hits("/api/stream"), 20);
        Ok(())
    }

    #[tokio::test]
    async fn a_series_reference_carries_season_and_episode() -> Result<(), SourceError> {
        let body = r#"{"success":true,"sources":[{"url":"https://moon.peakstorm.top/hls/tv/master.m3u8","quality":"1080p"}]}"#;
        let mut mock = ScriptedFetcher::default().page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"Dark","first_air_date":"2017-12-01"}"#,
        );
        for name in ALL_PROVIDERS {
            let provider_body = if name == "goblin" {
                body.to_string()
            } else {
                r#"{"success":true,"sources":[]}"#.to_string()
            };
            mock = mock.page(
                format!("/api/stream?type=tv&id={TMDB_ID}&season=2&episode=5&provider={name}"),
                200,
                provider_body,
            );
        }
        let mock = Arc::new(mock);
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 2, 5))
            .await?;

        assert_eq!(streams.len(), 1);
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Dark S02E05"))
        );
        let goblin = mock
            .requests()
            .into_iter()
            .find(|request| {
                request
                    .url
                    .query()
                    .is_some_and(|query| query.contains("provider=goblin"))
            })
            .unwrap_or_else(|| panic!("the goblin provider was queried"));
        assert!(
            goblin
                .url
                .query()
                .is_some_and(|query| query.contains("season=2"))
        );
        assert!(
            goblin
                .url
                .query()
                .is_some_and(|query| query.contains("episode=5"))
        );
        assert!(
            goblin
                .url
                .query()
                .is_some_and(|query| query.contains("type=tv"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_all_empty_sweep_answers_not_found() {
        let mock = Arc::new(mock_with("barbarian", r#"{"success":true,"sources":[]}"#));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert_eq!(mock.hits("/api/stream"), 20);
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
    fn normalizes_quality_labels() {
        assert_eq!(normalize_quality(Some("2160p")), "4K");
        assert_eq!(normalize_quality(Some("4K")), "4K");
        assert_eq!(normalize_quality(Some("1080p")), "1080p");
        assert_eq!(normalize_quality(Some("480")), "480p");
        assert_eq!(normalize_quality(Some("720p | English")), "720p");
        assert_eq!(normalize_quality(Some("dcloud")), "Auto");
        assert_eq!(normalize_quality(Some("Auto HLS")), "Auto");
        assert_eq!(normalize_quality(None), "HLS");
    }
}
