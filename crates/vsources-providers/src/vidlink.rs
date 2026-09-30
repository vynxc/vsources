//! `VidLink`: the `vidlink.pro` TMDB-keyed API.
//!
//! Ports `src/source/VidLink.js` + its Nuvio scraper
//! `src/nuvio/vidlink.cjs` (readable upstream — the flow below is
//! verbatim from it).
//!
//! Flow:
//!
//! 1. Resolve the TMDB id and name/year (context media or
//!    [`TmdbClient`]) — the scraper's `getTmdbInfo`.
//! 2. `GET https://enc-dec.app/api/enc-vidlink?text={tmdbId}` →
//!    `{result}` — the encrypted id the API routes on.
//! 3. `GET https://vidlink.pro/api/b/{movie|tv}/{enc}[/{s}/{e}]` with
//!    the scraper's `VIDLINK_HEADERS` (`Referer`/`Origin:
//!    vidlink.pro`, a Chrome 147 UA).
//! 4. The response fans into cards by shape — `stream.qualities` (a
//!    key → `{url}` map), a `stream.playlist` (fetched and parsed for
//!    its `#EXT-X-STREAM-INF` variants), a bare `url`, `streams` /
//!    `links` arrays, or a recursive URL walk that skips
//!    subtitle-shaped keys and values.
//! 5. Playlist cards resolve their variants (bandwidth/resolution
//!    lines, relative URIs joined against the playlist); a fetch or
//!    parse failure falls back to one `Auto` card pointing at the
//!    playlist itself.
//! 6. Everything sorts by the scraper's quality ladder (`4K` first)
//!    and then takes the JS wrapper's post-pass: http-only, dedup by
//!    URL, `.m3u8`/`/playlist` → HLS and `.mp4` → MP4, the
//!    `{name} (VidLink {quality})` label, and `[multi]` languages.
//!
//! Cuts for the library port:
//!
//! - `meta.sourceType: 'WebDL'` has no `StreamMeta` field — cut.
//! - `meta.title` has no `StreamMeta` field — the card title rides
//!   [`Stream::label`].
//! - The JS race at 25 s becomes a [`with_deadline`] wrap.
//! - Upstream sorts stable (JS `Array.sort`); this port's sort is
//!   stable by construction (`sort_by_key` over the ladder rank,
//!   preserving insertion order within a rank).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::with_deadline;

/// The id-encryption service (upstream `ENC_DEC_API`).
const ENC_DEC_API: &str = "https://enc-dec.app/api/enc-vidlink";
/// The stream API (upstream `VIDLINK_API`).
const VIDLINK_API: &str = "https://vidlink.pro/api/b";
/// The API's `Referer`/`Origin`.
const VIDLINK_ORIGIN: &str = "https://vidlink.pro";
/// The scraper's browser UA (Chrome 147).
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
/// Upstream `this.ttl` — 5 min.
const TTL: Duration = Duration::from_mins(5);
/// The JS race cap around the scraper call.
const DEADLINE: Duration = Duration::from_secs(25);
/// The per-request timeout (the scraper's fetch default).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The `VidLink` provider.
pub struct VidLink {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl VidLink {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "vidlink2".to_string(),
                label: "VidLink".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse("https://vidlink.pro").ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// One scraper call — the encrypted id, the API answer, the card
    /// fan, and the playlist resolution.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        name: &str,
        year: Option<u16>,
    ) -> Vec<Stream> {
        let tmdb_id = match &media.id {
            MediaId::Tmdb(id) => *id,
            // The API routes on TMDB ids only; IMDb-keyed references
            // were resolved to one before the sweep.
            MediaId::Imdb(_) => match ctx.media.as_ref().and_then(|m| m.tmdb_id) {
                Some(id) => id,
                None => return Vec::new(),
            },
        };
        let Some(encrypted) = encrypt_tmdb_id(ctx, tmdb_id).await else {
            return Vec::new();
        };
        let Ok(api_url) = vidlink_api_url(media, &encrypted) else {
            return Vec::new();
        };
        let request = FetchRequest::get(api_url)
            .with_header("User-Agent", USER_AGENT)
            .with_header("Referer", format!("{VIDLINK_ORIGIN}/"))
            .with_header("Origin", VIDLINK_ORIGIN)
            .with_header("Connection", "keep-alive")
            .with_header("Accept", "application/json,*/*")
            .with_timeout(REQUEST_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(data) = serde_json::from_str::<Value>(&response.body) else {
            return Vec::new();
        };

        let title = stream_title(name, year, media);
        let cards = process_response(&data, &title);
        // The playlist cards resolve into their variants; the direct
        // cards pass through.
        let mut resolved: Vec<Card> = Vec::new();
        for card in cards {
            if card.is_playlist {
                resolved.extend(fetch_and_parse_m3u8(ctx, &card.url, &title).await);
            } else {
                resolved.push(card);
            }
        }
        resolved.sort_by_key(|card| std::cmp::Reverse(quality_rank(&card.quality)));

        // The JS wrapper's post-pass: http-only, dedup, format
        // detection, label, and metadata.
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for card in resolved {
            if !card.url.starts_with("http") {
                continue;
            }
            let Ok(url) = Url::parse(&card.url) else {
                continue;
            };
            if !seen.insert(card.url.clone()) {
                continue;
            }
            let height = parse_height(&card.quality);
            let quality_label = if card.quality.is_empty() {
                height.map_or_else(|| "HLS".to_string(), |h| format!("{h}p"))
            } else {
                card.quality.clone()
            };
            let format = if card.url.contains(".m3u8") || card.url.contains("/playlist") {
                Format::Hls
            } else if card.url.contains(".mp4") {
                Format::Mp4
            } else {
                Format::Unknown
            };
            out.push(Stream {
                url,
                format,
                label: Some(format!("{title} (VidLink {quality_label})")),
                meta: vsources_core::types::StreamMeta {
                    languages: vec![CountryCode::Multi],
                    resolution: height,
                    source_id: Some(self.info.id.clone()),
                    source_label: Some(self.info.label.clone()),
                    ..vsources_core::types::StreamMeta::default()
                },
                ttl: TTL,
                is_external: false,
                behavior_hints: BTreeMap::new(),
            });
        }
        out
    }
}

#[async_trait]
impl Source for VidLink {
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

/// One intermediate card between the scraper and the wrapper.
struct Card {
    /// The stream (or playlist) URL.
    url: String,
    /// The quality label (`4K`, `1080p`, `Auto`, `Unknown`).
    quality: String,
    /// Whether this is a playlist card needing resolution.
    is_playlist: bool,
}

/// `GET enc-dec.app/api/enc-vidlink?text={tmdbId}` → `{result}` —
/// `None` on any failure (the scraper throws and the wrapper answers
/// zero).
async fn encrypt_tmdb_id(ctx: &ResolveCtx<'_>, tmdb_id: u64) -> Option<String> {
    let url = Url::parse(&format!("{ENC_DEC_API}?text={tmdb_id}")).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json,*/*")
        .with_timeout(REQUEST_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let value: Value = serde_json::from_str(&response.body).ok()?;
    let result = value.get("result")?.as_str()?.to_string();
    (!result.is_empty()).then_some(result)
}

/// The API URL — `/b/movie/{enc}` or `/b/tv/{enc}/{s}/{e}`.
fn vidlink_api_url(media: &MediaRef, encrypted: &str) -> Result<Url, url::ParseError> {
    let base = if media.season.is_some() {
        format!(
            "{VIDLINK_API}/tv/{encrypted}/{}/{}",
            media.season.unwrap_or(1),
            media.episode.unwrap_or(1)
        )
    } else {
        format!("{VIDLINK_API}/movie/{encrypted}")
    };
    Url::parse(&base)
}

/// The response fan — ports `processVidlinkResponse` shape by shape.
fn process_response(data: &Value, _title: &str) -> Vec<Card> {
    let mut cards = Vec::new();
    let stream = data.get("stream");

    if let Some(stream) = stream {
        // `stream.qualities` — a key → {url} map.
        if let Some(qualities) = stream.get("qualities").and_then(Value::as_object) {
            for (key, entry) in qualities {
                if let Some(url) = entry.get("url").and_then(Value::as_str) {
                    let quality = extract_quality(&serde_json::json!({ "quality": key }));
                    cards.push(Card {
                        url: url.to_string(),
                        quality,
                        is_playlist: false,
                    });
                }
            }
            // A playlist rides alongside the qualities.
            if let Some(playlist) = stream.get("playlist").and_then(Value::as_str) {
                cards.push(Card {
                    url: playlist.to_string(),
                    quality: "Auto".to_string(),
                    is_playlist: true,
                });
            }
            return cards;
        }
        // Playlist-only response.
        if let Some(playlist) = stream.get("playlist").and_then(Value::as_str) {
            cards.push(Card {
                url: playlist.to_string(),
                quality: "Auto".to_string(),
                is_playlist: true,
            });
            return cards;
        }
    }

    // A bare `url`.
    if let Some(url) = data.get("url").and_then(Value::as_str) {
        cards.push(Card {
            url: url.to_string(),
            quality: extract_quality(data),
            is_playlist: false,
        });
        return cards;
    }

    // `streams` / `links` arrays.
    for key in ["streams", "links"] {
        if let Some(entries) = data.get(key).and_then(Value::as_array) {
            for entry in entries {
                if let Some(url) = entry.get("url").and_then(Value::as_str) {
                    cards.push(Card {
                        url: url.to_string(),
                        quality: extract_quality(entry),
                        is_playlist: false,
                    });
                }
            }
            if !cards.is_empty() {
                return cards;
            }
        }
    }

    // The recursive URL walk — skipping subtitle-shaped keys/values.
    find_urls(data, &mut cards);
    cards
}

/// The recursive `findUrls` — every http/m3u8 string value becomes a
/// card named after its key, descending objects unless they look like
/// subtitle containers.
fn find_urls(value: &Value, cards: &mut Vec<Card>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, entry) in object {
        let key_lower = key.to_lowercase();
        if key_lower.contains("subtitle") || key_lower.contains("caption") {
            continue;
        }
        match entry {
            Value::String(text) => {
                let subtitleish = text.contains(".srt")
                    || text.contains(".vtt")
                    || text.contains("subtitle")
                    || text.contains("captions");
                if (text.starts_with("http") || text.contains(".m3u8")) && !subtitleish {
                    cards.push(Card {
                        url: text.clone(),
                        quality: extract_quality(&serde_json::json!({ key: text })),
                        is_playlist: false,
                    });
                }
            }
            Value::Object(_) | Value::Array(_) => find_urls(entry, cards),
            _ => {}
        }
    }
}

/// The quality of a response fragment — ports `extractQuality`: the
/// `quality`/`resolution`/`label`/`name` fields, substring matches,
/// then the bare-number threshold ladder.
fn extract_quality(fragment: &Value) -> String {
    for field in ["quality", "resolution", "label", "name"] {
        let Some(value) = fragment.get(field) else {
            continue;
        };
        let Some(text) = value.as_str() else {
            continue;
        };
        let lower = text.to_lowercase();
        if lower.contains("2160") || lower.contains("4k") {
            return "4K".to_string();
        }
        if lower.contains("1440") || lower.contains("2k") {
            return "1440p".to_string();
        }
        if lower.contains("1080") || lower.contains("fhd") {
            return "1080p".to_string();
        }
        if lower.contains("720") || lower.contains("hd") {
            return "720p".to_string();
        }
        if lower.contains("480") || lower.contains("sd") {
            return "480p".to_string();
        }
        if lower.contains("360") {
            return "360p".to_string();
        }
        if lower.contains("240") {
            return "240p".to_string();
        }
    }
    "Unknown".to_string()
}

/// Fetch a playlist and parse its variants — ports
/// `fetchAndParseM3U8`. A fetch or parse failure answers one `Auto`
/// card pointing at the playlist itself (the JS fallback).
async fn fetch_and_parse_m3u8(ctx: &ResolveCtx<'_>, playlist: &str, _title: &str) -> Vec<Card> {
    let Ok(url) = Url::parse(playlist) else {
        return Vec::new();
    };
    let request = FetchRequest::get(url.clone())
        .with_header("User-Agent", USER_AGENT)
        .with_header("Referer", format!("{VIDLINK_ORIGIN}/"))
        .with_header("Origin", VIDLINK_ORIGIN)
        .with_header("Connection", "keep-alive")
        .with_timeout(REQUEST_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return auto_card(playlist);
    };
    if !response.is_success() || !response.body.starts_with("#EXTM3U") {
        return auto_card(playlist);
    }

    let variants = parse_m3u8(&response.body, &url);
    if variants.is_empty() {
        return auto_card(playlist);
    }
    variants
        .into_iter()
        .map(|variant| Card {
            url: variant.url,
            quality: quality_from_resolution(&variant.resolution),
            is_playlist: false,
        })
        .collect()
}

/// The `Auto` fallback card.
fn auto_card(playlist: &str) -> Vec<Card> {
    vec![Card {
        url: playlist.to_string(),
        quality: "Auto".to_string(),
        is_playlist: false,
    }]
}

/// One parsed `#EXT-X-STREAM-INF` variant.
struct Variant {
    /// The variant URL (resolved against the playlist).
    url: String,
    /// The `RESOLUTION=WxH` line.
    resolution: String,
}

/// Parse the `#EXT-X-STREAM-INF` blocks of a master playlist — ports
/// `parseM3U8` (bandwidth tracked for sort stability upstream, unused
/// here since the ladder is label-based).
fn parse_m3u8(body: &str, base: &Url) -> Vec<Variant> {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut variants = Vec::new();
    // `Some` = a STREAM-INF is pending; the inner string is its
    // `RESOLUTION` (empty when the line lacks one — upstream's
    // `resolution: null`).
    let mut pending: Option<String> = None;
    for line in lines {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(
                attributes
                    .split(',')
                    .find_map(|pair| pair.strip_prefix("RESOLUTION="))
                    .unwrap_or_default()
                    .to_string(),
            );
        } else if !line.starts_with('#') && pending.is_some() {
            let url = line
                .parse::<Url>()
                .or_else(|_| base.join(line))
                .map_or_else(|_| line.to_string(), |url| url.to_string());
            variants.push(Variant {
                url,
                resolution: pending.clone().unwrap_or_default(),
            });
            pending = None;
        }
    }
    variants
}

/// The quality label of an `WxH` resolution — ports
/// `getQualityFromResolution` (`Auto` when absent).
fn quality_from_resolution(resolution: &str) -> String {
    let Some(height) = resolution
        .split('x')
        .nth(1)
        .and_then(|h| h.parse::<u32>().ok())
    else {
        return "Auto".to_string();
    };
    if height >= 2160 {
        "4K"
    } else if height >= 1440 {
        "1440p"
    } else if height >= 1080 {
        "1080p"
    } else if height >= 720 {
        "720p"
    } else if height >= 480 {
        "480p"
    } else if height >= 360 {
        "360p"
    } else {
        "240p"
    }
    .to_string()
}

/// The scraper's sort ladder — `4K` first, `Unknown` last.
fn quality_rank(quality: &str) -> i32 {
    match quality {
        "4K" => 5,
        "1440p" => 4,
        "1080p" => 3,
        "720p" => 2,
        "480p" => 1,
        "360p" => 0,
        "240p" => -1,
        "Auto" => -2,
        _ => -3,
    }
}

/// The JS wrapper's `parseHeight` — substring matches, then any
/// 3-4-digit number.
fn parse_height(quality: &str) -> Option<u16> {
    let lower = quality.to_lowercase();
    if lower.contains("4k") || lower.contains("2160") {
        return Some(2160);
    }
    if lower.contains("1440") {
        return Some(1440);
    }
    let digits: String = lower.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 3 || digits.len() == 4 {
        digits.parse::<u16>().ok()
    } else {
        None
    }
}

/// `Name (year)` / `Name S01E02` — the scraper's `createStreamTitle`.
fn stream_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| name.to_string(), |year| format!("{name} ({year})"))
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

/// Name and year, preferring pre-resolved context media — ports the
/// scraper's `getTmdbInfo` (title/year only).
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
    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};

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
                .filter(|request| request_key(request) == key)
                .find_map(|request| {
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
                    headers: BTreeMap::new(),
                    body: scripted.body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A provider over the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> VidLink {
        VidLink::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
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

    /// The dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::movie(MediaId::Tmdb(438_631))
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        // Upstream `this.id` is `vidlink2` — kept verbatim.
        assert_eq!(info.id, "vidlink2");
        assert_eq!(info.label, "VidLink");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://vidlink.pro/")
        );
        assert_eq!(info.priority, 0);
    }

    #[tokio::test]
    async fn resolves_qualities_with_a_parsed_playlist() -> Result<(), SourceError> {
        let api_payload = serde_json::json!({
            "stream": {
                "qualities": {
                    "1080": { "url": "https://bcdn.hakunaymatata.com/dune1080.mp4" },
                    "720": { "url": "https://bcdn.hakunaymatata.com/dune720.mp4" }
                },
                "playlist": "https://bcdn.hakunaymatata.com/master.m3u8"
            }
        });
        let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160\n\
v2160/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\n\
v720/index.m3u8\n";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve(
                    "vidlink.pro/api/b/movie/enc123",
                    200,
                    api_payload.to_string(),
                )
                .serve("bcdn.hakunaymatata.com/master.m3u8", 200, master),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the movie fixture must resolve: {e}"));

        // 2 direct qualities + 2 playlist variants; the 4K variant
        // sorts first (the ladder), then 1080, 720.
        assert_eq!(streams.len(), 4);
        let first = &streams[0];
        assert_eq!(
            first.url.as_str(),
            "https://bcdn.hakunaymatata.com/v2160/index.m3u8"
        );
        assert_eq!(first.format, Format::Hls);
        assert_eq!(first.meta.resolution, Some(2160));
        assert_eq!(first.meta.source_id.as_deref(), Some("vidlink2"));
        assert_eq!(first.meta.source_label.as_deref(), Some("VidLink"));
        assert_eq!(first.ttl, TTL);
        assert_eq!(first.meta.languages, vec![CountryCode::Multi]);
        assert_eq!(first.label.as_deref(), Some("Dune (2021) (VidLink 4K)"));
        // The playlist request carried the hotlink headers.
        assert_eq!(
            fetcher
                .sent_header("bcdn.hakunaymatata.com/master.m3u8", "Referer")
                .as_deref(),
            Some("https://vidlink.pro/")
        );
        assert_eq!(
            fetcher
                .sent_header("vidlink.pro/api/b/movie/enc123", "Origin")
                .as_deref(),
            Some("https://vidlink.pro")
        );
        // The enc service saw the TMDB id.
        let enc_query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.host_str() == Some("enc-dec.app"))
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(enc_query.contains("text=438631"), "{enc_query}");
        Ok(())
    }

    #[tokio::test]
    async fn mp4_qualities_directly_resolve() -> Result<(), SourceError> {
        let api_payload = serde_json::json!({
            "stream": {
                "qualities": {
                    "1080": { "url": "https://bcdn.hakunaymatata.com/a.mp4" },
                    "480": { "url": "https://bcdn.hakunaymatata.com/b.mp4" }
                }
            }
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve(
                    "vidlink.pro/api/b/movie/enc123",
                    200,
                    api_payload.to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_default();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].format, Format::Mp4);
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Dune (2021) (VidLink 1080p)")
        );
        assert_eq!(streams[1].meta.resolution, Some(480));
        Ok(())
    }

    #[tokio::test]
    async fn series_requests_the_season_episode_route() -> Result<(), SourceError> {
        let api_payload = serde_json::json!({
            "url": "https://bcdn.hakunaymatata.com/tv1.m3u8"
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"tvenc"}"#)
                .serve(
                    "vidlink.pro/api/b/tv/tvenc/1/2",
                    200,
                    api_payload.to_string(),
                ),
        );
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
        assert_eq!(streams[0].format, Format::Hls);
        // The series stream title carries S01E02.
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 (VidLink Unknown)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn playlist_fetch_failure_falls_back_to_the_auto_card() {
        let api_payload = serde_json::json!({
            "stream": { "playlist": "https://bcdn.hakunaymatata.com/master.m3u8" }
        });
        // The playlist is not scripted — the fetcher answers not-found.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve(
                    "vidlink.pro/api/b/movie/enc123",
                    200,
                    api_payload.to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the fallback fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://bcdn.hakunaymatata.com/master.m3u8"
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Dune (2021) (VidLink Auto)")
        );
    }

    #[tokio::test]
    async fn recursive_urls_skip_subtitle_shapes() {
        let api_payload = serde_json::json!({
            "playback": {
                "manifest": "https://cdn.example/play/index.m3u8",
                "subtitle": "https://cdn.example/play/en.vtt",
                "poster": "https://cdn.example/art.jpg"
            }
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve(
                    "vidlink.pro/api/b/movie/enc123",
                    200,
                    api_payload.to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the recursive fixture must resolve: {e}"));
        // The .vtt subtitle is skipped; the manifest and the poster
        // both ship (the upstream findUrls pushes every non-subtitle
        // http string).
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example/play/index.m3u8"
        );
        assert_eq!(streams[1].url.as_str(), "https://cdn.example/art.jpg");
    }

    #[tokio::test]
    async fn duplicate_urls_collapse() {
        let api_payload = serde_json::json!({
            "streams": [
                { "url": "https://bcdn.hakunaymatata.com/dup.mp4" },
                { "url": "https://bcdn.hakunaymatata.com/dup.mp4" }
            ]
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve(
                    "vidlink.pro/api/b/movie/enc123",
                    200,
                    api_payload.to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the dedup fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
    }

    #[tokio::test]
    async fn enc_service_failure_is_not_found() {
        // The enc service is not scripted.
        let fetcher = Arc::new(MockFetcher::new().serve("vidlink.pro/api/b/movie/none", 200, "{}"));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn empty_response_is_not_found() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("enc-dec.app/api/enc-vidlink", 200, r#"{"result":"enc123"}"#)
                .serve("vidlink.pro/api/b/movie/enc123", 200, "{}"),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn api_miss_is_not_found() {
        let fetcher = Arc::new(MockFetcher::new().serve(
            "enc-dec.app/api/enc-vidlink",
            200,
            r#"{"result":"enc123"}"#,
        ));
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[test]
    fn quality_extraction_covers_the_ladder() {
        assert_eq!(
            extract_quality(&serde_json::json!({"quality": "1080"})),
            "1080p"
        );
        assert_eq!(
            extract_quality(&serde_json::json!({"quality": "2160p"})),
            "4K"
        );
        assert_eq!(
            extract_quality(&serde_json::json!({"resolution": "3840x2160"})),
            "4K"
        );
        assert_eq!(
            extract_quality(&serde_json::json!({"label": "FHD"})),
            "1080p"
        );
        assert_eq!(extract_quality(&serde_json::json!({})), "Unknown");
        assert_eq!(quality_from_resolution("1920x1080"), "1080p");
        assert_eq!(quality_from_resolution("3840x2160"), "4K");
        assert_eq!(quality_from_resolution(""), "Auto");
        assert_eq!(quality_rank("4K"), 5);
        assert_eq!(quality_rank("Unknown"), -3);
    }

    #[test]
    fn height_parsing_matches_the_wrapper() {
        assert_eq!(parse_height("4K"), Some(2160));
        assert_eq!(parse_height("2160p"), Some(2160));
        assert_eq!(parse_height("1440p"), Some(1440));
        assert_eq!(parse_height("1080p"), Some(1080));
        assert_eq!(parse_height("720p"), Some(720));
        assert_eq!(parse_height("Auto"), None);
        assert_eq!(parse_height("Unknown"), None);
    }

    #[test]
    fn m3u8_variants_resolve_relative_urls() {
        let base = Url::parse("https://cdn.example/master.m3u8")
            .unwrap_or_else(|e| panic!("valid base URL: {e}"));
        let body = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1280x720\n\
720/playlist.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2\n\
https://other.example/abs.m3u8\n";
        let variants = parse_m3u8(body, &base);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].url, "https://cdn.example/720/playlist.m3u8");
        assert_eq!(variants[0].resolution, "1280x720");
        assert_eq!(variants[1].url, "https://other.example/abs.m3u8");
        // No resolution line → Auto.
        assert_eq!(quality_from_resolution(&variants[1].resolution), "Auto");
    }
}
