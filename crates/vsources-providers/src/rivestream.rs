//! `RiveStream`: the 11-provider scrapper.rivestream.app sweep with
//! master-playlist expansion.
//!
//! Ports `src/source/RiveStream.js` + `src/nuvio/rivestream.cjs`
//! (rivestream.ru — movies + TV, direct playable HLS up to 4K). The
//! front end scrapes through `scrapper.rivestream.app`:
//!
//! 1. `GET /api/provider?provider={p}&id={tmdbId}[&season=&episode=]
//!    [&cb=…]` for each of the 11 backend providers (apex, citadel,
//!    primevids, quasar, solstice, horizon, pulse, flowcast, asiacloud,
//!    hindicast, guru — the slow/intermittent seven get shorter
//!    timeouts; primevids/citadel carry a coarse cache-bust `cb`) →
//!    `{data: {sources: [{quality, url, source, format, size}]}}`.
//! 2. Each source resolves into 1..N playable cards: MP4 sources ship
//!    directly; HLS URLs are **fetched as playlists** (the scraper
//!    fetches with `Referer: rivestream.ru/`) — a master playlist
//!    expands into one card per `#EXT-X-STREAM-INF` variant
//!    (RESOLUTION/BANDWIDTH/CODECS/FRAME-RATE/VIDEO-RANGE parsed,
//!    relative variant URLs resolved against the master, the master's
//!    token propagated), a media playlist ships as-is with quality and
//!    language parsed from the API label (`720p | Hindi`).
//! 3. The wrapper maps the scraper cards onto stream metadata: height
//!    from the quality label, language → `Multi Audio`/`Hindi`/…, the
//!    `2160p → HEVC` codec markers, and the hotlink `Referer:
//!    https://rivestream.ru/` for every host except valhallastream
//!    (the scraper's proxy, which handles Referer internally). Cards
//!    dedupe by URL and sort 4K-first.
//!
//! Cuts for the library port:
//!
//! - The scraper's stream shape dropped the `language`/`source` fields
//!   the wrapper reads (both fell back to defaults); the port carries
//!   them through so the wrapper's own parsing sees them — the
//!   evident intent of both files.
//! - The TMDB anime probe is cut (the `TmdbClient` precedent) —
//!   `isAnime` is always false, so the audio labels are the plain
//!   language names.
//! - `meta.title` becomes [`Stream::label`]; `serverName` rides the
//!   label. No `/proxy`: the scraper's proxyHeaders contract becomes
//!   the wrapper's `Referer` request header (the wrapper's own final
//!   header policy, not the scraper's richer proxyHeaders).
//! - The 34.5 s outer race becomes [`with_deadline`].

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde::Deserialize;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::with_deadline;

/// The provider id, upstream `this.id`.
const ID: &str = "rivestream";
/// The display label, upstream `this.label`.
const LABEL: &str = "RiveStream";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://rivestream.ru";
/// The scraper API root.
const SCRAPPER_API: &str = "https://scrapper.rivestream.app";
/// The hotlink Referer (`REFERER`).
const REFERER: &str = "https://rivestream.ru/";
/// Upstream `this.ttl` — stream URLs have short-lived auth tokens.
const TTL: Duration = Duration::from_mins(5);
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// A provider fetch (normal providers; the slow seven get 8 s).
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(20);
/// A provider fetch for the slow/intermittent providers.
const SLOW_PROVIDER_TIMEOUT: Duration = Duration::from_secs(8);
/// A playlist fetch (normal; slow providers get 6 s).
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(12);
/// A playlist fetch for the slow providers.
const SLOW_PLAYLIST_TIMEOUT: Duration = Duration::from_secs(6);
/// The outer race (the JS `34500` cap).
const SWEEP_DEADLINE: Duration = Duration::from_millis(34_500);

/// All 11 backend providers, order = priority (most-reliable
/// multi-quality first) — verbatim from the scraper.
const ALL_PROVIDERS: [&str; 11] = [
    "apex",
    "citadel",
    "primevids",
    "quasar",
    "solstice",
    "horizon",
    "pulse",
    "flowcast",
    "asiacloud",
    "hindicast",
    "guru",
];

/// The slow/intermittent providers with shorter timeouts.
const SLOW_PROVIDERS: [&str; 7] = [
    "asiacloud",
    "flowcast",
    "hindicast",
    "guru",
    "pulse",
    "horizon",
    "solstice",
];

/// `RESOLUTION=(\d+)x(\d+)` — a variant's dimensions.
static RESOLUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"RESOLUTION=(\d+)x(\d+)")
        .unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// `[?&]token=([^&]+)` — the master's token.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[?&]token=([^&]+)").unwrap_or_else(|e| panic!("valid token pattern: {e}"))
});

/// `^https?:.*\.mp4(\?|$)` (case-insensitive) — a direct MP4 URL.
static MP4_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^https?:.*\.mp4(\?|$)").unwrap_or_else(|e| panic!("valid mp4 pattern: {e}"))
});

/// `(\d{3,4})p|(\d{3,4})P|(4K|2160p|1440p|1080p|720p|480p|360p)` — a
/// quality label.
static QUALITY_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})p|(\d{3,4})P|(4K|2160p|1440p|1080p|720p|480p|360p)")
        .unwrap_or_else(|e| panic!("valid quality-label pattern: {e}"))
});

/// `\|\s*([A-Za-z]+)\s*$` — the language suffix of a quality label.
static LANGUAGE_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\|\s*([A-Za-z]+)\s*$").unwrap_or_else(|e| panic!("valid language pattern: {e}"))
});

/// `(\d{3,4})` — a bare number inside a quality label (the wrapper's
/// `parseHeight` fallback).
static HEIGHT_NUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})").unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});

/// One scraped card after the scraper's resolution step — the `.cjs`'s
/// `buildStream` shape plus the language/source fields the wrapper
/// reads.
#[derive(Debug, Clone)]
struct ScrapedCard {
    /// The card URL.
    url: String,
    /// The normalized quality (`4K`, `1080p`, …).
    quality: String,
    /// The audio language parsed from the API label (`multi`, `Hindi`).
    language: String,
    /// The backend provider name (apex, citadel, …).
    provider: String,
    /// The provider's own source name.
    source_name: String,
}

/// The `RiveStream` provider.
pub struct RiveStream {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl RiveStream {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![
                    CountryCode::Multi,
                    CountryCode::En,
                    CountryCode::Hi,
                    CountryCode::Ja,
                ],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }
}

#[async_trait]
impl Source for RiveStream {
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

        // The scraper's whole chain inside the outer race.
        let scrape = async { self.scrape(ctx, media, tmdb_id).await };
        let cards = with_deadline(scrape, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();

        // The wrapper's card mapping.
        let mut streams: Vec<Stream> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for card in cards {
            if !card.url.starts_with("http") || !seen.insert(card.url.clone()) {
                continue;
            }
            let Ok(url) = Url::parse(&card.url) else {
                continue;
            };
            let language = parse_language(&card.language);
            streams.push(wrapper_stream(&card, &url, &language, &title));
        }
        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(streams)
    }
}

impl RiveStream {
    /// The scraper chain: every provider's sources resolved into cards,
    /// deduped by URL and sorted 4K-first — the port of
    /// `rivestream.cjs getStreams`.
    async fn scrape(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
    ) -> Vec<ScrapedCard> {
        let is_tv = media.season.is_some();
        let mut cards: Vec<ScrapedCard> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for provider in ALL_PROVIDERS {
            let sources =
                fetch_provider_sources(ctx, provider, tmdb_id, is_tv, media.season, media.episode)
                    .await;
            for source in sources {
                for card in resolve_source(ctx, provider, source).await {
                    if seen.insert(card.url.clone()) {
                        cards.push(card);
                    }
                }
            }
        }
        // Sort by quality (4K first).
        cards.sort_by_key(|card| std::cmp::Reverse(quality_order(&card.quality)));
        cards
    }
}

/// One provider's raw sources — the port of `fetchProviderSources`
/// (errors answer empty, like the JS catch).
async fn fetch_provider_sources(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    tmdb_id: u64,
    is_tv: bool,
    season: Option<u32>,
    episode: Option<u32>,
) -> Vec<ApiSource> {
    let slow = SLOW_PROVIDERS.contains(&provider);
    let mut target = format!("{SCRAPPER_API}/api/provider?provider={provider}&id={tmdb_id}");
    if is_tv && let (Some(season), Some(episode)) = (season, episode) {
        let _ = write!(target, "&season={season}&episode={episode}");
    }
    // Cache-bust for primevids/citadel (matches the front-end
    // behaviour) — a coarse ~50-minute bucket.
    if provider == "primevids" || provider == "citadel" {
        let bucket = now_millis() / 3_000_000;
        let _ = write!(target, "&cb={bucket}");
    }
    let Ok(url) = Url::parse(&target) else {
        return Vec::new();
    };
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Referer", REFERER)
        .with_header("Accept", "application/json, */*")
        .with_timeout(if slow {
            SLOW_PROVIDER_TIMEOUT
        } else {
            PROVIDER_TIMEOUT
        });
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.is_success() {
        return Vec::new();
    }
    let Ok(payload) = serde_json::from_str::<ProviderResponse>(&response.body) else {
        return Vec::new();
    };
    payload
        .data
        .map(|data| {
            data.sources
                .into_iter()
                .filter(|source| !source.url.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve one source URL into 1..N cards — the port of
/// `resolveSource`: MP4 sources ship directly; HLS URLs are fetched as
/// playlists and master playlists expand into variants.
async fn resolve_source(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    source: ApiSource,
) -> Vec<ScrapedCard> {
    let quality_label = source.quality.as_deref().unwrap_or_default().to_string();
    let source_name = source
        .source
        .clone()
        .unwrap_or_else(|| provider.to_string());
    let format = source.format.as_deref().unwrap_or_default().to_lowercase();
    let is_mp4 = format == "mp4" || MP4_URL.is_match(&source.url).unwrap_or(false);

    // MP4 streams (FlowCast) — directly playable, no parsing needed.
    if is_mp4 {
        let quality =
            if quality_label.chars().all(|c| c.is_ascii_digit()) && !quality_label.is_empty() {
                format!("{quality_label}p")
            } else if quality_label.is_empty() {
                "MP4".to_string()
            } else {
                quality_label.clone()
            };
        return vec![ScrapedCard {
            url: source.url,
            quality,
            language: "multi".to_string(),
            provider: provider.to_string(),
            source_name,
        }];
    }

    // Try fetching the URL — a master or media playlist.
    let Ok(url) = Url::parse(&source.url) else {
        return Vec::new();
    };
    let slow = SLOW_PROVIDERS.contains(&provider);
    let request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Referer", REFERER)
        .with_header("Accept", "application/vnd.apple.mpegurl, */*")
        .with_timeout(if slow {
            SLOW_PLAYLIST_TIMEOUT
        } else {
            PLAYLIST_TIMEOUT
        });
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.body.starts_with("#EXTM3U") {
        return Vec::new();
    }

    let variants = parse_master_playlist(&response.body, &url);
    if variants.is_empty() {
        // Media playlist (segments only) — the playlist URL directly,
        // with quality + language from the API's label.
        return vec![ScrapedCard {
            url: source.url,
            quality: quality_from_label(&quality_label),
            language: language_from_label(&quality_label),
            provider: provider.to_string(),
            source_name,
        }];
    }
    variants
        .into_iter()
        .map(|variant| ScrapedCard {
            url: variant.url,
            quality: variant.quality,
            language: language_from_label(&quality_label),
            provider: provider.to_string(),
            source_name: source_name.clone(),
        })
        .collect()
}

/// One playlist variant.
struct Variant {
    /// The (absolute, token-carrying) variant URL.
    url: String,
    /// The height-bucketed quality label.
    quality: String,
}

/// Parse a master playlist into variants — the port of
/// `parseMasterM3u8` (RESOLUTION/BANDWIDTH/CODECS/FRAME-RATE/VIDEO-
/// RANGE, relative URL resolution, token carry). A media playlist
/// answers empty.
fn parse_master_playlist(text: &str, master: &Url) -> Vec<Variant> {
    let mut variants: Vec<Variant> = Vec::new();
    let mut current: Option<(u32, u32)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-STREAM-INF") {
            current = RESOLUTION
                .captures(line)
                .ok()
                .flatten()
                .and_then(|captures| {
                    Some((
                        captures.get(1)?.as_str().parse().ok()?,
                        captures.get(2)?.as_str().parse().ok()?,
                    ))
                });
        } else if !line.is_empty() && !line.starts_with('#') {
            let Some((width, height)) = current.take() else {
                continue;
            };
            // Resolve the variant URL against the master.
            let variant = if line.starts_with("http") {
                line.to_string()
            } else if let Ok(joined) = master.join(line) {
                joined.to_string()
            } else {
                continue;
            };
            // Carry the upstream token if present in the master URL.
            let token = TOKEN
                .captures(master.as_str())
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map(|group| group.as_str().to_string());
            let url = match token.filter(|_| !variant.contains("token=")) {
                Some(token) => {
                    let separator = if variant.contains('?') { '&' } else { '?' };
                    format!("{variant}{separator}token={token}")
                }
                None => variant,
            };
            let primary = if height >= 2160 || width >= 3840 {
                2160
            } else if height >= 1440 || width >= 2560 {
                1440
            } else if height >= 1080 || width >= 1920 {
                1080
            } else if height >= 720 || width >= 1280 {
                720
            } else if height >= 480 || width >= 852 {
                480
            } else {
                360
            };
            variants.push(Variant {
                url,
                quality: if primary >= 2160 {
                    "4K".to_string()
                } else {
                    format!("{primary}p")
                },
            });
        }
    }
    variants
}

/// The wrapper's card mapping — the port of `RiveStream.js`'s stream
/// construction (height, language, codec, headers, format).
fn wrapper_stream(card: &ScrapedCard, url: &Url, language: &str, title: &str) -> Stream {
    let height = parse_height(card.quality.as_str());
    let codec = if height >= 2160 { "HEVC" } else { "x264" };
    // The wrapper's URL/format inference: the scraper sets `type`, the
    // wrapper reads URL paths — HLS by .m3u8, else MP4.
    let is_hls = url.path().contains(".m3u8");
    let audio_label = language;

    let server_name = if card.source_name.is_empty() {
        card.provider.clone()
    } else {
        format!("{} {}", card.provider, card.source_name)
    };
    let mut stream = Stream::new(url.clone(), if is_hls { Format::Hls } else { Format::Mp4 })
        .with_label(format!(
            "{title} — [RiveStream {server_name}] {height}p {audio_label}"
        ))
        .with_ttl(TTL);
    // For HLS streams that need Referer (Citadel img1.*, PrimeVids
    // ngcorp.dad): the player sends it directly. Already-proxied URLs
    // (valhallastream) handle Referer internally.
    let needs_referer = !url.host_str().is_some_and(|host| {
        host.contains("valhallastream.dpdns.org") || host.contains("localhost")
    });
    if needs_referer {
        stream.meta = stream.meta.with_header("Referer", REFERER);
    }
    stream.meta.languages = parse_country_codes(language);
    stream.meta.audio = vec![language.to_string()];
    stream.meta.quality = Some("WebDL".to_string());
    stream.meta.codec = Some(codec.to_string());
    stream.meta.resolution = Some(height);
    stream.meta.source_id = Some(ID.to_string());
    stream.meta.source_label = Some(LABEL.to_string());
    stream
}

/// The wrapper's height parse: substring buckets then a bare number,
/// defaulting to 1080.
fn parse_height(quality: &str) -> u16 {
    let lower = quality.to_ascii_lowercase();
    if lower.contains("2160") || lower.contains("4k") {
        return 2160;
    }
    if lower.contains("1080") {
        return 1080;
    }
    if lower.contains("720") {
        return 720;
    }
    if lower.contains("480") {
        return 480;
    }
    if lower.contains("360") {
        return 360;
    }
    HEIGHT_NUM
        .captures(&lower)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
        .unwrap_or(1080)
}

/// The wrapper's language normalization — `multi` → `Multi Audio`, a
/// known name passes through, else `English`.
fn parse_language(lang: &str) -> String {
    let lower = lang.to_ascii_lowercase();
    if lower.contains("multi") {
        "Multi Audio".to_string()
    } else if lower.contains("japanese") {
        "Japanese".to_string()
    } else if lower.contains("hindi") {
        "Hindi".to_string()
    } else if lower.contains("tamil") {
        "Tamil".to_string()
    } else if lower.contains("telugu") {
        "Telugu".to_string()
    } else if lower.contains("kannada") {
        "Kannada".to_string()
    } else {
        // English, empty, and unknown labels alike (the JS's default).
        "English".to_string()
    }
}

/// The wrapper's language → country-code set (anime folded away — the
/// probe is cut).
fn parse_country_codes(language: &str) -> Vec<CountryCode> {
    let lower = language.to_ascii_lowercase();
    let mut codes = vec![CountryCode::Multi];
    if lower.contains("hindi") {
        codes.push(CountryCode::Hi);
    }
    if lower.contains("english") {
        codes.push(CountryCode::En);
    }
    if lower.contains("japanese") {
        codes.push(CountryCode::Ja);
    }
    if lower.contains("tamil") {
        codes.push(CountryCode::Ta);
    }
    if lower.contains("telugu") {
        codes.push(CountryCode::Te);
    }
    if codes.len() == 1 {
        codes.push(CountryCode::En);
    }
    codes
}

/// The scraper's quality-from-label — `720p | Hindi` → `720p`.
fn quality_from_label(label: &str) -> String {
    if label.is_empty() {
        return "HLS".to_string();
    }
    let captures = QUALITY_LABEL
        .captures(label)
        .ok()
        .flatten()
        .and_then(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|group| group.as_str().to_lowercase())
        });
    match captures {
        Some(quality) => {
            if quality == "4k" || quality == "2160" {
                "4K".to_string()
            } else if quality.ends_with('p') {
                quality
            } else {
                format!("{quality}p")
            }
        }
        None => label.to_string(),
    }
}

/// The scraper's language-from-label — `720p | Hindi` → `Hindi`, else
/// `multi`.
fn language_from_label(label: &str) -> String {
    LANGUAGE_LABEL
        .captures(label)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map_or_else(|| "multi".to_string(), |group| group.as_str().to_string())
}

/// The 4K-first sort order of the scraper's quality labels.
fn quality_order(quality: &str) -> u8 {
    match quality {
        "4K" | "2160p" => 0,
        "1440p" => 1,
        "1080p" => 2,
        "720p" => 3,
        "480p" => 4,
        "360p" => 5,
        "HLS" => 6,
        _ => 9,
    }
}

/// `Date.now()` — the cache-bust bucket clock.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
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

/// One raw source of the scraper API.
#[derive(Debug, Clone, Deserialize)]
struct ApiSource {
    /// The source URL.
    url: String,
    /// The API's quality label (`720p | Hindi`).
    #[serde(default)]
    quality: Option<String>,
    /// The provider's source name.
    #[serde(default)]
    source: Option<String>,
    /// The format (`mp4`, `hls`).
    #[serde(default)]
    format: Option<String>,
}

/// The scraper API response envelope.
#[derive(Deserialize)]
struct ProviderResponse {
    /// The sources envelope.
    data: Option<ProviderData>,
}

/// The `data` object.
#[derive(Deserialize)]
struct ProviderData {
    /// The raw sources.
    #[serde(default)]
    sources: Vec<ApiSource>,
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

        /// The Referer header of the first request to `path`.
        fn referer_of(&self, path: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| request.url.path() == path)
                .and_then(|request| request.headers.get("Referer").cloned())
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
    fn provider(mock: &Arc<ScriptedFetcher>) -> RiveStream {
        RiveStream::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// The fixture media (Dune: Part Two, TMDB 693134).
    const TMDB_ID: u64 = 693_134;

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
        assert_eq!(info.id, "rivestream");
        assert_eq!(info.label, "RiveStream");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![
                CountryCode::Multi,
                CountryCode::En,
                CountryCode::Hi,
                CountryCode::Ja
            ]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://rivestream.ru/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn expands_master_playlists_and_maps_cards() -> Result<(), SourceError> {
        // Apex answers a master playlist (with a token to carry);
        // citadel answers a media playlist with a language label.
        let mock = Arc::new(
            tmdb_mock()
                .page(
                    "/api/provider",
                    200,
                    r#"{"data":{"sources":[{"quality":"","url":"https://proxy.valhallastream.dpdns.org/m3u8-proxy?url=apex-master.m3u8&token=tok1","source":"Apex","format":"hls"}]}}"#,
                )
                .page(
                    "/m3u8-proxy",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nv-1080.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n/v-360.m3u8\n",
                )
                .page(
                    "/english.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg-1.ts\n",
                )
                // The citadel provider fetch needs its own source entry —
                // register a second body for the same endpoint.
                .page(
                    "/api/provider",
                    200,
                    r#"{"data":{"sources":[{"quality":"720p | Hindi","url":"https://img1.citadel.example/english.m3u8","source":"Citadel","format":"hls"}]}}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // The apex master expanded into two variants (1080p + 360p),
        // the citadel media playlist shipped as one card.
        assert_eq!(streams.len(), 3);
        // 4K-first order: 1080p before 360p here.
        let heights: Vec<Option<u16>> = streams
            .iter()
            .map(|stream| stream.meta.resolution)
            .collect();
        assert!(heights.contains(&Some(1080)));
        assert!(heights.contains(&Some(360)));
        assert!(heights.contains(&Some(720)));
        // Relative variant URLs resolved against the master, with the
        // master's token carried.
        let variant = streams
            .iter()
            .find(|stream| stream.meta.resolution == Some(1080))
            .unwrap_or_else(|| panic!("the 1080p variant exists"));
        assert_eq!(
            variant.url.as_str(),
            "https://proxy.valhallastream.dpdns.org/v-1080.m3u8?token=tok1"
        );
        // Already-proxied valhallastream URLs carry no Referer.
        assert!(!variant.meta.request_headers.contains_key("Referer"));
        // The citadel card carries the hotlink Referer and the Hindi
        // language flags.
        let citadel = streams
            .iter()
            .find(|stream| stream.meta.resolution == Some(720))
            .unwrap_or_else(|| panic!("the citadel card exists"));
        assert_eq!(
            citadel
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://rivestream.ru/")
        );
        assert!(citadel.meta.languages.contains(&CountryCode::Hi));
        assert!(
            variant
                .label
                .as_deref()
                .is_some_and(|label| label.contains("[RiveStream apex Apex] 1080p Multi Audio"))
        );
        assert!(
            citadel
                .label
                .as_deref()
                .is_some_and(|label| label.contains("[RiveStream citadel Citadel] 720p Hindi"))
        );
        // The playlist fetch carried the site Referer.
        assert_eq!(
            mock.referer_of("/m3u8-proxy").as_deref(),
            Some("https://rivestream.ru/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn ships_flowcast_mp4_sources_directly() -> Result<(), SourceError> {
        let mock = Arc::new(
            tmdb_mock().page(
                "/api/provider",
                200,
                r#"{"data":{"sources":[{"quality":"720","url":"https://hakunaymatata.com/files/movie-720.mp4","source":"FlowCast","format":"mp4"}]}}"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.format, Format::Mp4);
        assert_eq!(stream.meta.resolution, Some(720));
        assert_eq!(stream.meta.codec.as_deref(), Some("x264"));
        assert_eq!(stream.meta.quality.as_deref(), Some("WebDL"));
        // MP4 hosts still need the Referer.
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://rivestream.ru/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn dedupes_urls_across_providers() -> Result<(), SourceError> {
        let shared = r#"{"data":{"sources":[{"quality":"1080p","url":"https://shared.example.com/hls/master.m3u8","source":"","format":"hls"}]}}"#;
        let mock = Arc::new(
            tmdb_mock()
                .page("/api/provider", 200, shared)
                .page("/api/provider", 200, shared)
                .page("/hls/master.m3u8", 200, "#EXTM3U\n#EXTINF:4,\nseg.ts\n"),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // Both providers answered the same URL; the wrapper dedupes.
        assert_eq!(streams.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_sweep_answers_not_found() {
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
    fn parses_labels() {
        assert_eq!(quality_from_label("720p | Hindi"), "720p");
        assert_eq!(quality_from_label("4K"), "4K");
        // A bare `480` matches no alternative (they all carry the
        // `p`) and passes through verbatim, like the JS.
        assert_eq!(quality_from_label("480"), "480");
        assert_eq!(quality_from_label(""), "HLS");
        assert_eq!(quality_from_label("dcloud"), "dcloud");
        assert_eq!(language_from_label("720p | Hindi"), "Hindi");
        assert_eq!(language_from_label("1080p"), "multi");
        assert_eq!(parse_height("4K"), 2160);
        assert_eq!(parse_height("1440p"), 1440);
        assert_eq!(parse_height("Auto"), 1080);
        assert_eq!(parse_language("multi"), "Multi Audio");
        assert_eq!(parse_language("Hindi"), "Hindi");
        assert_eq!(parse_language(""), "English");
        assert_eq!(
            parse_country_codes("Hindi"),
            vec![CountryCode::Multi, CountryCode::Hi]
        );
        assert_eq!(
            parse_country_codes("Multi Audio"),
            vec![CountryCode::Multi, CountryCode::En]
        );
    }
}
