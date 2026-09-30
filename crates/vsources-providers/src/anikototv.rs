//! `AnikotoTV`: anime sub/dub HLS from megaplay.buzz.
//!
//! Ports `src/source/AnikotoTV.js` + `src/nuvio/anikototv.cjs` (the
//! wrapper and the obfuscated scraper folded into one module):
//!
//! 1. resolve the anime id for the TMDB entry —
//!    `GET https://arm.haglund.dev/api/v2/tmdb?id={tmdb}` (plus
//!    `&s=&e=` for series) answers `{ mal, anilist, episode }`; when
//!    ARM has no mapping, the `AniList` GraphQL bridge
//!    `query ($search: String) { Media (search: $search, type: ANIME)
//!    { id idMal } }` runs on the season-adjusted title
//!    (`{title} Season {n}` for later seasons), retrying the plain
//!    title when the adjusted one has no MAL id;
//! 2. sweep megaplay's `Vidstream` server for both audio tracks:
//!    `GET https://megaplay.buzz/stream/{mal|ani}/{id}/{absolute
//!    episode}/{sub|dub}` → `data-id="(\d+)"` (an `<iframe src>` page
//!    is followed once) → `GET /stream/getSources?id={dataId}` with
//!    `X-Requested-With: XMLHttpRequest`;
//! 3. the 2026-09 sources API answers the encrypted shape
//!    `{ tracks, enc }` instead of `{ sources: { file } }` — `enc`
//!    decrypts through [`decrypt_megaplay_enc`] (the upstream
//!    `installMegaplayShim` fetch patch, folded into the parse here);
//! 4. the master m3u8 is probed once for `RESOLUTION=\d+x(\d+)` (the
//!    quality label, `1080p` fallback), its caption `tracks` ride
//!    along as subtitles, and the card ships with the megaplay
//!    hotlink headers (`Referer`/`Origin: megaplay.buzz`);
//! 5. the whole scraper races a 25 s deadline (`callNuvioProvider`'s
//!    `timeoutMs`), and the raw streams flow through
//!    [`build_stream_results`].
//!
//! Cuts and mappings (vs. upstream):
//!
//! - The wrapper's anime-only TMDB genre gate (`isAnimeContent` —
//!   Animation genre 16 or `original_language: ja`) is cut: the shared
//!   [`TmdbClient`] exposes no genres and `ctx.media` carries none
//!   (the `itachi`/`cinebyrocks` precedent). The scraper's `AniList`
//!   resolution still naturally fails for non-anime titles, which
//!   answers the same zero.
//! - The TMDB refetch inside the scraper (`getTMDBDetails`) is folded
//!   into the wrapper's resolution: the title and year come from
//!   `ctx.media`/[`TmdbClient`], and the episode title and duration
//!   keep the scraper's own failure defaults (`Episode {n}`, `24 min`).
//! - `getAbsoluteEpisode`'s TVDB login + episodes chain and the
//!   `aiometadata.elfhosted.com` meta scrape are cut (they refine
//!   absolute numbering only on the `AniList`-fallback path for season
//!   ≥ 2, and need TMDB/TVDB surfaces the SDK does not expose) — the
//!   passed episode number stands, exactly the chain's own terminal
//!   fallback.
//! - `getTMDBSeasonName` (season names for sequel search titles) is
//!   cut for the same reason; the null branch upstream already builds
//!   `{title} Season {n}`.
//! - The scraper's movie branch is unreachable (the wrapper is
//!   series-only and bails without a season), so only the series path
//!   is ported.
//! - `meta.title`/the scraper's `size` (the same 4-line description
//!   string) become [`Stream::label`] and `size` parsing respectively
//!   — the string carries no `GB|MB|TB` figure, so the byte size stays
//!   unset, exactly like the JS `parseSize`.
//! - The per-call random mobile UA becomes a round-robin counter
//!   (the crate carries no RNG); the megaplay page fetches rotate
//!   through the same four upstream UAs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::megaplay::decrypt_megaplay_enc;
use crate::nuvio::{BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline};

/// The provider id, upstream `this.id`.
const ID: &str = "anikototv";
/// The display label, upstream `this.label`.
const LABEL: &str = "AnikotoTV";
/// The catalog origin, upstream `this.baseUrl` (streams resolve on
/// megaplay.buzz).
const BASE_URL: &str = "https://anikototv.com";
/// The ARM id-mapping bridge, upstream `arm.haglund.dev`.
const ARM_API: &str = "https://arm.haglund.dev/api/v2/tmdb";
/// The `AniList` GraphQL endpoint.
const ANILIST_GQL: &str = "https://graphql.anilist.co";
/// The megaplay host, upstream `domain: "megaplay.buzz"`.
const MEGAPLAY: &str = "https://megaplay.buzz";
/// The megaplay server id, upstream `{id: "Vidstream"}`.
const SERVER_ID: &str = "Vidstream";
/// Upstream `this.ttl`.
const TTL: Duration = Duration::from_mins(10);
/// One API call (the family's `timeout: { request: 15000 }`).
const API_TIMEOUT: Duration = Duration::from_secs(15);
/// The scraper's outer race — `callNuvioProvider`'s `timeoutMs:
/// 25000` (kept under the 30 s source timeout).
const SWEEP_DEADLINE: Duration = Duration::from_secs(25);
/// The `AniList` GraphQL query, verbatim from upstream.
const ANILIST_QUERY: &str =
    " query ($search: String) { Media (search: $search, type: ANIME) { id idMal } } ";
/// The scraper's mobile UAs (`MOBILE_UAS`), rotated per call.
const MOBILE_UAS: [&str; 4] = [
    "Mozilla/5.0 (Linux; Android 14; Pixel 8 Pro) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Linux; Android 13; SM-S918B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/116.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Linux; Android 12; Pixel 6) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/115.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
];

/// `data-id="(\d+)"` — megaplay's stream-page id marker.
static DATA_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-id="(\d+)""#).unwrap_or_else(|e| panic!("valid data-id pattern: {e}"))
});

/// `<iframe src="…">` — the nested player page fallback.
static IFRAME_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<iframe[^>]*src="([^"]+)""#)
        .unwrap_or_else(|e| panic!("valid iframe pattern: {e}"))
});

/// `RESOLUTION=\d+x(\d+)` — the master m3u8 quality probe.
static RESOLUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"RESOLUTION=\d+x(\d+)").unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// The anime-id resolution — the port of `getMalId`'s result: an ARM
/// mapping or an `AniList` bridge hit, with the absolute episode ARM
/// reports (or the requested one).
struct AnimeId {
    /// The `MyAnimeList` id, when known.
    mal_id: Option<u64>,
    /// The `AniList` id, when known.
    ani_id: Option<u64>,
    /// The episode number the stream URL keys on.
    episode: u32,
}

impl AnimeId {
    /// The id the stream URL uses (`mal` when present, else `ani`) and
    /// its prefix.
    fn key(&self) -> Option<(u64, &'static str)> {
        self.mal_id
            .map(|id| (id, "mal"))
            .or_else(|| self.ani_id.map(|id| (id, "ani")))
    }
}

/// One decrypted megaplay source sweep — `extractHLS`'s result.
struct MegaplaySource {
    /// The stream URL (`sources.file` or the decrypted `enc` blob).
    url: String,
    /// The quality label from the m3u8 probe (`1080p` fallback).
    quality: String,
    /// The caption tracks.
    subtitles: Vec<NuvioSubtitle>,
}

/// The `AnikotoTV` provider.
pub struct AnikotoTV {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The round-robin mobile-UA cursor (upstream picks randomly).
    ua_cursor: AtomicUsize,
}

impl AnikotoTV {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            ua_cursor: AtomicUsize::new(0),
        }
    }

    /// The next mobile UA — the port of `getHeaders`'s random pick,
    /// as a round-robin (see the module docs).
    fn next_ua(&self) -> &'static str {
        let index = self.ua_cursor.fetch_add(1, Ordering::Relaxed);
        MOBILE_UAS[index % MOBILE_UAS.len()]
    }

    /// The megaplay sub/dub sweep — the port of the scraper's
    /// `getStreams` loop: one `Vidstream` server, both audio types,
    /// each with its own [`extract_hls`] run.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        anime: &AnimeId,
        display: &Display,
    ) -> Vec<NuvioStream> {
        let Some((id, prefix)) = anime.key() else {
            return Vec::new();
        };
        let mut streams = Vec::new();
        for kind in ["sub", "dub"] {
            let stream_url = format!("{MEGAPLAY}/stream/{prefix}/{id}/{}/{kind}", anime.episode);
            let Ok(stream_url) = Url::parse(&stream_url) else {
                continue;
            };
            let Some(source) = extract_hls(ctx, self.next_ua(), &stream_url).await else {
                continue;
            };

            // The card display strings, verbatim from the scraper.
            let quality = source.quality.to_lowercase();
            let (flag, audio_label, audio_name) = if kind == "sub" {
                ("🇯🇵 Japanese", "SUB", "Japanese (SUB)")
            } else {
                ("🇺🇲 English", "DUB", "English (DUB)")
            };
            // Upstream's ternary labels both branches HLS-family:
            // "HLS" only when the URL literally contains ".m3u8".
            let transport = if source.url.contains(".m3u8") {
                "HLS"
            } else {
                "M3U8"
            };
            let title = format!(
                "🎦 {display_title} - ({year})\n🎬 S{season:02}E{episode:02} - {ep_title}\n✨ {quality} | {flag} • 🗣️ {audio_label}\n🔗 {SERVER_ID} | ⏳ {duration} | ⚡ {transport}",
                display_title = display.title,
                year = display.year,
                season = display.season,
                episode = display.episode,
                ep_title = display.episode_title,
                duration = display.duration,
            );
            streams.push(
                NuvioStream::new(source.url)
                    .with_name(format!("{LABEL} | {quality} | {audio_name}"))
                    .with_title(title.clone())
                    .with_size(title)
                    .with_header("Referer", format!("{MEGAPLAY}/"))
                    .with_header("Origin", MEGAPLAY),
            );
            for subtitle in source.subtitles {
                if let Some(stream) = streams.last_mut() {
                    stream.subtitles.push(subtitle);
                }
            }
        }
        streams
    }
}

/// The display facts the scraper's `getTMDBDetails` would have
/// supplied — the title and year from the wrapper's resolution, plus
/// the scraper's own failure defaults for the episode title and
/// duration (see the module docs).
struct Display {
    /// The series title.
    title: String,
    /// The release year (the scraper's fallback year).
    year: i64,
    /// The season.
    season: u32,
    /// The episode.
    episode: u32,
    /// The episode title (`Episode {n}`).
    episode_title: String,
    /// The duration (`24 min`).
    duration: &'static str,
}

#[async_trait]
impl Source for AnikotoTV {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // Anime-only: the wrapper bails without season/episode.
        let (Some(season), episode) = (media.season, media.episode.unwrap_or(1)) else {
            return Err(SourceError::NotFound);
        };

        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let title = format!("{} {}", name, media.format_season_and_episode());

        let display = Display {
            title: name,
            year: i64::from(year.unwrap_or(2026)),
            season,
            episode,
            episode_title: format!("Episode {episode}"),
            duration: "24 min",
        };

        // The scraper races the 25 s deadline; a miss answers the
        // empty sweep, which the wrapper turns into zero streams.
        let sweep = async {
            let Some(anime) =
                resolve_anime_id(ctx, self.next_ua(), tmdb_id, &display, season, episode).await
            else {
                return Vec::new();
            };
            self.sweep(ctx, &anime, &display).await
        };
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }

        let country_codes = vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En];
        Ok(build_stream_results(&BuildParams {
            streams: &raw,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &country_codes,
            ttl: TTL,
        }))
    }
}

/// Resolve the anime id — the port of `getMalId`: the ARM bridge
/// first, then the `AniList` GraphQL bridge on the season-adjusted
/// title (retrying the plain title), with the requested episode as
/// the fallback numbering.
async fn resolve_anime_id(
    ctx: &ResolveCtx<'_>,
    ua: &str,
    tmdb_id: u64,
    display: &Display,
    season: u32,
    episode: u32,
) -> Option<AnimeId> {
    // The ARM bridge: `{ mal | mal_id, anilist | ani_id, episode }`.
    let arm_url = format!("{ARM_API}?id={tmdb_id}&s={season}&e={episode}");
    if let Ok(url) = Url::parse(&arm_url)
        && let Ok(response) = ctx
            .fetcher
            .request(FetchRequest::get(url).with_timeout(API_TIMEOUT))
            .await
        && response.is_success()
        && let Ok(data) = response.json::<Value>()
    {
        let mal_id = data
            .get("mal")
            .or_else(|| data.get("mal_id"))
            .and_then(Value::as_u64);
        let ani_id = data
            .get("anilist")
            .or_else(|| data.get("ani_id"))
            .and_then(Value::as_u64);
        if mal_id.is_some() || ani_id.is_some() {
            let arm_episode = data
                .get("episode")
                .and_then(Value::as_u64)
                .and_then(|episode| u32::try_from(episode).ok());
            return Some(AnimeId {
                mal_id,
                ani_id,
                episode: arm_episode.unwrap_or(episode),
            });
        }
    }

    // The AniList bridge on the season-adjusted title — the season
    // name lookup is cut, so later seasons build `{title} Season {n}`
    // (the upstream null branch).
    let plain = display.title.clone();
    let adjusted = if season > 1 {
        format!("{} Season {season}", display.title)
    } else {
        plain.clone()
    };
    let mut hit = anilist_bridge(ctx, ua, &adjusted).await;
    if hit.as_ref().is_none_or(|hit| hit.mal_id.is_none()) && adjusted != plain {
        hit = anilist_bridge(ctx, ua, &plain).await;
    }
    // The fallback numbering is the requested episode (the
    // absolute-episode refinement chain is cut — see the module docs).
    hit.map(|mut hit| {
        hit.episode = episode;
        hit
    })
}

/// One `AniList` GraphQL `Media` lookup — the port of `aniListBridge`.
async fn anilist_bridge(ctx: &ResolveCtx<'_>, ua: &str, title: &str) -> Option<AnimeId> {
    let body =
        serde_json::json!({ "query": ANILIST_QUERY, "variables": { "search": title } }).to_string();
    let url = Url::parse(ANILIST_GQL).ok()?;
    let request = FetchRequest::post(url, body)
        .with_header("User-Agent", ua)
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_timeout(API_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let data: Value = response.json().ok()?;
    let media = data.pointer("/data/Media")?;
    Some(AnimeId {
        mal_id: media.get("idMal").and_then(Value::as_u64),
        ani_id: media.get("id").and_then(Value::as_u64),
        episode: 0,
    })
}

/// One megaplay stream resolution — the port of `extractHLS`: the
/// `/stream/{…}/{sub|dub}` page (following one `<iframe>` hop for the
/// `data-id`), the `getSources` JSON (with the encrypted `enc` shape
/// decrypted), the caption tracks, and the master m3u8 quality probe.
async fn extract_hls(ctx: &ResolveCtx<'_>, ua: &str, stream_url: &Url) -> Option<MegaplaySource> {
    // The stream page — `data-id`, or one iframe hop to find it.
    let page_request = FetchRequest::get(stream_url.clone())
        .with_header("User-Agent", ua)
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("Referer", format!("{MEGAPLAY}/"))
        .with_timeout(API_TIMEOUT);
    let page = ctx.fetcher.request(page_request).await.ok()?;
    if !page.is_success() {
        return None;
    }
    let mut body = page.body;
    let data_id = if let Some(id) = first_capture(&DATA_ID, &body) {
        id
    } else {
        // The iframe fallback: resolve the nested player page.
        let src = first_capture(&IFRAME_SRC, &body)?;
        let nested = if src.starts_with("http") {
            Url::parse(&src).ok()?
        } else {
            Url::parse(&format!("{MEGAPLAY}{src}")).ok()?
        };
        let nested_request = FetchRequest::get(nested)
            .with_header("User-Agent", ua)
            .with_header("Accept-Language", "en-US,en;q=0.9")
            .with_header("Referer", format!("{MEGAPLAY}/"))
            .with_timeout(API_TIMEOUT);
        let nested_page = ctx.fetcher.request(nested_request).await.ok()?;
        if !nested_page.is_success() {
            return None;
        }
        body = nested_page.body;
        first_capture(&DATA_ID, &body)?
    };

    // getSources — the 2026-09 encrypted shape decrypts through the
    // megaplay key (the upstream fetch shim, folded here).
    let sources_url = Url::parse(&format!("{MEGAPLAY}/stream/getSources?id={data_id}")).ok()?;
    let sources_request = FetchRequest::get(sources_url)
        .with_header("User-Agent", ua)
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_header("Referer", stream_url.as_str())
        .with_timeout(API_TIMEOUT);
    let sources_response = ctx.fetcher.request(sources_request).await.ok()?;
    if !sources_response.is_success() {
        return None;
    }
    let data: Value = sources_response.json().ok()?;
    let file = data
        .pointer("/sources/file")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            data.get("enc")
                .and_then(Value::as_str)
                .and_then(decrypt_megaplay_enc)
        })?;
    if file.is_empty() {
        return None;
    }

    // The caption tracks (`kind: captions | subtitles`).
    let mut subtitles = Vec::new();
    if let Some(tracks) = data.get("tracks").and_then(Value::as_array) {
        for track in tracks {
            if track.get("kind").and_then(Value::as_str) != Some("captions")
                && track.get("kind").and_then(Value::as_str) != Some("subtitles")
            {
                continue;
            }
            let url = track.get("file").and_then(Value::as_str);
            let id = track
                .get("label")
                .or_else(|| track.get("file"))
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            if let Some(url) = url.filter(|url| !url.is_empty()) {
                subtitles.push(NuvioSubtitle {
                    id: Some(id.to_string()),
                    url: Some(url.to_string()),
                    language: Some("eng".to_string()),
                    ..NuvioSubtitle::default()
                });
            }
        }
    }

    // The master m3u8 quality probe (default `1080p`).
    let mut quality = "1080p".to_string();
    if let Ok(probe_url) = Url::parse(&file) {
        let probe = FetchRequest::get(probe_url)
            .with_header("Referer", format!("{MEGAPLAY}/"))
            .with_timeout(API_TIMEOUT);
        if let Ok(response) = ctx.fetcher.request(probe).await
            && response.is_success()
            && let Some(height) = first_capture(&RESOLUTION, &response.body)
        {
            quality = format!("{height}p");
        }
    }

    Some(MegaplaySource {
        url: file,
        quality,
        subtitles,
    })
}

/// The first capture group of a regex match.
fn first_capture(regex: &Regex, haystack: &str) -> Option<String> {
    regex
        .captures(haystack)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())
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

    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};

    use super::*;

    /// A fetcher serving canned `(status, body)` keyed by URL path (or
    /// `path?query`) — later registrations REPLACE earlier ones,
    /// `page_sequence` serves entries in order — recording every
    /// request. Query-bearing lookups fall back to the bare path, so
    /// TMDB requests (`?api_key=…`) are scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<(u16, String)>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `status`/`body` (replacing any earlier
        /// registration).
        fn page(self, key: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.into(), vec![(status, body.into())]);
            self
        }

        /// Serve the same key a sequence of bodies, popping in order.
        fn page_sequence(self, key: impl Into<String>, entries: Vec<(u16, String)>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.into(), entries);
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

        /// How many requests hit a path starting with `prefix`.
        fn hits_starting_with(&self, prefix: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request.url.path().starts_with(prefix))
                .count()
        }

        /// A header of the first request whose URL contains `needle`.
        fn sent_header(&self, needle: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| request.url.as_str().contains(needle))
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
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
            let entry = entry.or_else(|| {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(request.url.path()).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            });
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

    /// The fixture media: Frieren S01E02 (MAL 52991, data-id 8817).
    const TMDB_ID: u64 = 209_867;
    /// The ARM mapping for the fixture.
    const ARM_BODY: &str = r#"{"mal":52991,"anilist":154587,"episode":2}"#;
    /// The ARM page (query-keyed).
    const ARM: &str = "/api/v2/tmdb?id=209867&s=1&e=2";
    /// The megaplay stream pages.
    const SUB_PAGE: &str = "/stream/mal/52991/2/sub";
    const DUB_PAGE: &str = "/stream/mal/52991/2/dub";
    /// The megaplay stream page body.
    const MEGAPLAY_PAGE: &str = r#"<html><div data-id="8817"><video></video></div></html>"#;
    /// The getSources page (query-keyed — both sweeps share the
    /// data-id).
    const GET_SOURCES: &str = "/stream/getSources?id=8817";
    /// The encrypted sub sources — ground truth generated with Node's
    /// `crypto` using the upstream key/IV (the `nuvio::megaplay`
    /// ground-truth recipe).
    const ENC_SUB: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfU_mcdDzyMudFPrC1OQLhJpZ5ycCp9IOePp3IulIXsS_dmIU7WY-6FC_RFX82wF6a4";
    /// The encrypted dub sources.
    const ENC_DUB: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfV9FstHnQBKGBWYluF-VtF0PD8jt7V9hQnCQYizN6yLQpwJ8f2ORE3oLZAyYlo4swU";

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> AnikotoTV {
        AnikotoTV::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "anikototv");
        assert_eq!(info.label, "AnikotoTV");
        assert_eq!(info.content_types, vec![MediaType::Series]);
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://anikototv.com/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    /// The sub/dub sweep is one linear script over the queue-backed
    /// mock; splitting it would hide the ordering.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn sweeps_sub_and_dub_through_the_encrypted_sources() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page(ARM, 200, ARM_BODY)
                .page(SUB_PAGE, 200, MEGAPLAY_PAGE)
                .page(DUB_PAGE, 200, MEGAPLAY_PAGE)
                .page_sequence(
                    GET_SOURCES,
                    vec![
                        format!(
                            r#"{{"tracks":[{{"kind":"captions","label":"English","file":"https://megap.akirax.buzz/subs/frieren.vtt"}}],"enc":"{ENC_SUB}"}}"#
                        ),
                        format!(r#"{{"tracks":[],"enc":"{ENC_DUB}"}}"#),
                    ]
                    .into_iter()
                    .map(|body| (200, body))
                    .collect(),
                )
                .page(
                    "/hls/frieren/sub/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nsub_1080.m3u8\n",
                )
                .page(
                    "/hls/frieren/dub/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=3840x2160\nsub_2160.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        // Sub then dub, in sweep order.
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].url.as_str(),
            "https://megap.akirax.buzz/hls/frieren/sub/master.m3u8?token=sub123"
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://megap.akirax.buzz/hls/frieren/dub/master.m3u8?token=dub456"
        );
        // The m3u8 probe set the quality → resolution.
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(streams[1].meta.resolution, Some(2160));
        // The megaplay hotlink headers ride every card.
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some("https://megaplay.buzz")
        );
        // The sub card carries the caption track; the dub card's empty
        // tracks answer none.
        assert_eq!(streams[0].meta.subtitles.len(), 1);
        assert_eq!(
            streams[0].meta.subtitles[0].url.as_str(),
            "https://megap.akirax.buzz/subs/frieren.vtt"
        );
        assert!(streams[1].meta.subtitles.is_empty());
        // The audio markers in the card text drive the language flags.
        assert!(streams[0].meta.languages.contains(&CountryCode::Ja));
        assert!(streams[1].meta.languages.contains(&CountryCode::En));
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("anikototv"))
        );
        assert_eq!(streams[0].ttl, TTL);
        // The label carries the wrapper title and the scraper's rich
        // description.
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Frieren: Beyond Journey's End S01E02"))
        );
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("🇯🇵 Japanese • 🗣️ SUB"))
        );
        // The getSources request carried the XHR marker and the
        // stream-page referer.
        assert_eq!(
            mock.sent_header("getSources?id=8817", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            mock.sent_header("getSources?id=8817", "Referer").as_deref(),
            Some("https://megaplay.buzz/stream/mal/52991/2/sub")
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_legacy_plaintext_sources_shape_still_works() -> Result<(), SourceError> {
        // The sub sweep answers the legacy `{sources: {file}}` shape;
        // the dub page without a data-id (and no iframe) is dropped.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page(ARM, 200, ARM_BODY)
                .page(SUB_PAGE, 200, MEGAPLAY_PAGE)
                .page(DUB_PAGE, 200, r"<html>no data-id here</html>")
                .page(
                    GET_SOURCES,
                    200,
                    r#"{"sources":{"file":"https://megap.akirax.buzz/hls/legacy/master.m3u8"},"tracks":[]}"#,
                )
                .page(
                    "/hls/legacy/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nv.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://megap.akirax.buzz/hls/legacy/master.m3u8"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_iframe_page_hop_finds_the_data_id() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page(ARM, 200, ARM_BODY)
                .page(
                    SUB_PAGE,
                    200,
                    r#"<html><iframe src="/embed/player/8817"></iframe></html>"#,
                )
                .page(DUB_PAGE, 200, r"<html>no data-id</html>")
                .page("/embed/player/8817", 200, MEGAPLAY_PAGE)
                .page(
                    GET_SOURCES,
                    200,
                    format!(r#"{{"tracks":[],"enc":"{ENC_SUB}"}}"#),
                )
                .page(
                    "/hls/frieren/sub/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nv.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(mock.hits("/embed/player/8817"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn arm_miss_falls_back_to_the_anilist_bridge() -> Result<(), SourceError> {
        // ARM 404s; the AniList bridge answers on the plain title
        // (season 1 → no adjustment) and the dub page is unserved.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page(ARM, 404, "{}")
                .page(
                    "/",
                    200,
                    r#"{"data":{"Media":{"id":154587,"idMal":52991}}}"#,
                )
                .page(SUB_PAGE, 200, MEGAPLAY_PAGE)
                .page(
                    GET_SOURCES,
                    200,
                    format!(r#"{{"tracks":[],"enc":"{ENC_SUB}"}}"#),
                )
                .page(
                    "/hls/frieren/sub/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nv.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        // The fallback numbering: episode 2 (the requested one).
        assert!(
            mock.requests()
                .iter()
                .any(|request| request.url.as_str()
                    == "https://megaplay.buzz/stream/mal/52991/2/sub")
        );
        Ok(())
    }

    #[tokio::test]
    async fn without_a_season_the_source_bails() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::tmdb(TMDB_ID, MediaType::Series))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn no_anime_mapping_anywhere_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"House of the Dragon","first_air_date":"2022-08-21"}"#,
                )
                .page(ARM, 200, r#"{"mal":null,"anilist":null}"#),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert_eq!(mock.hits("/stream"), 0);
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(ARM, 200, ARM_BODY)
                .page(SUB_PAGE, 200, MEGAPLAY_PAGE)
                .page(
                    GET_SOURCES,
                    200,
                    format!(r#"{{"tracks":[],"enc":"{ENC_SUB}"}}"#),
                )
                .page(
                    "/hls/frieren/sub/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nv.m3u8\n",
                ),
        );
        let provider = provider(&mock);
        let media = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: Some("tt22354494".to_string()),
            name: "Frieren: Beyond Journey's End".to_string(),
            year: Some(2023),
            season: Some(1),
            episode: Some(2),
        };
        let ctx = ResolveCtx {
            fetcher: mock.as_ref() as &dyn Fetcher,
            media: Some(media),
            source_id: None,
            referer: None,
        };

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(mock.hits_starting_with("/3/tv"), 0);
        Ok(())
    }
}
