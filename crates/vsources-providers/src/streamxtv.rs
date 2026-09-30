//! `StreamXTV`: streamxtv.sbs direct streams plus the anime aggregator.
//!
//! Ports `src/source/StreamXTV.js` + `src/nuvio/streamxtv.cjs`
//! (streamxtv.tech — direct playable HLS up to 4K, anime sub/dub
//! embeds). The wrapper answers three result groups:
//!
//! 1. **Direct streams** — the 20-provider sweep over
//!    `api.framextv.tech` (the `provider=<p>` param is where the 4K
//!    sources live): batches of 5 with a 500 ms delay between batches
//!    (the API throttles bursts), one retry per provider, and a 26 s
//!    internal deadline that returns partial results rather than
//!    letting the source-level timeout discard everything. Each
//!    source carries its own required headers (moon.peakstorm.top
//!    wants `Referer: player.videasy.to/`, Vuflix wants
//!    `ww2.yesmovies.ag/`) — they ride
//!    [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!    through [`build_stream_results`]. Title-level subtitles (deduped
//!    by language, first per language) attach to every stream; the
//!    wrapper enriches each title with `WEB-DL {HEVC HDR|x264}
//!    {audio}` markers.
//! 2. **Anime embeds** — when streamxtv's AniList-backed
//!    `/anime/search` (via the Render backend) finds the title,
//!    Megaplay and `VidNest` sub/dub embed cards join the results.
//! 3. **Movie/TV embed fallback** — only when the direct sweep found
//!    nothing: `VidSrc`, `VidKing`, `VidZee`, and Videasy embed cards (the
//!    source's pre-existing safety net).
//!
//! The sweep runs concurrently with the anime lookup — upstream starts
//! the sweep promise un-awaited so the total time is `max(sweep,
//! anime)` instead of the sum, protecting the 35 s source timeout.
//!
//! Upstream returned the embed cards raw and its resolver chain
//! extracted them (the Nuvio extractor explicitly hands embed-page
//! hosts to the dedicated extractors); here the provider folds that
//! step in by resolving each embed through the shared
//! [`ExtractorRegistry`] and merging its own metadata into the
//! extracted streams (the allwish pattern). Hosts whose dedicated
//! extractors are still unported in Rust (`VidNest`, Videasy, and
//! `VidKing`'s speedracelight API) answer empty and the card drops —
//! upstream's own behavior for hosts with no working extractor.
//!
//! Mappings and cuts (vs. upstream):
//!
//! - The upstream **shared 90 s-cached sweep** (framextv.cjs delegates
//!   to streamxtv.cjs — both hit the same backend) is cut:
//!   cross-provider result sharing is the parent `CachedSource`'s
//!   domain, so this module runs its own sweep every resolve.
//! - `meta.vidking` (movies only, the `VidKing` speedracelight routing)
//!   is covered by passing the context media into the embed context —
//!   the registry's media-keyed fallback joins the same chain.
//! - The scraper's `bingeGroup`/`notWebReady` behavior hints have no
//!   stream-level equivalent in the shared
//!   [`build_stream_results`] port.
//! - The anime-match return keeps only the `AniList` id — the wrapper's
//!   matched title rode the JS return value unused, and the Jikan and
//!   Kitsu fallbacks still fire their fetches but answer no id
//!   (upstream `id: null`: their MAL/Kitsu-only matches cannot fill
//!   the `{anilistId}` templates — the nkastream precedent).
//! - The search's containment and first-word scores are fractions
//!   while the acceptance threshold is 50, so only exact normalized
//!   matches (score 100) can pass — ported verbatim.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency (the allwish precedent): the second query variant
//!   collapses into the first, and precomposed accents simply drop
//!   out of the normalizer — fine for romanized anime titles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

use crate::nuvio::{
    BuildParams, NuvioStream, NuvioSubtitle, build_audio_label, build_stream_results,
    normalize_audio_tracks, parse_height,
};

/// The provider id, upstream `this.id`.
const ID: &str = "streamxtv";
/// The display label, upstream `this.label`.
const LABEL: &str = "StreamXTV";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://streamxtv.tech";
/// The stream API root (the streamxtv.sbs backend, upstream
/// `API_BASE` of the scraper).
const API_BASE: &str = "https://api.framextv.tech";
/// The streamxtv.tech anime aggregator backend (upstream `API_BASE`
/// of the wrapper).
const ANIME_API_BASE: &str = "https://streamx-backend-myr0.onrender.com/api";
/// Upstream `this.ttl` — direct stream URLs are tokened/short-lived.
const TTL: Duration = Duration::from_mins(10);
/// The sweep API's Referer (the header set that gets through the
/// per-IP throttle; upstream `EMBED_REFERER`).
const API_REFERER: &str = "https://embed.streamxtv.tech/";
/// The anime search's Referer.
const ANIME_REFERER: &str = "https://streamxtv.tech/";
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The whole-sweep deadline — must stay below the source-level race
/// so partial results survive (upstream `SWEEP_DEADLINE_MS`, Task 84's
/// 26 s).
const SWEEP_DEADLINE: Duration = Duration::from_secs(26);
/// Providers per batch (the API throttles bursts).
const BATCH_SIZE: usize = 5;
/// Delay between batches (upstream `BATCH_DELAY_MS`).
const BATCH_DELAY: Duration = Duration::from_millis(500);
/// One provider request (upstream `REQUEST_TIMEOUT_MS`, Task 84's
/// 9 s).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(9);
/// Retries per provider (upstream `REQUEST_RETRIES`).
const REQUEST_RETRIES: u32 = 1;
/// Backoff before the retry — upstream `1500 * (attempt + 1)`.
const RETRY_BACKOFF: Duration = Duration::from_millis(1500);
/// The wrapper's `fetchJson` timeout — the Render free-tier backend
/// sleeps when idle; first requests after inactivity take 15–30 s.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Stop starting new batches once this much of the deadline remains
/// (the JS `SWEEP_DEADLINE_MS - 8000` gate; Task 84's 18 s stop
/// check).
const BATCH_STOP_MARGIN: Duration = Duration::from_secs(8);
/// Subtitles are deduped by language, capped (upstream
/// `MAX_SUBTITLES`).
const MAX_SUBTITLES: usize = 20;

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

/// One movie/TV embed fallback — the wrapper's `MOVIE_TV_PROVIDERS`
/// (providers with working extractors in this addon; embed fallback
/// only).
struct EmbedProvider {
    /// The provider's display label.
    label: &'static str,
    /// The movie embed template (`{id}`).
    movie: &'static str,
    /// The TV embed template (`{id}`, `{s}`, `{e}`).
    tv: &'static str,
}

/// The movie/TV embed fallbacks — `VidSrc`, `VidKing`, `VidZee`, and
/// Videasy.
const MOVIE_TV_PROVIDERS: [EmbedProvider; 4] = [
    EmbedProvider {
        label: "VidSrc",
        movie: "https://vidsrc-embed.ru/embed/movie/{id}?autoplay=0",
        tv: "https://vidsrc-embed.ru/embed/tv/{id}-{s}-{e}?autoplay=0&autonext=0",
    },
    EmbedProvider {
        label: "VidKing",
        movie: "https://www.vidking.net/embed/movie/{id}?autoPlay=false",
        tv: "https://www.vidking.net/embed/tv/{id}/{s}/{e}?autoPlay=false&nextEpisode=false",
    },
    EmbedProvider {
        label: "Vidzee",
        movie: "https://player.vidzee.wtf/embed/movie/{id}",
        tv: "https://player.vidzee.wtf/embed/tv/{id}?season={s}&episode={e}",
    },
    EmbedProvider {
        label: "Videasy",
        movie: "https://player.videasy.net/movie/{id}",
        tv: "https://player.videasy.net/tv/{id}/{s}/{e}",
    },
];

/// One anime embed — the wrapper's `ANIME_PROVIDERS` (Megaplay is the
/// most reliable, with sub/dub support).
struct AnimeProvider {
    /// The provider's display label.
    label: &'static str,
    /// The embed template (`{anilistId}`, `{ep}`, `{subDub}`).
    url: &'static str,
}

/// The anime embeds — Megaplay and `VidNest`.
const ANIME_PROVIDERS: [AnimeProvider; 2] = [
    AnimeProvider {
        label: "Megaplay",
        url: "https://megaplay.buzz/stream/ani/{anilistId}/{ep}/{subDub}",
    },
    AnimeProvider {
        label: "VidNest",
        url: "https://vidnest.fun/anime/{anilistId}/{ep}/{subDub}",
    },
];

/// The sort order of normalized qualities (4K first, upstream
/// `Q_ORDER`).
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

/// The `StreamXTV` provider.
pub struct StreamXTV {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The embed-card resolver (the anime and fallback embeds).
    registry: Arc<ExtractorRegistry>,
}

impl StreamXTV {
    /// A provider over the shared TMDB client and extractor registry.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                // Upstream `this.countryCodes` — [multi, ja]. The
                // build-time flags are always the anime-aware override
                // ([multi, ja, en] or [multi, en]); this default only
                // describes the source.
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            registry,
        }
    }
}

#[async_trait]
impl Source for StreamXTV {
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

        // The sweep and the anime lookup run concurrently — upstream
        // starts the sweep promise un-awaited, so the total time is
        // the max of the two, not the sum.
        let sweep = self.sweep(ctx, media, tmdb_id);
        let anime = find_anilist_id(ctx, &name);
        let (mut direct, anilist_id) = tokio::join!(sweep, anime);
        let is_anime = anilist_id.is_some();

        let mut streams = Vec::new();
        if !direct.is_empty() {
            // The wrapper's enrichment: audio markers + WEB-DL/HEVC/
            // HDR tags on every title, then the anime-aware language
            // defaults.
            enrich(&mut direct, is_anime);
            let country_codes = if is_anime {
                vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::En]
            };
            streams.extend(build_stream_results(&BuildParams {
                streams: &direct,
                title: &title,
                source_id: ID,
                source_label: LABEL,
                country_codes: &country_codes,
                ttl: TTL,
            }));
        }

        // The anime path — Megaplay/VidNest sub/dub embeds. Skipped
        // when only the Jikan/Kitsu fallbacks matched: every template
        // requires `{anilistId}`.
        if let Some(anilist_id) = anilist_id {
            let episode = if media.season.is_some() {
                media.episode.unwrap_or(1)
            } else {
                1
            };
            for sub_dub in ["sub", "dub"] {
                for provider in ANIME_PROVIDERS {
                    let embed = provider
                        .url
                        .replace("{anilistId}", &anilist_id.to_string())
                        .replace("{ep}", &episode.to_string())
                        .replace("{subDub}", sub_dub);
                    let languages = if sub_dub == "dub" {
                        vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
                    } else {
                        vec![CountryCode::Multi, CountryCode::Ja]
                    };
                    let label = format!("{title} ({} {})", provider.label, sub_dub.to_uppercase());
                    streams.extend(self.resolve_embed(ctx, &embed, &label, languages).await);
                }
            }
        }

        // The movie/TV embed fallback — only when the direct API
        // returned nothing, preserving the source's pre-existing
        // behavior as a safety net. The episode defaults to 1 like
        // the anime path's `|| 1` (a season reference without an
        // episode would interpolate a literal `undefined` upstream).
        if direct.is_empty() {
            for provider in MOVIE_TV_PROVIDERS {
                let embed = match media.season {
                    Some(season) => provider
                        .tv
                        .replace("{id}", &tmdb_id.to_string())
                        .replace("{s}", &season.to_string())
                        .replace("{e}", &media.episode.unwrap_or(1).to_string()),
                    None => provider.movie.replace("{id}", &tmdb_id.to_string()),
                };
                let label = format!("{title} ({})", provider.label);
                streams.extend(
                    self.resolve_embed(ctx, &embed, &label, vec![CountryCode::Multi])
                        .await,
                );
            }
        }

        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(streams)
    }
}

impl StreamXTV {
    /// The 20-provider batched sweep — the port of the scraper's
    /// `getStreams`: every provider with the `provider=<p>` param,
    /// deduped by URL, sorted 4K-first.
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
                // Title-level subtitles — captured from the first
                // provider that has any (identical across providers).
                if shared_subs.as_ref().is_none_or(Vec::is_empty) {
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
                    let kind = if source.kind.as_deref() == Some("dash") || url.contains(".mpd") {
                        "application/dash+xml"
                    } else {
                        "application/vnd.apple.mpegurl"
                    };
                    // Per-source headers, normalized to the casing
                    // `build_stream_results` reads; harmless keys
                    // dropped.
                    let mut stream = NuvioStream::new(url)
                        .with_quality(quality.clone())
                        .with_kind(kind)
                        .with_name(format!("StreamXTV - {quality} {server} ({provider})"))
                        .with_title(format!("StreamXTV {provider} {quality} {server}"));
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
                    // verbatim whenever present (sparsely populated —
                    // null on most backends).
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

    /// Resolve one embed card through the registry — upstream
    /// returned the raw embed URL and its resolver chain extracted it
    /// (the Nuvio extractor hands embed-page hosts to the dedicated
    /// extractors); the provider folds that step in and merges its own
    /// metadata into the extracted streams. Unclaimed or unresolvable
    /// hosts answer empty and the card drops — upstream's own answer
    /// for hosts with no working extractor.
    async fn resolve_embed(
        &self,
        ctx: &ResolveCtx<'_>,
        embed: &str,
        label: &str,
        languages: Vec<CountryCode>,
    ) -> Vec<Stream> {
        let Ok(url) = Url::parse(embed) else {
            return Vec::new();
        };
        // The context media covers upstream's `meta.vidking` channel:
        // the registry's media-keyed fallback joins the same chain
        // (movies only upstream — speedracelight returns wrong content
        // for series/anime).
        let embed_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: ctx.media.clone(),
            source_id: Some(self.info.id.as_str()),
            referer: None,
        };
        let Ok(extracted) = self.registry.extract(&embed_ctx, &url).await else {
            return Vec::new();
        };
        let mut streams = Vec::new();
        for mut stream in extracted {
            stream.meta.source_id = Some(self.info.id.clone());
            stream.meta.source_label = Some(self.info.label.clone());
            stream.meta.languages.clone_from(&languages);
            stream.label = Some(label.to_string());
            streams.push(stream);
        }
        streams
    }
}

/// One provider JSON fetch with the retry — the port of the scraper's
/// `fetchJson` (the API is rate-limited, so transient 5xx/timeouts
/// retry with a short backoff; `None` on hard failure).
async fn fetch_provider(
    ctx: &ResolveCtx<'_>,
    provider: &str,
    is_tv: bool,
    tmdb_id: u64,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<ProviderResponse> {
    let mut url = Url::parse(&format!("{API_BASE}/api/stream"))
        .unwrap_or_else(|e| panic!("valid StreamXTV API URL: {e}"));
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
            tokio::time::sleep(RETRY_BACKOFF * (attempt + 1)).await;
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
    /// The MIME hint (`dash` marks DASH; `.mpd` URLs too).
    #[serde(rename = "type", default)]
    kind: Option<String>,
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
    /// The hotlink Referer (lowercase casing).
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
/// the port of `mapSubtitles` (the API often returns "English" and
/// "English (2)" — the first per language is the primary track).
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
/// placeholders like `dcloud` → `Auto`).
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

/// The wrapper's title enrichment — StreamXTV.js's markers:
/// `WEB-DL`, `HEVC HDR`/`x264` by height, and the audio label (the
/// API's own `audioTracks` when present, else the anime-aware
/// default — unlike `FrameX`, the wrapper leaves `audioTracks`
/// untouched).
fn enrich(streams: &mut [NuvioStream], is_anime: bool) {
    for stream in streams {
        let height = parse_height(stream.quality.as_deref());
        let tracks = normalize_audio_tracks(stream.audio_tracks.as_ref());
        let mut markers = vec![
            "WEB-DL".to_string(),
            match height {
                Some(2160) => "HEVC HDR".to_string(),
                _ => "x264".to_string(),
            },
        ];
        let audio = build_audio_label(&tracks, stream.has_multiple_audio)
            .unwrap_or_else(|| if is_anime { "Japanese" } else { "English" }.to_string());
        markers.push(audio);
        let title = stream.title.clone().unwrap_or_default();
        stream.title = Some(format!("{title} {}", markers.join(" ")));
    }
}

/// The streamxtv anime match — the port of the wrapper's
/// `findAniListId`: the AniList-backed `/anime/search` over the
/// query variants, then the Jikan and Kitsu fallback fetches (their
/// matches carry no `AniList` id — upstream `id: null` — so they
/// cannot answer).
async fn find_anilist_id(ctx: &ResolveCtx<'_>, name: &str) -> Option<u64> {
    let name_norm = normalize(name);
    for query in query_variants(name) {
        let url = format!(
            "{ANIME_API_BASE}/anime/search?q={}",
            encode_component(&query)
        );
        let Some(data) = fetch_json(ctx, &url, Some(ANIME_REFERER)).await else {
            continue;
        };
        let Ok(response) = serde_json::from_value::<SearchResponse>(data) else {
            continue;
        };
        if response.results.is_empty() {
            continue;
        }
        // The best match by normalized title comparison — only exact
        // matches (score 100) clear the 50 threshold: the containment
        // and first-word scores are fractions.
        if let Some((result, score)) = best_match(&response.results, &name_norm)
            && score >= 50.0
        {
            return anilist_id(result.id.as_ref());
        }
    }

    // The fallbacks still fire (their fetches are behavior), but a
    // Jikan/Kitsu match cannot fill the `{anilistId}` templates.
    jikan_search(ctx, name).await;
    kitsu_search(ctx, name).await;
    None
}

/// The `/anime/search` response.
#[derive(Deserialize)]
struct SearchResponse {
    /// The results.
    #[serde(default)]
    results: Vec<SearchResult>,
}

/// One `/anime/search` result.
#[derive(Deserialize)]
struct SearchResult {
    /// The `AniList` id (the backend mirrors `AniList`; a number or a
    /// numeric string).
    #[serde(default)]
    id: Option<Value>,
    /// The title.
    #[serde(default)]
    title: String,
}

/// A normalized string's char count as the score base (u32-wrapping,
/// like the workspace's other length ratios).
fn char_count(text: &str) -> f64 {
    f64::from(u32::try_from(text.chars().count()).unwrap_or(u32::MAX))
}

/// The best search result — the port of the JS scoring: exact
/// normalized equality scores 100, containment the length ratio, a
/// first-word prefix that ratio times 0.7.
fn best_match<'a>(results: &'a [SearchResult], name_norm: &str) -> Option<(&'a SearchResult, f64)> {
    let mut best: Option<(&SearchResult, f64)> = None;
    for result in results {
        let result_norm = normalize(&result.title);
        if result_norm.is_empty() {
            continue;
        }
        if result_norm == name_norm {
            return Some((result, 100.0));
        }
        let mut score: f64 = 0.0;
        if result_norm.contains(name_norm) || name_norm.contains(&result_norm) {
            let ratio = char_count(&result_norm).min(char_count(name_norm))
                / char_count(&result_norm).max(char_count(name_norm));
            score = score.max(ratio);
        }
        let first_word = name_norm.split(' ').next().unwrap_or_default();
        if first_word.chars().count() > 3 && result_norm.starts_with(first_word) {
            let prefix = char_count(first_word) / char_count(&result_norm).max(1.0) * 0.7;
            score = score.max(prefix);
        }
        if score > best.map_or(0.0, |(_, score)| score) {
            best = Some((result, score));
        }
    }
    best
}

/// The `AniList` id of a raw search value — a JSON number or its
/// numeric string form.
fn anilist_id(raw: Option<&Value>) -> Option<u64> {
    raw.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

/// Search query variants, deduped in order — the port of the JS
/// `[name, NFD(name), punctuation-spaced name]`. Unicode `NFD`
/// decomposition is unavailable without an extra dependency (the
/// allwish precedent), so the NFD variant collapses into the raw
/// name and drops out of the dedup.
fn query_variants(name: &str) -> Vec<String> {
    let spaced: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character.is_whitespace() {
                character
            } else {
                ' '
            }
        })
        .collect();
    let spaced = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut variants = vec![name.to_string()];
    if !spaced.is_empty() && !variants.contains(&spaced) {
        variants.push(spaced);
    }
    variants
        .into_iter()
        .filter(|variant| !variant.trim().is_empty())
        .collect()
}

/// Normalize for fuzzy title matching — the port of the JS
/// `normalize`: lowercase, diacritics stripped, non-alphanumerics
/// dropped, whitespace collapsed. Precomposed accents simply drop
/// out (no `NFD` decomposition) — fine for romanized anime titles.
fn normalize(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// GET a URL as JSON — the port of the wrapper's `fetchJson` (null
/// on non-200 or malformed bodies).
async fn fetch_json(ctx: &ResolveCtx<'_>, url: &str, referer: Option<&str>) -> Option<Value> {
    let url = Url::parse(url).ok()?;
    let mut request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json,text/plain,*/*")
        .with_timeout(FETCH_TIMEOUT);
    if let Some(referer) = referer {
        request = request.with_header("Referer", referer);
    }
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    serde_json::from_str(&response.body).ok()
}

/// The Jikan fallback — the port of the wrapper's MAL probe (its
/// MAL-only match carries no `AniList` id; only the fetch remains).
async fn jikan_search(ctx: &ResolveCtx<'_>, name: &str) {
    let Ok(mut url) = Url::parse("https://api.jikan.moe/v4/anime") else {
        return;
    };
    url.query_pairs_mut()
        .append_pair("q", name)
        .append_pair("limit", "5")
        .append_pair("sfw", "true");
    fallback_fetch(ctx, &url).await;
}

/// The Kitsu fallback — the port of the wrapper's Kitsu probe (no
/// `AniList` or MAL ids at all; only the fetch remains).
async fn kitsu_search(ctx: &ResolveCtx<'_>, name: &str) {
    let Ok(mut url) = Url::parse("https://kitsu.app/api/edge/anime") else {
        return;
    };
    url.query_pairs_mut()
        .append_pair("filter[text]", name)
        .append_pair("page[limit]", "5");
    fallback_fetch(ctx, &url).await;
}

/// One fallback search GET.
async fn fallback_fetch(ctx: &ResolveCtx<'_>, url: &Url) {
    let request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json, text/plain, */*")
        .with_timeout(FETCH_TIMEOUT);
    let _ = ctx.fetcher.request(request).await;
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

/// Percent-encode like the JS `encodeURIComponent`.
fn encode_component(s: &str) -> String {
    /// The uppercase hex digits for percent escapes.
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
    ];
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)]);
            out.push(HEX[usize::from(byte & 0x0F)]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::{ExtractorError, FetchError};
    use vsources_core::traits::{Extractor, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;

    use super::*;

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare path,
    /// so the anime search (`?q=…`) can be scripted by path alone.
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

    /// The stub's stream TTL — thirty minutes.
    const STUB_TTL: Duration = Duration::from_mins(30);

    /// A stand-in for the Megaplay and `VidSrc` extractors: claims
    /// megaplay.buzz and vidsrc-embed.ru embeds, answering one direct
    /// HLS stream whose path echoes the embed's (the assertions read
    /// the built template URLs back through it).
    struct StubExtractor;

    #[async_trait]
    impl Extractor for StubExtractor {
        fn id(&self) -> &'static str {
            "embed-stub"
        }

        fn label(&self) -> &'static str {
            "EmbedStub"
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            matches!(url.host_str(), Some("megaplay.buzz" | "vidsrc-embed.ru"))
        }

        async fn extract(
            &self,
            _ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            let path = url.path().trim_start_matches('/');
            let direct = Url::parse(&format!("https://cdn.example.com/{path}/index.m3u8"))
                .unwrap_or_else(|e| panic!("valid stub URL: {e}"));
            Ok(vec![Stream::new(direct, Format::Hls).with_ttl(STUB_TTL)])
        }
    }

    /// A registry over the stub extractor.
    fn registry() -> Arc<ExtractorRegistry> {
        Arc::new(ExtractorRegistry::new(vec![Arc::new(StubExtractor)]))
    }

    /// The provider over a TMDB client and the stub registry sharing
    /// the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> StreamXTV {
        StreamXTV::new(
            Arc::new(TmdbClient::new("test-key", mock.clone())),
            registry(),
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

    /// The fixture media (Dune: Part Two, TMDB 693134).
    const TMDB_ID: u64 = 693_134;

    /// TMDB details + every provider answering an empty success (only
    /// `provider` gets real sources); the anime search finds nothing.
    fn mock_with(provider: &str, provider_body: &str) -> ScriptedFetcher {
        let mut mock = ScriptedFetcher::default()
            .page(
                format!("/3/movie/{TMDB_ID}"),
                200,
                r#"{"title":"Dune: Part Two","release_date":"2024-02-27"}"#,
            )
            .page("/api/anime/search", 200, r#"{"results":[]}"#);
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
        assert_eq!(info.id, "streamxtv");
        assert_eq!(info.label, "StreamXTV");
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
            Some("https://streamxtv.tech/")
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
        // flags. The serverless source falls back to the provider name
        // — streamxtv.cjs, unlike framextv.cjs, does not dedupe it, so
        // the title template doubles it.
        assert!(streams[0].label.as_deref().is_some_and(|label| {
            label.contains(
                "StreamXTV barbarian 4K barbarian WEB-DL HEVC HDR Dual Audio (Hindi + English)",
            )
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
        // The English default audio marker when the API omits tracks
        // (the anime search found nothing).
        assert!(
            streams[1]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("WEB-DL x264 English"))
        );
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("streamxtv"))
        );
        assert_eq!(streams[0].ttl, TTL);
        // All 20 providers were swept.
        assert_eq!(mock.hits("/api/stream"), 20);
        // No anime embeds without an AniList match, and no movie/TV
        // fallback while the direct sweep found streams.
        assert!(
            mock.requests()
                .iter()
                .all(|request| !request.url.host_str().is_some_and(|host| {
                    host == "megaplay.buzz" || host == "vidnest.fun" || host == "vidsrc-embed.ru"
                }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_anime_match_adds_japanese_markers_and_embeds() -> Result<(), SourceError> {
        let body = r#"{"success":true,"sources":[{"url":"https://moon.peakstorm.top/hls/tv/master.m3u8","quality":"1080p"}]}"#;
        let mut mock = ScriptedFetcher::default();
        for name in ALL_PROVIDERS {
            let provider_body = if name == "barbarian" {
                body.to_string()
            } else {
                r#"{"success":true,"sources":[]}"#.to_string()
            };
            mock = mock.page(
                format!("/api/stream?type=tv&id={TMDB_ID}&season=1&episode=2&provider={name}"),
                200,
                provider_body,
            );
        }
        // The anime search: the second result is the exact normalized
        // match (AniList id 21 — a numeric string, as the backend
        // mirrors AniList).
        let mock = Arc::new(mock.page(
            "/api/anime/search",
            200,
            r#"{"results":[{"id":1535,"title":"Solo Leveling Season 2"},{"id":"21","title":"Solo Leveling"}]}"#,
        ));
        let provider = provider(&mock);
        // The engine pre-resolves TMDB metadata; the provider needs no
        // TMDB fetch.
        let resolved = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: None,
            name: "Solo Leveling".to_string(),
            year: Some(2024),
            season: Some(1),
            episode: Some(2),
        };
        let fetcher: &dyn Fetcher = mock.as_ref();
        let ctx = ResolveCtx {
            fetcher,
            media: Some(resolved),
            source_id: None,
            referer: None,
        };

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        // One direct stream + the two Megaplay embeds (sub and dub) —
        // VidNest has no claimant in the stub registry and drops.
        assert_eq!(streams.len(), 3, "direct + Megaplay sub + Megaplay dub");
        let direct = &streams[0];
        assert!(
            direct
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Solo Leveling S01E02"))
                && direct
                    .label
                    .as_deref()
                    .is_some_and(|label| label.contains("WEB-DL x264 Japanese"))
        );
        assert!(direct.meta.languages.contains(&CountryCode::Ja));

        let sub = &streams[1];
        assert_eq!(
            sub.url.as_str(),
            "https://cdn.example.com/stream/ani/21/2/sub/index.m3u8"
        );
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(
            sub.label.as_deref(),
            Some("Solo Leveling S01E02 (Megaplay SUB)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.source_id.as_deref(), Some("streamxtv"));
        assert_eq!(sub.meta.extractor_label.as_deref(), Some("EmbedStub"));
        assert_eq!(sub.ttl, STUB_TTL);

        let dub = &streams[2];
        assert_eq!(
            dub.url.as_str(),
            "https://cdn.example.com/stream/ani/21/2/dub/index.m3u8"
        );
        assert_eq!(
            dub.label.as_deref(),
            Some("Solo Leveling S01E02 (Megaplay DUB)")
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        );

        // The stream count already proves the VidNest cards found no
        // claimant (the stub claims megaplay.buzz and vidsrc-embed.ru
        // only) and dropped; the echoed paths carry the template's
        // anilist id, episode, and sub/dub.
        Ok(())
    }

    #[tokio::test]
    async fn a_near_miss_anime_search_adds_no_embeds() -> Result<(), SourceError> {
        let body = r#"{"success":true,"sources":[{"url":"https://moon.peakstorm.top/hls/tv/master.m3u8","quality":"1080p"}]}"#;
        let mut mock = ScriptedFetcher::default();
        for name in ALL_PROVIDERS {
            let provider_body = if name == "goblin" {
                body.to_string()
            } else {
                r#"{"success":true,"sources":[]}"#.to_string()
            };
            mock = mock.page(
                format!("/api/stream?type=movie&id={TMDB_ID}&provider={name}"),
                200,
                provider_body,
            );
        }
        // Every result merely contains the name — the containment
        // score is a fraction, below the 50 threshold, so the anime
        // path never fires (verbatim upstream quirk).
        let mock = Arc::new(
            mock.page(
                "/api/anime/search",
                200,
                r#"{"results":[{"id":21,"title":"Solo Leveling Season 2"}]}"#,
            )
            .page(
                "/3/movie/693134",
                200,
                r#"{"title":"Solo Leveling","release_date":"2024-01-06"}"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("WEB-DL x264 English"))
        );
        assert!(
            mock.requests()
                .iter()
                .all(|request| request.url.host_str() != Some("megaplay.buzz"))
        );
        // The Jikan and Kitsu fallback probes fired.
        assert!(mock.hits("/v4/anime") >= 1);
        assert!(mock.hits("/api/edge/anime") >= 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_sweep_falls_back_to_embed_cards() -> Result<(), SourceError> {
        let mock = Arc::new(mock_with("barbarian", r#"{"success":true,"sources":[]}"#));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // Only VidSrc has a claimant in the stub registry; VidKing,
        // VidZee, and Videasy drop.
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.com/embed/movie/693134/index.m3u8"
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Dune: Part Two (2024) (VidSrc)")
        );
        assert_eq!(streams[0].meta.languages, vec![CountryCode::Multi]);
        assert_eq!(streams[0].meta.source_id.as_deref(), Some("streamxtv"));
        // The sweep still ran in full before the fallback.
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
        let mock = Arc::new(mock.page("/api/anime/search", 200, r#"{"results":[]}"#));
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
    async fn an_unresolvable_everything_answers_not_found() {
        let mock = Arc::new(mock_with("barbarian", r#"{"success":true,"sources":[]}"#));
        // A registry whose extractors claim nothing: the embed cards
        // find no claimant and drop.
        let provider = StreamXTV::new(
            Arc::new(TmdbClient::new("test-key", mock.clone())),
            Arc::new(ExtractorRegistry::new(Vec::new())),
        );
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

    #[test]
    fn scores_only_exact_matches_above_the_threshold() {
        let results = vec![
            SearchResult {
                id: Some(Value::from(1)),
                title: "Solo Leveling Season 2".to_string(),
            },
            SearchResult {
                id: Some(Value::from(2)),
                title: "Solo Leveling".to_string(),
            },
        ];
        // Exact normalized match — short-circuits at 100.
        let (result, score) =
            best_match(&results, "solo leveling").unwrap_or_else(|| panic!("the exact match wins"));
        assert_eq!(result.id, Some(Value::from(2)));
        assert!((score - 100.0).abs() < f64::EPSILON);
        // Containment and first-word matches stay fractions — below
        // the 50 acceptance threshold.
        let (result, score) =
            best_match(&results, "solo").unwrap_or_else(|| panic!("a first-word match scores"));
        assert_eq!(result.id, Some(Value::from(2)));
        assert!(score < 50.0, "the first-word score is {score}");
        let (_, score) = best_match(&results, "solo leveling season")
            .unwrap_or_else(|| panic!("containment scores"));
        assert!(score < 50.0, "the containment score is {score}");
        // No match at all.
        assert!(best_match(&results, "frieren").is_none());
    }

    #[test]
    fn dedupes_query_variants() {
        // The punctuation-spaced variant joins; the NFD variant
        // collapsed into the raw name (no decomposition available).
        assert_eq!(query_variants("Solo Leveling"), vec!["Solo Leveling"]);
        assert_eq!(
            query_variants("Frieren: Beyond Journey's End"),
            vec![
                "Frieren: Beyond Journey's End",
                "Frieren Beyond Journey s End"
            ]
        );
    }
}
