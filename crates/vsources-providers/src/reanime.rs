//! `ReAnime`: reanime.to — direct downloads and legacy HLS through `FlixCloud`
//! CDN.
//!
//! Ports `src/source/ReAnime.js` + `src/nuvio/reanime.cjs`:
//!
//! 1. TMDB → title/year → `GET /api/v1/search?q=<title>&limit=10`
//!    (the reanime.to Referer, a 3-attempt retry ladder) → anime
//!    entries with `anilist_id` (falling back to the id encoded in
//!    the cover URL, `/nx<id>-<hash>.jpg`) and `season_year`.
//! 2. An **exact-title + year gate** picks the match: normalized title
//!    equality plus `|season_year − tmdb_year| ≤ 3` for TV (± 1 for
//!    movies) — the blind first-result fallback served the wrong
//!    anime upstream (Task 37). A missing anilist id is recovered
//!    from the watch page's `anilist_id:<N>` payload.
//! 3. `GET /api/flix/<anilistId>/<ep>` gives the server embeds. Prefer
//!    `/d/<accessId>/__data.json`, resolving its indexed download metadata
//!    into a signed progressive file. Older deployments fall back to the
//!    shared encrypted HLS chain in [`crate::nuvio::flixcloud`].
//! 4. The wrapper dedupes by master URL and emits one card per
//!    server: the `ReAnime [<server>] | 1080p | <dataType>` name, the
//!    Japanese (Sub) audio label, `x264`/`WebDL` markers, and the
//!    `[multi, ja, en]` language flags.
//!
//! Upstream oddities ported faithfully:
//!
//! - In the legacy HLS path, the wrapper's `isDub` check reads `s.language`/`s.lang`, which
//!   the scraper **never sets** — every card is `Japanese (Sub)`,
//!   even for `dataType: "dub"` servers.
//! - The wrapper's `serverName` fallback chain (`s.source ||
//!   s.serverName || s.name || 'ReAnime'`) lands on `s.name`, the
//!   full `ReAnime [HD-1] | 1080p | sub` string, so the label reads
//!   `… — [ReAnime ReAnime [HD-1] | 1080p | sub] Japanese (Sub)`.
//!
//! Cuts for the library port:
//!
//! - The `/reanime-proxy/playlist.m3u8?url=…&key=…` URL scheme is cut
//!   (no server to host it). Cards ship the **direct master URL**
//!   with the headers the proxy sent upstream (`Referer`/`Origin:
//!   flixcloud.cc`), and the per-session 32-byte playlist XOR key
//!   rides [`Stream::behavior_hints`] as `reanimeXorKey` — the
//!   playlist body is base64+XOR encrypted and the segments are
//!   image-disguised MPEG-TS, which
//!   [`nuvio::decrypt`](crate::nuvio::decrypt) handles client-side.
//!   The proxy's Node `User-Agent` is dropped (the fetcher's browser
//!   UA replaces it — the net-layer convention).
//! - The scraper's transport ladder (curl → got-scraping → child
//!   Node) collapses onto the shared fetcher, which already
//!   impersonates Chrome TLS; the 3-attempt/3 s-backoff retry of its
//!   `fetchJson` stays.
//! - The scraper's own TMDB fetch and its `video_id`/`video_title`
//!   passthrough (nothing consumed them) — the shared
//!   [`TmdbClient`] serves the title; `meta.serverName`/
//!   `audioLabel`/`isMultiAudio` have no [`StreamMeta`] fields (the
//!   label carries them).
//! - The wrapper's 30 s race rides
//!   `with_deadline`.
//!
//! [`Stream::behavior_hints`]: vsources_core::types::Stream::behavior_hints
//! [`StreamMeta`]: vsources_core::types::StreamMeta

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, FetchResponse, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta,
};

use crate::nuvio::flixcloud::{m3u8_token_fields, parse_flix_page, resolve_flix_stream};
use crate::nuvio::with_deadline;

/// The provider id, upstream `this.id`.
const ID: &str = "reanime";
/// The display label, upstream `this.label`.
const LABEL: &str = "ReAnime";
/// The site origin, upstream `this.baseUrl` / the scraper's
/// `REANIME_API`.
const BASE_URL: &str = "https://reanime.to";
/// The `FlixCloud` embed host.
const FLIXCLOUD: &str = "https://flixcloud.cc";
/// The full Chrome UA — reanime.to gates on the UA/Referer pair.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// `FlixCloud`'s simple UA — its Cloudflare flags the full Chrome UA as
/// a bot (the scraper's `UA_SIMPLE`).
const UA_SIMPLE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
/// Upstream `this.ttl` — 5 min (stream tokens expire).
const TTL: Duration = Duration::from_mins(5);
/// One upstream fetch (the `fetchJson` default timeout).
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// The wrapper's `Promise.race` cap.
const SWEEP_DEADLINE: Duration = Duration::from_secs(30);
/// Retries per API call (the scraper's 3-attempt `fetchJson`).
const RETRIES: u32 = 3;
/// Backoff before each retry — `3000 * (attempt + 1)`.
const RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// `anilist_id:<N>` in the watch page's `SvelteKit` payload.
static ANILIST_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"anilist_id:(\d+)").unwrap_or_else(|e| panic!("valid anilist-id pattern: {e}"))
});

/// `/nx<anilistId>-<hash>.jpg` — the id encoded in a cover URL.
static COVER_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/nx(\d+)-").unwrap_or_else(|e| panic!("valid cover-id pattern: {e}"))
});

/// `(\d{3,4})` — the height inside the quality label.
static HEIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})").unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});

/// `/e/([a-zA-Z0-9_-]+)` — the access id of a flix `dataLink`.
static ACCESS_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/e/([a-zA-Z0-9_-]+)").unwrap_or_else(|e| panic!("valid access-id pattern: {e}"))
});

/// A trailing parenthesized qualifier — the search ladder strips it
/// before retrying (animotvslash's `cleanTitle` strip, kept private
/// there; same wrapper family, same pattern).
static TRAILING_PARENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(.*?\)\s*$").unwrap_or_else(|e| panic!("valid trailing-parens pattern: {e}"))
});

/// One reanime search result.
#[derive(Debug, Clone)]
struct ReanimeResult {
    /// The site's anime slug.
    anime_id: String,
    /// The `AniList` id (recovered from the cover URL when the API
    /// answers `0`).
    anilist_id: Option<u64>,
    /// The display title.
    title: String,
    /// The season year.
    year: Option<u16>,
}

/// One `FlixCloud` server of the `/api/flix` answer.
#[derive(Debug, Clone)]
struct FlixServer {
    /// The server display name (`HD-1`).
    server_name: String,
    /// The audio marker (`sub` / `dub`).
    data_type: String,
    /// The flixcloud embed URL (`…/e/<accessId>`).
    data_link: String,
}

/// One scraper stream — the shape the wrapper consumes.
#[derive(Debug, Clone)]
struct ReanimeStream {
    /// `ReAnime [<server>] | 1080p | <dataType>`.
    name: String,
    /// The decrypted master playlist URL.
    url: String,
    /// The quality label.
    quality: String,
    /// The audio marker — the wrapper's `isDub` probe target (never
    /// set by the scraper, so always `None`).
    language: Option<String>,
    /// Progressive download or legacy HLS.
    format: Format,
    /// The per-session playlist XOR key, base64 (empty when the WASM
    /// exposed none).
    xor_key_b64: String,
}

/// The `ReAnime` provider.
pub struct ReAnime {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl ReAnime {
    /// A provider over the shared TMDB client. The `FlixCloud`
    /// decryption chain is shared plumbing (not an embed the
    /// registry resolves), so no extractor registry is needed.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }
}

#[async_trait]
impl Source for ReAnime {
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

        let sweep = async { self.sweep(ctx, &name, year, media).await };
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(build_cards(&raw, &title))
    }
}

impl ReAnime {
    /// The scraper's whole sweep: search → match → anilist recovery →
    /// the flix server list → per-server `FlixCloud` resolution — the
    /// port of `reanime.cjs` `getStreams`.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        year: Option<u16>,
        media: &MediaRef,
    ) -> Vec<ReanimeStream> {
        let is_tv = media.season.is_some();
        let Some(anime) = find_anime_by_title(ctx, name, year, is_tv).await else {
            return Vec::new();
        };
        // A missing anilist id is recovered from the watch page.
        let anilist_id = match anime.anilist_id {
            Some(id) => id,
            None => match fetch_anime_meta(ctx, &anime.anime_id).await {
                Some(id) => id,
                None => return Vec::new(),
            },
        };

        // Absolute episode numbering; movies watch episode 1.
        let ep_num = if is_tv { media.episode.unwrap_or(1) } else { 1 };
        let servers = fetch_flix_servers(ctx, anilist_id, ep_num).await;
        if servers.is_empty() {
            return Vec::new();
        }

        let mut streams = Vec::new();
        let mut seen = HashSet::new();
        for server in servers {
            // HD-1 and HD-2 may share the same accessId.
            let Some(access_id) = ACCESS_ID
                .captures(&server.data_link)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map(|group| group.as_str().to_string())
            else {
                continue;
            };
            // The current FlixCloud download route is directly playable and
            // avoids the legacy encrypted-playlist adapter requirement.
            if let Some((url, quality)) = resolve_flix_download(ctx, &access_id).await {
                if seen.insert(url.as_str().to_string()) {
                    streams.push(ReanimeStream {
                        name: format!(
                            "ReAnime [{}] | {quality} | {}",
                            server.server_name, server.data_type
                        ),
                        url: url.to_string(),
                        quality,
                        language: Some(server.data_type.clone()),
                        format: Format::Mp4,
                        xor_key_b64: String::new(),
                    });
                }
                continue;
            }
            let Some(flix) = resolve_flixcloud(ctx, &access_id).await else {
                continue;
            };
            if !seen.insert(flix.master_url.clone()) {
                continue;
            }
            streams.push(ReanimeStream {
                name: format!(
                    "ReAnime [{}] | 1080p | {}",
                    server.server_name, server.data_type
                ),
                url: flix.master_url,
                quality: "1080p".to_string(),
                language: None,
                format: Format::Hls,
                xor_key_b64: flix.xor_key_b64,
            });
        }
        streams
    }
}

// ---------------------------------------------------------------------------
// Search + match
// ---------------------------------------------------------------------------

/// One `/api/v1/search` call — the port of `searchReanime` (3
/// attempts, the reanime.to Referer, the `encodeURIComponent`d
/// query).
async fn search_reanime(ctx: &ResolveCtx<'_>, query: &str) -> Vec<ReanimeResult> {
    let Ok(mut url) = Url::parse(&format!("{BASE_URL}/api/v1/search")) else {
        return Vec::new();
    };
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("limit", "10");
    let Some(json) = fetch_json_retry(ctx, &url).await else {
        return Vec::new();
    };
    let Some(results) = json.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|result| {
            let anime_id = result.get("anime_id")?.as_str()?.to_string();
            // Search sometimes answers `anilist_id: 0`; the cover URL
            // carries the id.
            let mut anilist_id = result
                .get("anilist_id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0);
            if anilist_id.is_none() {
                let cover = result
                    .pointer("/cover_image/extra_large")
                    .or_else(|| result.pointer("/cover_image/large"))
                    .or_else(|| result.pointer("/cover_image/medium"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                anilist_id = COVER_ID
                    .captures(cover)
                    .ok()
                    .flatten()
                    .and_then(|captures| captures.get(1))
                    .and_then(|group| group.as_str().parse().ok());
            }
            let title = ["/title/english", "/title/romaji", "/title/native"]
                .iter()
                .find_map(|pointer| result.pointer(pointer).and_then(Value::as_str))
                .unwrap_or("Unknown");
            Some(ReanimeResult {
                anime_id,
                anilist_id,
                title: title.to_string(),
                year: result
                    .get("season_year")
                    .and_then(Value::as_u64)
                    .and_then(|year| u16::try_from(year).ok()),
            })
        })
        .collect()
}

/// Find the anime for a title — the port of `findAnimeByTitle`: the
/// query ladder (raw, parens-stripped, before-colon), and per query
/// the exact-title + year gate (± 3 for TV, ± 1 for movies).
async fn find_anime_by_title(
    ctx: &ResolveCtx<'_>,
    title: &str,
    year: Option<u16>,
    is_tv: bool,
) -> Option<ReanimeResult> {
    let normalized = normalize(title);
    let target_year = u32::from(year.unwrap_or(0));
    let tolerance = if is_tv { 3 } else { 1 };
    for query in title_queries(title) {
        for result in search_reanime(ctx, &query).await {
            if normalize(&result.title) != normalized {
                continue;
            }
            let result_year = u32::from(result.year.unwrap_or(0));
            if target_year == 0
                || result_year == 0
                || target_year.abs_diff(result_year) <= tolerance
            {
                return Some(result);
            }
        }
    }
    None
}

/// The query ladder: raw → parens-stripped → before the first
/// colon/dash/en-dash, deduped.
fn title_queries(title: &str) -> Vec<String> {
    let stripped = TRAILING_PARENS.replace(title, "").trim().to_string();
    let mut head = title;
    for separator in [':', '-', '–'] {
        if let Some(cut) = head.find(separator) {
            head = &head[..cut];
        }
    }
    let head = head.trim();
    let mut queries = vec![title.to_string(), stripped, head.to_string()];
    let mut seen = HashSet::new();
    queries.retain(|query| !query.is_empty() && seen.insert(query.clone()));
    queries
}

/// The watch-page anilist id — the port of `fetchAnimeMeta`
/// (`GET /watch/<animeId>?ep=1`).
async fn fetch_anime_meta(ctx: &ResolveCtx<'_>, anime_id: &str) -> Option<u64> {
    let url = format!("{BASE_URL}/watch/{anime_id}?ep=1");
    let html = fetch_text_retry(ctx, &url).await?;
    ANILIST_ID
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
}

// ---------------------------------------------------------------------------
// The flix server list + the FlixCloud chain
// ---------------------------------------------------------------------------

/// The flix server list — the port of the `/api/flix/<anilistId>/<ep>`
/// fetch (a `{success, servers}` answer, `success: false` or an empty
/// server list both yielding nothing).
async fn fetch_flix_servers(ctx: &ResolveCtx<'_>, anilist_id: u64, ep: u32) -> Vec<FlixServer> {
    let Ok(url) = Url::parse(&format!("{BASE_URL}/api/flix/{anilist_id}/{ep}")) else {
        return Vec::new();
    };
    let Some(json) = fetch_json_retry(ctx, &url).await else {
        return Vec::new();
    };
    if json.get("success").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    json.get("servers")
        .and_then(Value::as_array)
        .map(|servers| {
            servers
                .iter()
                .map(|server| FlixServer {
                    server_name: server
                        .get("serverName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data_type: server
                        .get("dataType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data_link: server
                        .get("dataLink")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .filter(|server| !server.data_link.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the site's current Svelte download metadata (Nuvio's September
/// 2026 `ReAnime` adapter uses the same endpoint). Values are indexes into the
/// node's devalue array; parse those references instead of guessing tokens.
async fn resolve_flix_download(ctx: &ResolveCtx<'_>, access_id: &str) -> Option<(Url, String)> {
    let url = Url::parse(&format!("{FLIXCLOUD}/d/{access_id}/__data.json")).ok()?;
    let response = ctx
        .fetcher
        .request(
            FetchRequest::get(url)
                .with_header("Referer", format!("{FLIXCLOUD}/"))
                .with_header("User-Agent", UA)
                .with_timeout(Duration::from_secs(8)),
        )
        .await
        .ok()?;
    if !response.is_success() {
        return None;
    }
    download_from_data(&response.json::<Value>().ok()?)
}

fn download_from_data(data: &Value) -> Option<(Url, String)> {
    fn field<'a>(values: &'a [Value], object: &Value, key: &str) -> Option<&'a Value> {
        let index = usize::try_from(object.get(key)?.as_u64()?).ok()?;
        values.get(index)
    }
    for node in data.get("nodes")?.as_array()? {
        let Some(values) = node.get("data").and_then(Value::as_array) else {
            continue;
        };
        let Some(root) = values.first() else { continue };
        let Some(video) = field(values, root, "video") else {
            continue;
        };
        let Some(download) = field(values, root, "download") else {
            continue;
        };
        let id = field(values, video, "fileId")?.as_str()?;
        let token = field(values, download, "token")?.as_str()?;
        let mut base = Url::parse(field(values, download, "base")?.as_str()?).ok()?;
        if id.is_empty() || token.is_empty() || !matches!(base.scheme(), "https" | "http") {
            return None;
        }
        base.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push("download")
            .push(id);
        base.query_pairs_mut().append_pair("token", token);
        let quality = field(values, video, "resolution")
            .and_then(Value::as_str)
            .unwrap_or("1080p")
            .to_string();
        return Some((base, quality));
    }
    None
}

/// The `FlixCloud` decryption chain for one access id — the port of
/// `resolveFlixcloud`'s network half (the `/e/` page with the simple
/// UA, then the `/api/m3u8/<token>` pair), on top of the shared
/// [`crate::nuvio::flixcloud`] crypto.
async fn resolve_flixcloud(
    ctx: &ResolveCtx<'_>,
    access_id: &str,
) -> Option<crate::nuvio::flixcloud::FlixStream> {
    let page_url = format!("{FLIXCLOUD}/e/{access_id}?v=2");
    let response = flix_get(ctx, &page_url).await?;
    let page = parse_flix_page(&response.body)?;

    let token_url = format!("{FLIXCLOUD}/api/m3u8/{}", page.token_value);
    let response = flix_get(ctx, &token_url).await?;
    let json: Value = serde_json::from_str(&response.body).ok()?;
    let (vid_b64, key_b64) = m3u8_token_fields(&json, &page.token_value)?;
    let materials = page.materials(&vid_b64, &key_b64);
    resolve_flix_stream(&materials)
}

/// One flixcloud fetch — `fetchBufNodeChain` (the simple UA, a plain
/// `Accept`, no retry).
async fn flix_get(ctx: &ResolveCtx<'_>, url: &str) -> Option<FetchResponse> {
    let url = Url::parse(url).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA_SIMPLE)
        .with_header("Accept", "*/*")
        .with_timeout(FETCH_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    Some(response)
}

// ---------------------------------------------------------------------------
// Fetch helpers with retry
// ---------------------------------------------------------------------------

/// GET a URL as JSON with the 3-attempt retry — the scraper's
/// `fetchJson` over the curl chain (the reanime.to Referer, the full
/// Chrome UA).
async fn fetch_json_retry(ctx: &ResolveCtx<'_>, url: &Url) -> Option<Value> {
    for attempt in 0..RETRIES {
        let request = FetchRequest::get(url.clone())
            .with_header("User-Agent", UA)
            .with_header("Accept", "application/json")
            .with_header("Referer", format!("{BASE_URL}/"))
            .with_timeout(FETCH_TIMEOUT);
        if let Ok(response) = ctx.fetcher.request(request).await
            && response.is_success()
            && let Ok(json) = serde_json::from_str::<Value>(&response.body)
        {
            return Some(json);
        }
        if attempt + 1 < RETRIES {
            tokio::time::sleep(retry_backoff(attempt)).await;
        }
    }
    None
}

/// GET a URL as text with the 3-attempt retry — the scraper's
/// `fetchText`.
async fn fetch_text_retry(ctx: &ResolveCtx<'_>, url: &str) -> Option<String> {
    let Ok(url) = Url::parse(url) else {
        return None;
    };
    for attempt in 0..RETRIES {
        let request = FetchRequest::get(url.clone())
            .with_header("User-Agent", UA)
            .with_header("Accept", "*/*")
            .with_header("Referer", format!("{BASE_URL}/"))
            .with_timeout(FETCH_TIMEOUT);
        if let Ok(response) = ctx.fetcher.request(request).await
            && response.is_success()
        {
            return Some(response.body);
        }
        if attempt + 1 < RETRIES {
            tokio::time::sleep(retry_backoff(attempt)).await;
        }
    }
    None
}

/// The backoff before retry `attempt` — `3000 * (attempt + 1)`.
fn retry_backoff(attempt: u32) -> Duration {
    RETRY_BACKOFF * (attempt + 1)
}

// ---------------------------------------------------------------------------
// Card building (the wrapper's hand-built results)
// ---------------------------------------------------------------------------

/// The wrapper's card building: dedupe by URL, audio label, language
/// flags, and the flixcloud hotlink headers — the port of
/// `ReAnime.js` `handleInternal`'s loop (see the module docs for the
/// two oddities).
fn build_cards(streams: &[ReanimeStream], display_title: &str) -> Vec<Stream> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for stream in streams {
        if !stream.url.starts_with("http") || !seen.insert(stream.url.clone()) {
            continue;
        }
        // isDub probes `language`/`lang`, which the scraper never
        // sets — always false.
        let is_dub = stream
            .language
            .as_deref()
            .is_some_and(|language| language.to_lowercase().contains("dub"));
        let audio_label = if is_dub {
            "English (Dub)"
        } else {
            "Japanese (Sub)"
        };
        let country_codes = if is_dub {
            vec![CountryCode::Multi, CountryCode::En, CountryCode::Ja]
        } else {
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        };
        // The fallback chain (`s.source || s.serverName || s.name`)
        // lands on the full name string — the scraper sets neither
        // `source` nor `serverName` at the top level.
        let server_name = stream.name.clone();

        let height = HEIGHT
            .captures(&stream.quality)
            .ok()
            .flatten()
            .and_then(|captures| captures.get(1))
            .and_then(|group| group.as_str().parse::<u16>().ok())
            .unwrap_or(1080);

        let mut meta = StreamMeta {
            languages: country_codes,
            resolution: Some(height),
            quality: Some("WebDL".to_string()),
            codec: Some("x264".to_string()),
            source_id: Some(ID.to_string()),
            source_label: Some(LABEL.to_string()),
            ..StreamMeta::default()
        };
        // The headers the proxy sent upstream.
        meta.request_headers
            .insert("Referer".to_string(), format!("{FLIXCLOUD}/"));
        meta.request_headers
            .insert("Origin".to_string(), FLIXCLOUD.to_string());

        let mut card = Stream {
            url: Url::parse(&stream.url).unwrap_or_else(|e| panic!("valid master URL: {e}")),
            format: stream.format,
            label: Some(format!(
                "{display_title} — [ReAnime {server_name}] {audio_label}"
            )),
            meta,
            ttl: TTL,
            is_external: false,
            behavior_hints: std::collections::BTreeMap::new(),
        };
        if !stream.xor_key_b64.is_empty() {
            card.behavior_hints
                .insert("reanimeXorKey".to_string(), stream.xor_key_b64.clone());
        }
        out.push(card);
    }
    out
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// The scraper's `norm`: lowercase, combining marks and
/// non-alphanumerics collapsed to single spaces, trimmed (NFD is
/// unavailable without an extra dependency — the `allwish`
/// precedent).
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(character);
        } else if ('\u{0300}'..='\u{036f}').contains(&character) {
            // A combining mark — dropped.
        } else {
            pending_space = true;
        }
    }
    out
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the display title.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
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
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};

    use super::*;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare
    /// path, so the search and TMDB calls can be scripted by path
    /// alone.
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

    /// The fixture media: Haikyuu!! S01E01.
    const TMDB_ID: u64 = 123_456;

    /// The live-captured `FlixCloud` material (shared with
    /// `nuvio::flixcloud`'s ground-truth corpus): the `/e/` page's
    /// crypto fields, the WASM payload, and the `/api/m3u8` pair.
    const SEED: &str = "d231526af61cb187";
    const FRAG1_B64: &str = "DSxOQu715GZX/ZY8t5r4DuTisXgzOCBpRMlis2gp814=";
    const IV_B64: &str = "XOw1a12lw6Iui3lDmaAU+Q==";
    const KEY_B64: &str = "9xsi+5vEQVghIGHFCPf3z4Cmlp6VtbVCMT/RFECq6kA=";
    const KEY_FRAG2_B64: &str = "yblBgEX8Lm/8hqiFRMdHhmeHQtHggAJ/zxP+K9qdijw=";
    const VID_B64: &str = "k0+di6VPrLtFKEQzi/xbLLIyQG0AiTzHsY/fysKCDftlgRkSe1+B8rugfP+0N7TIyQgZLj13IsgsXbaqXkQmVp/owcKVBKwhOCy6rahiKQ6/FvYbKPhNXWuN+Zqb8hFPNoZWiff9xgqBKkICVYV719Estb9livtn2YzYrpcpbIj4dvp1tYF+/GG7A5DmtIDZSKD58w4iL51HB7z0HiLrZSxxdV7d1KOqsOfhCisnhGpDXKcYK62ahcxUE9QkHqdYXBrWXC3Jmp301y6py9mPNfwiJZfvb1BFiuYl4aW7zrKoBcx69P8U6dSYy7uLn050UVgF9f4mr4HAdWhxQOBob9bXiHYlHNXzDwFeyvT8mFxGKfC/84FmKkjNdM+BTx9IV7SH8F3D9Y2AnJvsZPitxys3YgawFVkECEA3lkxd4hPKEBfT10dSo0YFMavSIsgmgnUAlzpWq3IB84gLAx/oQkDtyL0bC+jBrLh+9dDxYvY=";
    const WASM_B64: &str = "AGFzbQEAAAABEQNgAX8AYAV/f39/fwBgAAF/AwQDAAECBQMBAAEGBgF/AUEACwcZBAZtZW1vcnkCAAJfcwAAAl9yAAECX2MAAgqBAgMGACAAJAALuQEBA39BACEFA0ACQCAFIARPDQAgACAFai0AACABIAVqLQAAcyACIAVqLQAAcyEGIAZBB2tB/wFxIQYgBkEDdiAGQQV0Qf8BcXIhBiAGQThqQf8BcSEGIAZBzAFrQf8BcSEGIAZBOWtB/wFxIQYgBkECdEH/AXEgBkEGdnIhBiAGQQV0Qf8BcSAGQQN2ciEGIAVBJGwjAGpB/wFxIQcgBiAHcyEGIAMgBWogBjoAACAFQQFqIQUMAQsLCz0BAX9BACEAA0ACQCAAQSBPDQBBkBAgAGpB0A8gAGotAABB8A8gAGotAABzOgAAIABBAWohAAwBCwtBkBALC0cBAEHQDwtATaKqw/G2AbW7MrfEanX/4pSun/PjKX+FPpjrnJiEjAnzid2QFS8ObKFHqsfejfvbGY5oAsMlhWTpxShaNhG4Zw==";

    /// The master URL the ground-truth material decrypts to.
    const LIVE_MASTER_URL: &str = "https://fetch8.flixcloud.cc/_v7/e0dfc0c5-7712-4567-9088-d86701425be4/master.m3u8?token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ2aWRlb19pZCI6ImUwZGZjMGM1LTc3MTItNDU2Ny05MDg4LWQ4NjcwMTQyNWJlNCIsImNsaWVudF9pcCI6IjY2LjIzMS40NC4yMjciLCJleHAiOjE3OTAzNTMwNzIsImlhdCI6MTc5MDMzMTQ3MiwiaXNzIjoidmlkZW8taG9zdGluZy1wbGF0Zm9ybSJ9.2AzP02FauIzJIGi3i688EqyNNTFH9qZXc30JoDuUO0c";

    /// The `/e/vimu1sw5xonj?v=2` page — the live capture's payload
    /// region (mixed quoted/unquoted keys, same decoys).
    fn flix_page() -> String {
        format!(
            r#"<body>data: [null,null,{{type:"data",data:{{"42446185cbe125a1_cbe125a1":"c9b23d633ce35ebd18031cb0",available_fonts:{{LTFinnegan_MediumIt:1}}}}}}]
			is_iframe:false,obfuscation_seed:"{SEED}","57c01216adfdf9c3440859d4698792fb_2608093f":"df95627f881ea096141d836e",default_audio_track:0,is_domain_owner:false,"790430a10008fa831d94dd58_2185c397a1a90e8b":"82ae1ce75577b128e5082e07",obfuscated_crypto_data:{{cd_2e15a574:{{ad_5e624ac7:[{{od_e28700f6:{{kf_da67e9be:"{FRAG1_B64}",ivf_cb9d11d5:"{IV_B64}",db87:"0.uuivz6wdlik",ed93:"XO0zsibQ17Q=",metadata:{{timestamp:1790331472437,version:"2.1",encoding:"aes256cbc"}}}}]}}}},bb241621763744c558a8d7b1f3f38bb1:"bcb66e66602e3b1861e57039",intro_chapter:{{start:47,end:136,title:"OP"}},aid:"vimu1sw5xonj",video_title:"[Anime Time] Haikyuu!! - 01.mkv","3893eee5f98fb215_d6b7d80f":"{KEY_FRAG2_B64}",video_id:"e0dfc0c5-7712-4567-9088-d86701425be4",w_payload:"{WASM_B64}"</body>"#
        )
    }

    /// The `/api/m3u8/c9b23d633ce35ebd18031cb0` answer — the keys are
    /// the sha256 token derivations.
    fn flix_m3u8_json() -> String {
        format!(r#"{{"2e20e3f597":"{VID_B64}","68141d28e2":"{KEY_B64}"}}"#)
    }

    /// The expected `reanimeXorKey` — the WASM data-segment XOR.
    fn expected_xor_key_b64() -> String {
        STANDARD.encode([
            0xbe, 0x2b, 0x77, 0x53, 0xe4, 0x99, 0x0f, 0xd9, 0x1a, 0x75, 0x1d, 0x03, 0xb4, 0xf8,
            0x04, 0x39, 0x8d, 0x20, 0xf7, 0xf1, 0x20, 0x0c, 0xfa, 0xe1, 0xd7, 0x5d, 0xc3, 0xc6,
            0xae, 0x95, 0x34, 0x6e,
        ])
    }

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> ReAnime {
        ReAnime::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// TMDB + reanime + flixcloud pages for the fixture media.
    fn pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"Haikyuu!!","first_air_date":"2014-04-06"}"#,
        )
        .page(
            "/api/v1/search",
            200,
            r#"{"results":[{"anime_id":"haikyuu","anilist_id":11,"title":{"english":"Haikyuu!!","romaji":"Haikyuu!!"},"season_year":2014,"can_watch":true}]}"#,
        )
        .page(
            "/api/flix/11/1",
            200,
            r#"{"success":true,"servers":[{"serverName":"HD-1","dataType":"sub","dataLink":"https://flixcloud.cc/e/vimu1sw5xonj"},{"serverName":"HD-2","dataType":"dub","dataLink":"https://flixcloud.cc/e/vimu1sw5xonj"}]}"#,
        )
        .page("/e/vimu1sw5xonj", 200, flix_page())
        .page("/api/m3u8/c9b23d633ce35ebd18031cb0", 200, flix_m3u8_json())
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "reanime");
        assert_eq!(info.label, "ReAnime");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://reanime.to/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_the_flixcloud_chain_and_dedupes_shared_masters() -> Result<(), SourceError> {
        let mock = Arc::new(pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        // HD-1 and HD-2 share one accessId — one master, one card.
        assert_eq!(streams.len(), 1);
        let card = &streams[0];
        assert_eq!(card.url.as_str(), LIVE_MASTER_URL);
        assert_eq!(card.format, Format::Hls);

        // The wrapper's oddity chain: the server-name fallback lands
        // on the full name string, and `language` is never set, so
        // the audio label is always Japanese (Sub).
        assert_eq!(
            card.label.as_deref(),
            Some("Haikyuu!! S01E01 — [ReAnime ReAnime [HD-1] | 1080p | sub] Japanese (Sub)")
        );

        // The meta markers: 1080, x264, WebDL, [multi, ja, en].
        assert_eq!(card.meta.resolution, Some(1080));
        assert_eq!(card.meta.codec.as_deref(), Some("x264"));
        assert_eq!(card.meta.quality.as_deref(), Some("WebDL"));
        assert_eq!(
            card.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        );
        assert_eq!(card.meta.source_id.as_deref(), Some("reanime"));

        // The flixcloud hotlink headers the proxy sent.
        assert_eq!(
            card.meta.request_headers.get("Referer").map(String::as_str),
            Some("https://flixcloud.cc/")
        );
        assert_eq!(
            card.meta.request_headers.get("Origin").map(String::as_str),
            Some("https://flixcloud.cc")
        );

        // The per-session playlist XOR key rides the behavior hints.
        let expected_key = expected_xor_key_b64();
        assert_eq!(
            card.behavior_hints.get("reanimeXorKey"),
            Some(&expected_key)
        );
        assert_eq!(card.ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn a_year_mismatch_is_rejected_by_the_exact_gate() {
        // Same title, a 1985 season year — the ± 3 TV gate rejects it.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Haikyuu!!","first_air_date":"2014-04-06"}"#,
                )
                .page(
                    "/api/v1/search",
                    200,
                    r#"{"results":[{"anime_id":"odin","anilist_id":99,"title":{"english":"Haikyuu!!"},"season_year":1985}]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn a_missing_anilist_id_is_recovered_from_the_watch_page() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Haikyuu!!","first_air_date":"2014-04-06"}"#,
                )
                .page(
                    "/api/v1/search",
                    200,
                    r#"{"results":[{"anime_id":"haikyuu","anilist_id":0,"title":{"english":"Haikyuu!!"},"season_year":2014}]}"#,
                )
                .page(
                    "/watch/haikyuu",
                    200,
                    r#"<script>data:{anilist_id:11,format:"TV"}</script>"#,
                )
                .page(
                    "/api/flix/11/1",
                    200,
                    r#"{"success":true,"servers":[{"serverName":"HD-1","dataType":"sub","dataLink":"https://flixcloud.cc/e/vimu1sw5xonj"}]}"#,
                )
                .page("/e/vimu1sw5xonj", 200, flix_page())
                .page(
                    "/api/m3u8/c9b23d633ce35ebd18031cb0",
                    200,
                    flix_m3u8_json(),
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        assert_eq!(streams.len(), 1);
        assert!(
            mock.requests()
                .iter()
                .any(|request| request.url.path() == "/watch/haikyuu")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_movie_reference_watches_episode_one() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/movie/{TMDB_ID}"),
                    200,
                    r#"{"title":"Suzume","release_date":"2022-11-11"}"#,
                )
                .page(
                    "/api/v1/search",
                    200,
                    r#"{"results":[{"anime_id":"suzume","anilist_id":22,"title":{"english":"Suzume"},"season_year":2022}]}"#,
                )
                .page(
                    "/api/flix/22/1",
                    200,
                    r#"{"success":true,"servers":[{"serverName":"HD-1","dataType":"sub","dataLink":"https://flixcloud.cc/e/vimu1sw5xonj"}]}"#,
                )
                .page("/e/vimu1sw5xonj", 200, flix_page())
                .page("/api/m3u8/c9b23d633ce35ebd18031cb0", 200, flix_m3u8_json()),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        // The display title carries the movie year, not S/E markers.
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.starts_with("Suzume (2022) — [ReAnime"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let media = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: None,
            name: "Haikyuu!!".to_string(),
            year: Some(2014),
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

        assert_eq!(streams.len(), 1);
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
    fn builds_the_title_query_ladder() {
        // Raw → parens-stripped → before the first colon/dash.
        let queries = title_queries("Mutiny (2026)");
        assert_eq!(queries[0], "Mutiny (2026)");
        assert_eq!(queries[1], "Mutiny");
        let queries = title_queries("Sousou no Frieren: Beyond Journey's End");
        assert_eq!(queries[queries.len() - 1], "Sousou no Frieren");
    }

    #[test]
    fn normalizes_for_the_exact_gate() {
        assert_eq!(normalize("Haikyuu!!"), "haikyuu");
        assert_eq!(normalize("  Shippūden  "), "shipp den");
        assert_eq!(normalize("Solo-Leveling"), "solo leveling");
    }
    #[test]
    fn current_download_metadata_resolves_devalue_references() {
        let data = serde_json::json!({"nodes":[null,{"data":[
            {"video":1,"download":5},{"fileId":2,"resolution":3},
            "60aa496b-cea9-45a8-a4f4-8395990c0d06","1080p",null,
            {"base":6,"token":7},"https://fetch8.flixcloud.cc","test+token/="
        ]}]});
        let (url, quality) =
            download_from_data(&data).unwrap_or_else(|| panic!("download metadata"));
        assert_eq!(url.path(), "/download/60aa496b-cea9-45a8-a4f4-8395990c0d06");
        assert_eq!(
            url.query_pairs()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.into_owned())
                .as_deref(),
            Some("test+token/=")
        );
        assert_eq!(quality, "1080p");
        assert!(
            download_from_data(&serde_json::json!({"nodes":[{"data":[{"video":999}]}]})).is_none()
        );
    }
}
