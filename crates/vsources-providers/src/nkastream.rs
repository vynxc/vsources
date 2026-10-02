//! `NikaStream`: nikastream.blog — sub + dub anime through the
//! Anivexa Cloudflare Worker aggregator.
//!
//! Ports `src/source/NikaStream.js` + `src/nuvio/nikastream.cjs`
//! (movies — anime films — and series):
//!
//! 1. TMDB → title → the `AniList` GraphQL search (`format: TV`/`MOVIE`
//!    then unfiltered, original then macron-stripped title, a 3-attempt
//!    ladder with the 429/2 s retry) → the `AniList` id; Jikan and Kitsu
//!    fallbacks return MAL/title matches only — Anivexa needs the
//!    `AniList` id, so those answer an honest zero.
//! 2. `GET /episodes/{provider}/{anilistId}` for all 11 aggregator
//!    providers in parallel (the Anivexa `Referer`/`Origin`), keeping
//!    the ones with a sub or dub episode list.
//! 3. `GET /watch/{provider}/{anilistId}/{audio}/{provider}-{ep}` for
//!    every provider × audio that lists the episode (and, when
//!    neither lists it, both anyway — some providers serve unlisted
//!    episodes).
//! 4. Each raw stream converts to a card: encrypted-CDN (flixcloud.cc)
//!    and embed-only streams become iframes — **the wrapper drops
//!    iframes** (a player cannot run the page's WASM/JS), so only
//!    direct `hls`/`mp4` URLs survive. Shared engine probes validate them
//!    without downloading complete files in this provider.
//! 5. The wrapper filters playable streams, dedupes by URL (some
//!    providers answer the same URL for sub and dub), and builds one
//!    card each: the
//!    `{title} [NikaStream {provider} {SUB|DUB} {server}] {height}p
//!    WEB-DL {codec} {English|Japanese}` label, the `[multi, ja]` /
//!    `[multi, en]` language flags by audio, and the scraper's
//!    subtitle tracks.
//!
//! Cuts for the library port:
//!
//! - The scraper's own TMDB fetch (title/year) — the shared
//!   [`TmdbClient`] serves it (`cineby`/`framex` precedent).
//! - `behaviorHints.bingeGroup` has no stream-level equivalent (the
//!   `framex` cut); `notWebVideo` marked exactly the iframe streams
//!   the wrapper dropped anyway. The scraper's `mapPool(8)` bounded
//!   concurrency is subsumed by the fetcher layer's per-host queue.
//! - `meta.nuvioProvider`/`nuvioReferer`/`nuvioForceHls` — the
//!   server-proxy flags map onto
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!   (the `User-Agent`/`Referer`/`Origin` triple the proxy sent) and
//!   the `nuvioForceHls` behavior hint, the shared Nuvio policy.
//! - `meta.sourceType` (`WebDL`) → `meta.quality`; `meta.serverName`
//!   and the title's provider/audio/server markers ride
//!   [`Stream::label`].
//! - The wrapper's 70 s `Promise.race` cap rides
//!   `with_deadline`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta,
    SubtitleTrack,
};

use crate::nuvio::with_deadline;

/// The provider id, upstream `this.id`.
const ID: &str = "nikastream";
/// The display label, upstream `this.label`.
const LABEL: &str = "NikaStream";
/// The site origin, upstream `this.baseUrl` / the scraper's
/// `NIKASTREAM_ORIGIN`.
const BASE_URL: &str = "https://nikastream.blog";
/// The Anivexa aggregator API.
const ANIVEXA_API: &str = "https://anivexa-api.sudeepdon119.workers.dev";
/// The `AniList` GraphQL endpoint.
const ANILIST_GRAPHQL: &str = "https://graphql.anilist.co";
/// The upstream browser UA (the scraper's `UA`).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Upstream `this.ttl` — 10 min.
const TTL: Duration = Duration::from_mins(10);
/// The wrapper's `Promise.race` cap (70 s).
const SWEEP_DEADLINE: Duration = Duration::from_secs(70);
/// One Anivexa episodes call (upstream 12 s).
const EPISODES_TIMEOUT: Duration = Duration::from_secs(12);
/// One Anivexa watch call (upstream 15 s).
const WATCH_TIMEOUT: Duration = Duration::from_secs(15);
/// One `AniList` GraphQL call (upstream 20 s).
const ANILIST_TIMEOUT: Duration = Duration::from_secs(20);
/// One Jikan/Kitsu call (upstream 10 s).
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// The `AniList` 429 retry delay.
const RATE_LIMIT_DELAY: Duration = Duration::from_secs(2);
/// The `AniList` fetch-error retry delay.
const RETRY_DELAY: Duration = Duration::from_millis(1500);

/// All 11 Anivexa providers, priority order (most reliable first) —
/// the scraper's `ALL_PROVIDERS`, queried in parallel with failures
/// silently skipped.
const ALL_PROVIDERS: [&str; 11] = [
    "reanime",
    "anikoto",
    "anibd",
    "anineko",
    "animegg",
    "kaa",
    "mkissa",
    "2dhive",
    "senshi",
    "animedunya",
    "anizone",
];

/// The encrypted-CDN hosts whose m3u8 bodies only the embed page's JS
/// can decrypt (the scraper's `ENCRYPTED_CDN_HOSTS`).
const ENCRYPTED_CDN_HOSTS: [&str; 3] =
    ["flixcloud.cc", "fetch8.flixcloud.cc", "fetch7.flixcloud.cc"];

/// The audio variant of a watch query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audio {
    /// Japanese audio with subs.
    Sub,
    /// English audio.
    Dub,
}

impl Audio {
    /// The URL segment.
    fn key(self) -> &'static str {
        match self {
            Self::Sub => "sub",
            Self::Dub => "dub",
        }
    }

    /// The display label.
    fn label(self) -> &'static str {
        match self {
            Self::Sub => "SUB",
            Self::Dub => "DUB",
        }
    }

    /// The sort order (SUB first, then DUB).
    fn order(self) -> u8 {
        match self {
            Self::Sub => 0,
            Self::Dub => 1,
        }
    }
}

/// One raw Anivexa watch stream.
#[derive(Debug, Deserialize)]
struct RawStream {
    /// The direct stream URL.
    #[serde(default)]
    url: Option<String>,
    /// The kind (`hls`, `mp4`, `embed`).
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// The server display name.
    #[serde(default)]
    server: Option<String>,
    /// The embed page URL.
    #[serde(default)]
    embed: Option<String>,
    /// The hotlink Referer the CDN requires.
    #[serde(default)]
    referer: Option<String>,
    /// Subtitle tracks.
    #[serde(default)]
    subtitles: Option<Vec<RawSubtitle>>,
}

/// One raw Anivexa subtitle.
#[derive(Debug, Deserialize)]
struct RawSubtitle {
    /// The ISO code.
    #[serde(default)]
    srclang: Option<String>,
    /// The language name.
    #[serde(default)]
    lang: Option<String>,
    /// The long-form language.
    #[serde(default)]
    language: Option<String>,
    /// The display label.
    #[serde(default)]
    label: Option<String>,
    /// The subtitle file URL.
    #[serde(default)]
    url: Option<String>,
}

/// One scraper-shaped subtitle — `{id, url, lang}`.
#[derive(Debug, Clone)]
struct ScrapeSubtitle {
    /// The language-code id.
    id: String,
    /// The subtitle file URL.
    url: String,
    /// The language name.
    lang: String,
}

/// One converted stream — the scraper's card shape before the wrapper
/// builds the final result.
#[derive(Debug, Clone)]
struct NikaCard {
    /// The stream URL.
    url: String,
    /// The scraper's MIME marker (`application/vnd.apple.mpegurl` or
    /// `video/mp4`).
    kind: String,
    /// `NikaStream - {provider} {SUB|DUB} {server}`.
    name: String,
    /// The quality label.
    quality: String,
    /// The aggregator provider.
    provider: String,
    /// The audio variant.
    audio: Audio,
    /// The hotlink Referer, when the CDN requires one.
    referer: Option<String>,
    /// Subtitle tracks.
    subtitles: Vec<ScrapeSubtitle>,
}

/// The `NikaStream` provider.
pub struct NikaStream {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl NikaStream {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// A provider over the shared TMDB client. The aggregator answers
    /// direct URLs (embeds are dropped by the wrapper), so no
    /// extractor registry is needed.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            mappings: None,
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }
}

#[async_trait]
impl Source for NikaStream {
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

        // NikaStream is anime-only; a series reference without an
        // episode answers the honest zero (the scraper's guard).
        if media.season.is_some() && media.episode.is_none() {
            return Err(SourceError::NotFound);
        }

        let sweep = async { self.sweep(ctx, &name, media).await };
        let cards = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if cards.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(build_streams(&cards, &title))
    }
}

impl NikaStream {
    /// The scraper's whole sweep: `AniList` → provider episodes → watch
    /// queries → conversion + sort — the port of
    /// `nikastream.cjs` `getStreams` (the wrapper's playable filter
    /// and URL dedupe fold into the conversion).
    async fn sweep(&self, ctx: &ResolveCtx<'_>, name: &str, media: &MediaRef) -> Vec<NikaCard> {
        if let Some(ids) = crate::anime_mapping::ids(self.mappings.as_ref(), ctx, media).await {
            let streams = self.sweep_inner(ctx, name, media, Some(ids)).await;
            if !streams.is_empty() {
                return streams;
            }
        }
        self.sweep_inner(ctx, name, media, None).await
    }

    async fn sweep_inner(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        media: &MediaRef,
        mapped: Option<vsources_core::mappings::SeasonIds>,
    ) -> Vec<NikaCard> {
        let is_movie = media.season.is_none();
        let media_type = if is_movie { "movie" } else { "tv" };
        // Anivexa requires the AniList id — a Jikan/Kitsu-only match
        // answers the honest zero.
        let anilist_id = match mapped {
            Some(ids) => Some(ids.anilist_id),
            None => find_anilist_id(ctx, name, media_type).await,
        };
        let Some(anilist_id) = anilist_id else {
            return Vec::new();
        };
        let ep_num = if is_movie {
            1
        } else {
            media.episode.unwrap_or(1)
        };

        // Per-provider episode lists, in parallel; failures skipped.
        let episode_lists = futures::future::join_all(ALL_PROVIDERS.iter().map(|provider| async {
            let episodes = fetch_provider_episodes(ctx, provider, anilist_id).await;
            (*provider, episodes)
        }))
        .await;
        let active: Vec<(&str, EpisodeList)> = episode_lists
            .into_iter()
            .filter_map(|(provider, episodes)| episodes.map(|episodes| (provider, episodes)))
            .filter(|(_, episodes)| episodes.has_any())
            .collect();
        if active.is_empty() {
            return Vec::new();
        }

        // The watch queries: both audios when the provider lists the
        // episode; both anyway when it lists neither (some providers
        // serve unlisted episodes).
        let mut queries: Vec<(&str, Audio)> = Vec::new();
        for (provider, episodes) in &active {
            let mut listed = 0;
            for audio in [Audio::Sub, Audio::Dub] {
                if episodes.has_episode(audio, ep_num) {
                    queries.push((provider, audio));
                    listed += 1;
                }
            }
            if listed == 0 {
                queries.push((provider, Audio::Sub));
                if episodes.dub.is_some() {
                    queries.push((provider, Audio::Dub));
                }
            }
        }

        // Watch in parallel, then convert every stream.
        let watch_results = futures::future::join_all(
            queries
                .iter()
                .map(|(provider, audio)| fetch_watch(ctx, provider, anilist_id, *audio, ep_num)),
        )
        .await;
        let mut tasks: Vec<(RawStream, &str, Audio)> = Vec::new();
        for ((provider, audio), result) in queries.iter().zip(watch_results) {
            for raw in result {
                tasks.push((raw, provider, *audio));
            }
        }
        let converted = tasks
            .iter()
            .map(|(raw, provider, audio)| convert_stream(raw, provider, *audio));

        // Dedupe by URL + audio, then sort: SUB first, then the
        // provider priority order.
        let mut cards: Vec<NikaCard> = Vec::new();
        let mut seen = HashSet::new();
        for card in converted.flatten() {
            let key = format!("{}|{}", card.url, card.audio.key());
            if seen.insert(key) {
                cards.push(card);
            }
        }
        cards.sort_by(|a, b| {
            a.audio
                .order()
                .cmp(&b.audio.order())
                .then_with(|| provider_rank(&a.provider).cmp(&provider_rank(&b.provider)))
        });
        cards
    }
}

// ---------------------------------------------------------------------------
// AniList resolution
// ---------------------------------------------------------------------------

/// The `AniList` GraphQL query — top 3 anime for a search, optionally
/// format-filtered.
const ANILIST_QUERY: &str = "query ($search: String, $format: MediaFormat) {\
  Page(page: 1, perPage: 3) {\
    media(type: ANIME, search: $search, format: $format) { id idMal title { english romaji } format }\
  }\
}";

/// Find the `AniList` id for a title — the port of `findAniListId`
/// (`AniList` with the 429 retry, then Jikan and Kitsu fallbacks whose
/// matches lack the `AniList` id Anivexa requires, so they answer an
/// honest zero — their `malId`/`title` fed only the scraper's console
/// logs).
async fn find_anilist_id(ctx: &ResolveCtx<'_>, title: &str, media_type: &str) -> Option<u64> {
    // The original title, then the macron-stripped search form.
    let mut variants = vec![title.to_string()];
    let normalized = normalize_title_for_search(title);
    if normalized != title {
        variants.push(normalized);
    }
    let formats: [Option<&str>; 2] = if media_type == "tv" {
        [Some("TV"), None]
    } else {
        [Some("MOVIE"), None]
    };

    for search in &variants {
        for format in formats {
            for attempt in 0..3u32 {
                let body = json!({
                    "query": ANILIST_QUERY,
                    "variables": { "search": search, "format": format },
                })
                .to_string();
                let url = Url::parse(ANILIST_GRAPHQL).ok()?;
                let request = FetchRequest::post(url, body)
                    .with_header("User-Agent", UA)
                    .with_header("Accept", "application/json, text/plain, */*")
                    .with_header("Content-Type", "application/json")
                    .with_timeout(ANILIST_TIMEOUT);
                let Ok(response) = ctx.fetcher.request(request).await else {
                    if attempt < 2 {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    continue;
                };
                if response.status == 429 {
                    // AniList rate-limited — retry after 2 s.
                    tokio::time::sleep(RATE_LIMIT_DELAY).await;
                    continue;
                }
                if !response.is_success() {
                    continue;
                }
                // A malformed body is the JS catch branch — sleep and
                // retry.
                let media: Option<Vec<Value>> = response.json::<Value>().ok().and_then(|payload| {
                    payload
                        .pointer("/data/Page/media")
                        .cloned()
                        .and_then(|media| serde_json::from_value(media).ok())
                });
                let Some(media) = media else {
                    if attempt < 2 {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    continue;
                };
                if let Some(top) = media.first() {
                    return top.get("id").and_then(Value::as_u64);
                }
                // No results for this format — try the next.
                break;
            }
        }
    }

    // The fallbacks still fire (their fetches are behavior), but their
    // matches carry no AniList id.
    jikan_search(ctx, title).await;
    kitsu_search(ctx, title).await;
    None
}

/// The Jikan fallback — the top MAL match (no `AniList` id).
async fn jikan_search(ctx: &ResolveCtx<'_>, title: &str) -> Option<Value> {
    let mut url = Url::parse("https://api.jikan.moe/v4/anime").ok()?;
    url.query_pairs_mut()
        .append_pair("q", title)
        .append_pair("limit", "5")
        .append_pair("sfw", "true");
    fetch_fallback_json(ctx, &url).await
}

/// The Kitsu fallback — a title match (no ids at all).
async fn kitsu_search(ctx: &ResolveCtx<'_>, title: &str) -> Option<Value> {
    let mut url = Url::parse("https://kitsu.app/api/edge/anime").ok()?;
    url.query_pairs_mut()
        .append_pair("filter[text]", title)
        .append_pair("page[limit]", "5");
    fetch_fallback_json(ctx, &url).await
}

/// One fallback search GET.
async fn fetch_fallback_json(ctx: &ResolveCtx<'_>, url: &Url) -> Option<Value> {
    let request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json, text/plain, */*")
        .with_timeout(FALLBACK_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    response.json().ok()
}

/// A trailing ` (YYYY)` year suffix.
static YEAR_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(\d{4}\)\s*$").unwrap_or_else(|e| panic!("valid year-suffix pattern: {e}"))
});

/// A trailing season/part qualifier (`: Season 2 - Part 1`, …).
static SEASON_QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\s*[:\u{ff1a}]\s*(Season|Part)\s+\d+.*|\s*-\s*Part\s+\d+.*|\s+Season\s+\d+.*)$",
    )
    .unwrap_or_else(|e| panic!("valid season-qualifier pattern: {e}"))
});

/// `(\d{3,4})p?$` — the height inside a quality label.
static HEIGHT_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})p?$").unwrap_or_else(|e| panic!("valid height-label pattern: {e}"))
});

/// The `AniList` search normalizer: macrons stripped (Shippūden →
/// Shippuden — `AniList` uses Hepburn without macrons), year and
/// season/part qualifiers dropped.
fn normalize_title_for_search(title: &str) -> String {
    let mut macron_stripped = String::with_capacity(title.len());
    for character in title.chars() {
        match character {
            '\u{016b}' | '\u{016a}' => macron_stripped.push('u'),
            '\u{014d}' | '\u{014c}' => macron_stripped.push('o'),
            '\u{0101}' | '\u{0100}' => macron_stripped.push('a'),
            '\u{012b}' | '\u{012a}' => macron_stripped.push('i'),
            '\u{0113}' | '\u{0112}' => macron_stripped.push('e'),
            other => macron_stripped.push(other),
        }
    }
    let year_stripped = YEAR_SUFFIX.replace(&macron_stripped, "");
    let stripped = SEASON_QUALIFIER.replace(&year_stripped, "");
    stripped.trim().to_string()
}

// ---------------------------------------------------------------------------
// Anivexa episodes + watch
// ---------------------------------------------------------------------------

/// One provider's episode list.
#[derive(Debug, Deserialize)]
struct EpisodeList {
    /// The sub episodes.
    #[serde(default)]
    sub: Option<Vec<Value>>,
    /// The dub episodes.
    #[serde(default)]
    dub: Option<Vec<Value>>,
}

impl EpisodeList {
    /// Whether either audio has episodes.
    fn has_any(&self) -> bool {
        !self.sub.as_deref().unwrap_or_default().is_empty()
            || !self.dub.as_deref().unwrap_or_default().is_empty()
    }

    /// Whether `audio` lists episode `ep`.
    fn has_episode(&self, audio: Audio, ep: u32) -> bool {
        self.episodes(audio).is_some_and(|episodes| {
            episodes
                .iter()
                .any(|entry| episode_number(entry) == Some(ep))
        })
    }

    /// The episode entries of one audio.
    fn episodes(&self, audio: Audio) -> Option<&Vec<Value>> {
        match audio {
            Audio::Sub => self.sub.as_ref(),
            Audio::Dub => self.dub.as_ref(),
        }
    }
}

/// The `number` field of an episode entry (numeric or string form).
fn episode_number(entry: &Value) -> Option<u32> {
    if let Some(number) = entry.get("number").and_then(Value::as_u64) {
        return u32::try_from(number).ok();
    }
    entry
        .get("number")
        .and_then(Value::as_str)
        .and_then(|number| number.parse().ok())
}

/// One provider's episode list — the port of `fetchProviderEpisodes`
/// (`GET /episodes/{provider}/{anilistId}`).
async fn fetch_provider_episodes(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    anilist_id: u64,
) -> Option<EpisodeList> {
    let url = Url::parse(&format!("{ANIVEXA_API}/episodes/{provider}/{anilist_id}")).ok()?;
    let request = anivexa_get(url, EPISODES_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let json: Value = serde_json::from_str(&response.body).ok()?;
    serde_json::from_value(json.get(provider)?.get("episodes")?.clone()).ok()
}

/// One watch query — the port of `fetchWatch`
/// (`GET /watch/{provider}/{anilistId}/{audio}/{provider}-{ep}`).
async fn fetch_watch(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    anilist_id: u64,
    audio: Audio,
    ep_num: u32,
) -> Vec<RawStream> {
    let url = Url::parse(&format!(
        "{ANIVEXA_API}/watch/{provider}/{anilist_id}/{}/{provider}-{ep_num}",
        audio.key()
    ))
    .ok();
    let Some(url) = url else {
        return Vec::new();
    };
    let request = anivexa_get(url, WATCH_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.is_success() {
        return Vec::new();
    }
    let Ok(payload) = serde_json::from_str::<WatchResponse>(&response.body) else {
        return Vec::new();
    };
    payload.streams.unwrap_or_default()
}

/// The `/watch` response.
#[derive(Deserialize)]
struct WatchResponse {
    /// The provider's streams.
    #[serde(default)]
    streams: Option<Vec<RawStream>>,
}

/// One Anivexa GET — the Referer/Origin pair the worker gates on.
fn anivexa_get(url: Url, timeout: Duration) -> FetchRequest {
    FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json, text/plain, */*")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("Referer", format!("{BASE_URL}/"))
        .with_header("Origin", BASE_URL)
        .with_timeout(timeout)
}

// ---------------------------------------------------------------------------
// Stream conversion
// ---------------------------------------------------------------------------

/// Convert one raw stream into a card — the port of `convertStream`
/// with the wrapper's playable filter folded in: encrypted-CDN and
/// embed-only streams are iframes the wrapper drops, so they convert
/// to nothing; direct URLs become candidates for engine validation.
fn convert_stream(raw: &RawStream, provider: &str, audio: Audio) -> Option<NikaCard> {
    let url = raw
        .url
        .clone()
        .or_else(|| raw.embed.clone())
        .filter(|url| url.starts_with("http"))?;

    let is_embed =
        raw.kind.as_deref() == Some("embed") || (raw.url.is_none() && raw.embed.is_some());
    let is_iframe = is_embed && raw.url.is_none();
    // The encrypted CDNs always ship the embed URL as an iframe — the
    // m3u8 body only decrypts inside the page's JS.
    if is_iframe || (is_encrypted_cdn(raw.url.as_deref()) && raw.embed.is_some()) {
        return None;
    }

    // The engine validates each URL with bounded prefixes. A provider must
    // not download every direct file before it can return its candidates.
    let is_mp4 = raw.kind.as_deref() == Some("mp4")
        || Path::new(&url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
    let server = raw.server.clone().unwrap_or_default();
    let server_label = format!("{} {} {}", provider, audio.label(), server)
        .trim()
        .to_string();
    Some(NikaCard {
        url,
        kind: if is_mp4 {
            "video/mp4".to_string()
        } else {
            "application/vnd.apple.mpegurl".to_string()
        },
        name: format!("NikaStream - {server_label}"),
        quality: "1080p".to_string(),
        provider: provider.to_string(),
        audio,
        referer: raw.referer.clone(),
        // The scraper's subtitle mapping: id from the code chain, lang
        // from the label chain.
        subtitles: raw
            .subtitles
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|subtitle| {
                let url = subtitle.url.clone()?;
                let id = subtitle
                    .srclang
                    .clone()
                    .or_else(|| subtitle.lang.clone())
                    .or_else(|| subtitle.language.clone())
                    .unwrap_or_else(|| "en".to_string());
                let lang = subtitle
                    .label
                    .clone()
                    .or_else(|| subtitle.lang.clone())
                    .or_else(|| subtitle.language.clone())
                    .unwrap_or_else(|| "English".to_string());
                Some(ScrapeSubtitle { id, url, lang })
            })
            .collect(),
    })
}

/// Whether a URL sits on an encrypted-CDN host.
fn is_encrypted_cdn(url: Option<&str>) -> bool {
    let Some(url) = url.and_then(|url| Url::parse(url).ok()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    ENCRYPTED_CDN_HOSTS
        .iter()
        .any(|cdn| host == *cdn || host.ends_with(&format!(".{cdn}")))
}

// ---------------------------------------------------------------------------
// The wrapper's result building
// ---------------------------------------------------------------------------

/// The wrapper's result building: the playable filter (all direct by
/// construction), the URL dedupe (the sub/dub twins some providers
/// share), and the enriched card meta — the port of
/// `NikaStream.js`'s loop.
fn build_streams(cards: &[NikaCard], display_title: &str) -> Vec<Stream> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for card in cards {
        // Only direct http(s) streams are playable — iframes never
        // made it this far.
        if !card.url.starts_with("http") || !seen.insert(card.url.clone()) {
            continue;
        }
        let height = parse_height(&card.quality).unwrap_or(1080);
        let languages = if card.audio == Audio::Dub {
            vec![CountryCode::Multi, CountryCode::En]
        } else {
            vec![CountryCode::Multi, CountryCode::Ja]
        };
        let codec = if height >= 2160 { "HEVC" } else { "x264" };
        let audio_word = if card.audio == Audio::Dub {
            "English"
        } else {
            "Japanese"
        };
        // The wrapper's extractServer — the server behind the
        // provider/audio prefix of the card name.
        let server_tag = card_server(card).map_or_else(String::new, |server| format!(" {server}"));
        let provider_tag = if card.provider.is_empty() {
            card.audio.label().to_string()
        } else {
            format!("{} {}{}", card.provider, card.audio.label(), server_tag)
        };
        let label = format!(
            "{display_title} [NikaStream {provider_tag}] {height}p WEB-DL {codec} {audio_word}"
        );

        // The format from the URL path and the MIME marker.
        let path = Url::parse(&card.url)
            .map(|url| url.path().to_lowercase())
            .unwrap_or_default();
        let is_hls = path.contains(".m3u8")
            || path.contains("/playlist")
            || card.kind == "application/vnd.apple.mpegurl";
        let is_mp4 = Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
            || card.kind == "video/mp4";
        let format = if is_hls {
            Format::Hls
        } else if is_mp4 {
            Format::Mp4
        } else {
            Format::Unknown
        };

        let mut meta = StreamMeta {
            dubbed: Some(card.audio == Audio::Dub),
            subbed: Some(card.audio == Audio::Sub),
            languages,
            resolution: Some(height),
            quality: Some("WebDL".to_string()),
            codec: Some(codec.to_string()),
            source_id: Some(ID.to_string()),
            source_label: Some(LABEL.to_string()),
            subtitles: card
                .subtitles
                .iter()
                .filter_map(|subtitle| {
                    let url = Url::parse(&subtitle.url).ok()?;
                    Some(SubtitleTrack {
                        // The wrapper's normalizeSubtitle: the id
                        // (8 chars), the language.
                        label: Some(subtitle.id.chars().take(8).collect()),
                        language: Some(subtitle.lang.clone()),
                        url,
                    })
                })
                .collect(),
            ..StreamMeta::default()
        };
        if let Some(referer) = card.referer.as_deref() {
            // The proxy's triple: User-Agent, Referer, Origin.
            meta.request_headers
                .insert("User-Agent".to_string(), UA.to_string());
            meta.request_headers
                .insert("Referer".to_string(), referer.to_string());
            meta.request_headers.insert(
                "Origin".to_string(),
                referer.trim_end_matches('/').to_string(),
            );
        }

        let mut stream = Stream {
            url: Url::parse(&card.url).unwrap_or_else(|e| panic!("valid stream URL: {e}")),
            format,
            label: Some(label),
            meta,
            ttl: TTL,
            is_external: false,
            behavior_hints: std::collections::BTreeMap::new(),
        };
        // The force-HLS hint: HLS + Referer needed the proxy's
        // content-type check.
        if is_hls && card.referer.is_some() {
            stream
                .behavior_hints
                .insert("nuvioForceHls".to_string(), "1".to_string());
        }
        out.push(stream);
    }
    out
}

/// The server name behind the provider/audio prefix — the wrapper's
/// `extractServer` (the card name's tail after `SUB`/`DUB`).
fn card_server(card: &NikaCard) -> Option<String> {
    let rest = card.name.strip_prefix("NikaStream - ")?;
    let tail = rest.split_once(card.audio.label())?.1;
    let server = tail.strip_prefix(' ')?.trim().to_string();
    if server.is_empty() {
        None
    } else {
        Some(server)
    }
}

/// `(\d{3,4})p?` plus the 4K forms — the wrapper's local
/// `parseHeight` (the bare-number form included, unlike the shared
/// Nuvio parser).
fn parse_height(quality: &str) -> Option<u16> {
    let lower = quality.to_lowercase();
    if lower.contains("4k") || lower.contains("2160") {
        return Some(2160);
    }
    HEIGHT_LABEL
        .captures(&lower)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the display title.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// The provider priority for the sub-then-dub sort (upstream
/// `providerOrder`).
fn provider_rank(provider: &str) -> u8 {
    match provider {
        "reanime" => 0,
        "anikoto" => 1,
        "anibd" => 2,
        "anineko" => 3,
        "animegg" => 4,
        "kaa" => 5,
        _ => 99,
    }
}

// ---------------------------------------------------------------------------
// TMDB resolution (the shared wrapper pattern)
// ---------------------------------------------------------------------------

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
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;

    use super::*;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare
    /// path, so TMDB and the AniList/Jikan/Kitsu calls can be
    /// scripted by path alone.
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

    // -- fixtures ------------------------------------------------------------

    /// The fixture media: One Piece S01E01.
    const TMDB_ID: u64 = 37854;

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> NikaStream {
        NikaStream::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// The `AniList` GraphQL answer.
    fn anilist_page() -> String {
        r#"{"data":{"Page":{"media":[{"id":21,"idMal":21,"title":{"english":"One Piece","romaji":"One Piece"},"format":"TV"}]}}}"#.to_string()
    }

    /// The episode lists: anikoto (sub + dub), anibd (sub only),
    /// reanime (both), anineko (sub) — the rest 404.
    fn episode_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            "/episodes/anikoto/21",
            200,
            r#"{"anikoto":{"episodes":{"sub":[{"number":1}],"dub":[{"number":1}]}}}"#,
        )
        .page(
            "/episodes/anibd/21",
            200,
            r#"{"anibd":{"episodes":{"sub":[{"number":1}],"dub":[]}}}"#,
        )
        .page(
            "/episodes/reanime/21",
            200,
            r#"{"reanime":{"episodes":{"sub":[{"number":1}],"dub":[{"number":1}]}}}"#,
        )
        .page(
            "/episodes/anineko/21",
            200,
            r#"{"anineko":{"episodes":{"sub":[{"number":1}],"dub":[]}}}"#,
        )
    }

    /// The watch answers + the stream validations.
    fn watch_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            "/watch/anikoto/21/sub/anikoto-1",
            200,
            r#"{"streams":[{"url":"https://kryntal.top/hls/one-piece-sub.m3u8","type":"hls","server":"kryntal","referer":"https://anikoto.example/","subtitles":[{"srclang":"en","url":"https://anikoto.example/subs/en.vtt","label":"English"},{"srclang":"ar","url":"https://anikoto.example/subs/ar.vtt","label":"Arabic"}]}]}"#,
        )
        .page(
            "/watch/anikoto/21/dub/anikoto-1",
            200,
            r#"{"streams":[{"url":"https://kryntal.top/hls/one-piece-dub.m3u8","type":"hls","server":"kryntal","referer":"https://anikoto.example/","subtitles":[]}]}"#,
        )
        .page(
            "/watch/anibd/21/sub/anibd-1",
            200,
            r#"{"streams":[{"url":"https://animeapps.top/hls/one-piece.mp4","type":"mp4","server":"main"}]}"#,
        )
        .page(
            "/watch/reanime/21/sub/reanime-1",
            200,
            r#"{"streams":[{"url":"https://reanime-cdn.example/hls/one-piece.m3u8","type":"hls","server":"HD-1"}]}"#,
        )
        .page(
            "/watch/reanime/21/dub/reanime-1",
            200,
            r#"{"streams":[{"url":"https://reanime-cdn.example/hls/one-piece.m3u8","type":"hls","server":"HD-1"}]}"#,
        )
        .page(
            "/watch/anineko/21/sub/anineko-1",
            200,
            r#"{"streams":[{"embed":"https://vivibebe.site/embed/xyz","type":"embed","server":"mirror"}]}"#,
        )
        // The validation probes — every direct URL answers 200.
        .page("/hls/one-piece-sub.m3u8", 200, "#EXTM3U\n")
        .page("/hls/one-piece-dub.m3u8", 200, "#EXTM3U\n")
        .page("/hls/one-piece.mp4", 200, "video")
        .page("/hls/one-piece.m3u8", 200, "#EXTM3U\n")
    }

    /// TMDB + `AniList` + Anivexa pages for the fixture media.
    fn pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"One Piece","first_air_date":"1999-10-20"}"#,
        )
        .page("/", 200, anilist_page())
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "nikastream");
        assert_eq!(info.label, "NikaStream");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://nikastream.blog/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_converts_and_sorts() -> Result<(), SourceError> {
        let mock = Arc::new(watch_pages(episode_pages(
            pages(ScriptedFetcher::default()),
        )));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        // anikoto sub + dub, anibd sub, reanime (its sub/dub twins
        // share one URL — the wrapper's dedupe keeps the sub card);
        // the anineko embed-only stream is dropped.
        assert_eq!(streams.len(), 4);

        // SUB first, then the provider order: reanime before anikoto
        // before anibd, dub last.
        let urls: Vec<&str> = streams.iter().map(|stream| stream.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://reanime-cdn.example/hls/one-piece.m3u8",
                "https://kryntal.top/hls/one-piece-sub.m3u8",
                "https://animeapps.top/hls/one-piece.mp4",
                "https://kryntal.top/hls/one-piece-dub.m3u8",
            ]
        );

        // The anikoto sub card: the enriched label, the proxy header
        // triple, the Japanese flag, the subtitle tracks, HLS.
        let sub = &streams[1];
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(
            sub.label.as_deref(),
            Some("One Piece S01E01 [NikaStream anikoto SUB kryntal] 1080p WEB-DL x264 Japanese")
        );
        assert_eq!(
            sub.meta.request_headers.get("Referer").map(String::as_str),
            Some("https://anikoto.example/")
        );
        assert_eq!(
            sub.meta.request_headers.get("Origin").map(String::as_str),
            Some("https://anikoto.example")
        );
        assert_eq!(
            sub.meta
                .request_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(UA)
        );
        // HLS + Referer → the force-HLS hint.
        assert_eq!(
            sub.behavior_hints.get("nuvioForceHls").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.resolution, Some(1080));
        assert_eq!(sub.meta.quality.as_deref(), Some("WebDL"));
        assert_eq!(sub.meta.codec.as_deref(), Some("x264"));
        assert_eq!(sub.meta.subtitles.len(), 2);
        assert_eq!(
            sub.meta.subtitles[0].url.as_str(),
            "https://anikoto.example/subs/en.vtt"
        );
        assert_eq!(sub.meta.subtitles[0].label.as_deref(), Some("en"));

        // The dub card: the English flag, no subtitles.
        let dub = &streams[3];
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert!(
            dub.label
                .as_deref()
                .is_some_and(|label| label.contains("anikoto DUB kryntal"))
        );
        assert!(dub.meta.subtitles.is_empty());

        // The anibd card: MP4, no referer → no headers, no hint.
        let anibd = &streams[2];
        assert_eq!(anibd.format, Format::Mp4);
        assert!(anibd.meta.request_headers.is_empty());
        assert!(anibd.behavior_hints.is_empty());

        // The reanime card: the sub twin survived the URL dedupe.
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("[NikaStream reanime SUB HD-1]"))
        );
        assert_eq!(streams[0].meta.source_id.as_deref(), Some("nikastream"));
        assert_eq!(streams[0].ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn an_anilist_failure_falls_back_to_jikan_and_answers_zero() {
        // AniList 500s; Jikan answers a MAL-only match — Anivexa needs
        // the AniList id, so the honest zero wins.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"One Piece","first_air_date":"1999-10-20"}"#,
                )
                .page("/", 500, "server error")
                .page(
                    "/v4/anime",
                    200,
                    r#"{"data":[{"mal_id":21,"title_english":"One Piece"}]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert!(mock.requests().iter().any(|request| {
            request
                .url
                .host_str()
                .is_some_and(|host| host.contains("jikan"))
        }));
    }

    #[tokio::test]
    async fn no_provider_episodes_answers_zero() {
        // Every provider's episode list 404s.
        let mock = Arc::new(pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn provider_does_not_download_media_for_status_checks() -> Result<(), SourceError> {
        // The watch answer's URL 404s on validation.
        let mock = Arc::new(
            episode_pages(pages(ScriptedFetcher::default()))
                .page(
                    "/watch/anikoto/21/sub/anikoto-1",
                    200,
                    r#"{"streams":[{"url":"https://kryntal.top/hls/dead.m3u8","type":"hls","server":"kryntal"}]}"#,
                )
                .page(
                    "/watch/anikoto/21/dub/anikoto-1",
                    200,
                    r#"{"streams":[]}"#,
                )
                .page(
                    "/watch/anibd/21/sub/anibd-1",
                    200,
                    r#"{"streams":[{"url":"https://animeapps.top/hls/one-piece.mp4","type":"mp4","server":"main"}]}"#,
                )
                .page(
                    "/watch/reanime/21/sub/reanime-1",
                    200,
                    r#"{"streams":[]}"#,
                )
                .page(
                    "/watch/reanime/21/dub/reanime-1",
                    200,
                    r#"{"streams":[]}"#,
                )
                .page("/hls/one-piece.mp4", 200, "video"),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        assert_eq!(streams.len(), 2);
        assert!(
            mock.requests()
                .iter()
                .all(|r| !r.url.path().starts_with("/hls/"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_series_reference_without_an_episode_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);
        let media = MediaRef {
            id: MediaId::Tmdb(TMDB_ID),
            kind: MediaType::Series,
            season: Some(1),
            episode: None,
        };

        let result = provider.resolve(&ctx, &media).await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(watch_pages(episode_pages(
            pages(ScriptedFetcher::default()),
        )));
        let provider = provider(&mock);
        let media = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: None,
            name: "One Piece".to_string(),
            year: Some(1999),
            season: Some(1),
            episode: Some(1),
        };
        let ctx = ResolveCtx {
            fetcher: mock.as_ref() as &dyn Fetcher,
            media: Some(media),
            source_id: None,
            referer: None,
        };

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        assert_eq!(streams.len(), 4);
        assert!(mock.requests().iter().all(|request| {
            !request
                .url
                .host_str()
                .is_some_and(|host| host.contains("themoviedb"))
        }));
        Ok(())
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(404), 1, 1))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[test]
    fn normalizes_titles_for_the_anilist_search() {
        // Macrons strip, year and season/part qualifiers drop.
        assert_eq!(normalize_title_for_search("Shippūden"), "Shippuden");
        assert_eq!(
            normalize_title_for_search("Naruto: Shippuden (2007)"),
            "Naruto: Shippuden"
        );
        assert_eq!(
            normalize_title_for_search("Attack on Titan: Season 2 - Part 1"),
            "Attack on Titan"
        );
    }

    #[test]
    fn parses_height_labels() {
        assert_eq!(parse_height("1080p"), Some(1080));
        assert_eq!(parse_height("4K"), Some(2160));
        assert_eq!(parse_height("2160p"), Some(2160));
        // The bare-number form the wrapper's local parser accepts.
        assert_eq!(parse_height("720"), Some(720));
        assert_eq!(parse_height("unknown"), None);
    }
}
