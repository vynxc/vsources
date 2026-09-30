//! `2Peckle`/`ShowBox`: direct files from `FebBox` shares.
//!
//! Ports `src/source/Peckle.js`:
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`); name/year is best-effort
//!    (`getTmdbNameAndYear(…).catch(() => [null, null])` — the title
//!    falls back to `TMDB {id}`).
//! 2. TMDB → `ShowBox` id via the `id-mapping-api-showbox-proxy.hf.space`
//!    mapping proxy (`fetchProxy`).
//! 3. `ShowBox` id → `FebBox` share code via `showbox.media/index/share_link`
//!    with `X-Requested-With: XMLHttpRequest` (`getShareCode`; TV tries
//!    `type=2` first, then falls back to `type=1`).
//! 4. List the share's files, non-recursively (`file/file_share_list`);
//!    when the top level holds no video, walk one level of subdirectories
//!    (upstream caps at 5 directories, 2 levels deep — Render free-tier
//!    limits, kept verbatim).
//! 5. For each video file (episode-marker filtered for TV, first 2 files
//!    only), fetch `console/video_quality_list` **with the `FebBox` cookie**
//!    — an HTML table of `div.file_quality` rows carrying `data-url`,
//!    `data-quality`, and a `.size` cell: direct MKV (`ORG`) and
//!    transcoded HLS (4K/1080p/720p/360p) from `hls.shegu.net`. The JS
//!    fetched the two files' qualities in parallel; the port serializes
//!    the same requests.
//!
//! Requires the `FebBox` `ui=` cookie (upstream `FEBBOX_COOKIE`); without
//! it the provider answers [`SourceError::NotFound`] — upstream logs a
//! console error and returns zero streams. Upstream read it from the
//! environment; a library takes it as a constructor argument.
//!
//! Cuts and mappings from the upstream:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{titleBase} (2Peckle {qualityLabel})`) is carried as
//!   [`Stream::label`].
//! - the JS's `hdr` and `bitDepth` meta fields have no `StreamMeta`
//!   counterparts; they fold into the `quality` and `codec` strings
//!   (e.g. `BluRay Remux Dolby Vision`, `HEVC 10-bit`).
//! - upstream wraps every helper in `try/catch` and returns zero streams
//!   on any failure. The port keeps that for fetch failures (they map to
//!   `NotFound`, visible through the parent's negative cache) but
//!   distinguishes a malformed 2xx payload as a structural
//!   [`SourceError::Scrape`] so format drift surfaces; the per-file
//!   quality listing stays fully best-effort (a failed file is skipped).
//! - the JS's `console.error` diagnostics — no logging dependency in this
//!   crate.
//! - no result cache here (the parent's [`CachedSource`](crate::CachedSource)
//!   owns that), and
//!   no internal caches: the JS kept none for this flow.
//! - HLS URLs play without a `Referer` upstream; `ORG` (MKV) "may need
//!   cookie" per the JS comment — the comment is preserved, no headers
//!   are attached.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta,
};

/// The provider id, upstream `this.id`.
const ID: &str = "peckle";
/// The display label, upstream `this.label`.
const LABEL: &str = "2Peckle";
/// The `ShowBox` origin, upstream `SHOWBOX_BASE` (and `this.baseUrl`).
const SHOWBOX_BASE: &str = "https://showbox.media";
/// The `FebBox` origin, upstream `FEBBOX_BASE`.
const FEBBOX_BASE: &str = "https://www.febbox.com";
/// The id-mapping proxy, upstream `PROXY_BASE`.
const PROXY_BASE: &str = "https://id-mapping-api-showbox-proxy.hf.space/api/media";

/// Upstream `this.ttl` — 10 minutes: `FebBox` sign tokens rotate fast.
const TTL: Duration = Duration::from_mins(10);

/// Upstream request timeouts (`timeout: { request: … }`).
const PROXY_TIMEOUT: Duration = Duration::from_secs(10);
const SHARE_TIMEOUT: Duration = Duration::from_secs(8);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(8);

/// `div.file_quality` rows inside the quality-list HTML (the JS's
/// cheerio selector).
static FILE_QUALITY: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div.file_quality")
        .unwrap_or_else(|e| panic!("valid file_quality selector: {e}"))
});
/// The `.size` cell inside a row.
static SIZE_CELL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".size").unwrap_or_else(|e| panic!("valid size selector: {e}"))
});

/// The `2Peckle` provider.
pub struct Peckle {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// TMDB identity and metadata resolution.
    tmdb: Arc<TmdbClient>,
    /// The normalized `FebBox` `ui=…` cookie header value.
    cookie: Option<String>,
}

impl Peckle {
    /// Build the provider. The cookie is the upstream `FEBBOX_COOKIE`:
    /// comma-separated tokens are accepted and the first non-empty one
    /// is used, with an `ui=` prefix added when missing — ports
    /// `getCookie`. Without a cookie the provider resolves to
    /// [`SourceError::NotFound`].
    #[must_use]
    pub fn new(febbox_cookie: Option<String>, tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(SHOWBOX_BASE).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            cookie: febbox_cookie.and_then(|raw| normalize_cookie(&raw)),
        }
    }
}

#[async_trait]
impl Source for Peckle {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let Some(cookie) = self.cookie.as_deref() else {
            // No FEBBOX_COOKIE — upstream logs and returns zero streams.
            return Err(SourceError::NotFound);
        };

        let tmdb_id = resolve_tmdb_id(ctx, media, &self.tmdb).await?;
        // Best-effort name/year — `.catch(() => [null, null])`.
        let (name, year) = name_and_year_best_effort(ctx, media, &self.tmdb, tmdb_id).await;
        let is_tv = media.season.is_some();
        let title_base = title_base(name.as_deref(), year, media, tmdb_id);

        // Step 1: TMDB → `ShowBox` id via the mapping proxy.
        let showbox_id = fetch_showbox_id(ctx, tmdb_id, is_tv, media.season, media.episode).await?;

        // Step 2: `ShowBox` id → `FebBox` share code (TV tries type 2 first,
        // both kinds fall back to type 1 — the JS retries the same
        // request for movies).
        let share_type = u8::from(is_tv) + 1;
        let share_code = match get_share_code(ctx, &showbox_id, share_type).await? {
            Some(code) => code,
            None => match get_share_code(ctx, &showbox_id, 1).await? {
                Some(code) => code,
                None => return Err(SourceError::NotFound),
            },
        };

        // Step 3: the share's video files (with the TV episode filter).
        let files = video_files(ctx, &share_code, is_tv, media.season, media.episode).await?;
        if files.is_empty() {
            return Err(SourceError::NotFound);
        }

        // Step 4: qualities for the first two files only — a Render
        // free-tier cap kept verbatim.
        let streams = quality_streams(ctx, &share_code, cookie, &files, &title_base).await;
        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(streams)
    }
}

/// `raw.split(',').map(trim).filter(Boolean)[0]`, `ui=`-prefixed —
/// ports `getCookie`; empty input normalizes to no cookie at all.
fn normalize_cookie(raw: &str) -> Option<String> {
    let first = raw
        .split(',')
        .map(str::trim)
        .find(|token| !token.is_empty())?;
    Some(if first.starts_with("ui=") {
        first.to_string()
    } else {
        format!("ui={first}")
    })
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn resolve_tmdb_id(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => tmdb.tmdb_id_from_imdb(imdb, media.kind).await,
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear`, with its `.catch(() => [null, null])`.
async fn name_and_year_best_effort(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> (Option<String>, Option<u16>) {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return (Some(resolved.name.clone()), resolved.year);
    }
    match tmdb.name_and_year(tmdb_id, media.kind, None).await {
        Ok(name) => (Some(name.name), name.year),
        Err(_) => (None, None),
    }
}

/// The `(name || "TMDB {id}") + (season ? " S01E02" : " ({year})")`
/// title base — the upstream composition, carried in the stream label.
fn title_base(name: Option<&str>, year: Option<u16>, media: &MediaRef, tmdb_id: u64) -> String {
    let base = name.map_or_else(|| format!("TMDB {tmdb_id}"), str::to_string);
    if media.season.is_some() {
        format!("{base} {}", media.format_season_and_episode())
    } else {
        let year = year.map(|year| year.to_string()).unwrap_or_default();
        format!("{base} ({year})")
    }
}

/// TMDB → `ShowBox` id via the mapping proxy — ports `fetchProxy` (its
/// try/catch collapses fetch failures into the same miss as
/// `success: false`).
async fn fetch_showbox_id(
    ctx: &ResolveCtx<'_>,
    tmdb_id: u64,
    is_tv: bool,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<String, SourceError> {
    // `mediaType === 'tv' && season && episode` — the JS quirk: a series
    // reference without both parts takes the movie path.
    let path = if is_tv && season.is_some() && episode.is_some() {
        format!(
            "{PROXY_BASE}/tv/{tmdb_id}/{}/{}",
            season.unwrap_or(0),
            episode.unwrap_or(0)
        )
    } else {
        format!("{PROXY_BASE}/movie/{tmdb_id}")
    };
    let url = Url::parse(&path)
        .map_err(|error| SourceError::scrape(ID, format!("invalid proxy URL: {error}")))?;
    let request = FetchRequest::get(url)
        .with_header("Accept", "application/json")
        .with_timeout(PROXY_TIMEOUT);
    let response = ctx
        .fetcher
        .request(request)
        .await
        .map_err(|_| SourceError::NotFound)?;
    let data: ProxyResponse = serde_json::from_str(&response.body)
        .map_err(|error| SourceError::scrape(ID, format!("invalid proxy payload: {error}")))?;
    // `!proxyData?.success || !proxyData.id` → zero streams.
    let id = loose_string(&data.id).filter(|_| data.success);
    id.ok_or(SourceError::NotFound)
}

/// `ShowBox` id → `FebBox` share code — ports `getShareCode`; `Ok(None)` is
/// the JS's `null` (`code !== 1`, no link, or a swallowed fetch error),
/// which triggers the caller's type fallback.
async fn get_share_code(
    ctx: &ResolveCtx<'_>,
    showbox_id: &str,
    share_type: u8,
) -> Result<Option<String>, SourceError> {
    let url = Url::parse(&format!("{SHOWBOX_BASE}/index/share_link"))
        .map_err(|error| SourceError::scrape(ID, format!("invalid share_link URL: {error}")))?;
    let mut url = url;
    url.query_pairs_mut()
        .append_pair("id", showbox_id)
        .append_pair("type", &share_type.to_string());
    let request = FetchRequest::get(url)
        .with_header("Accept", "application/json")
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_header("Referer", format!("{SHOWBOX_BASE}/"))
        .with_timeout(SHARE_TIMEOUT);
    // The JS's try/catch → null → the type-1 fallback.
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Ok(None);
    };
    let data: ShareLinkResponse = serde_json::from_str(&response.body)
        .map_err(|error| SourceError::scrape(ID, format!("invalid share_link payload: {error}")))?;
    if data.code != 1 {
        return Ok(None);
    }
    // `data.data?.link` with the trailing slash stripped; the code is the
    // last path segment (the JS also carried the full link, unused).
    let Some(link) = data.data.and_then(|data| data.link) else {
        return Ok(None);
    };
    let link = link.trim_end_matches('/');
    let code = link.rsplit('/').next().unwrap_or_default();
    Ok(Some(code.to_string()))
}

/// The share's video files — top level first, then one level of
/// subdirectories when the top level holds no videos, then the TV
/// episode filter — ports `listShareFiles`/`walkShare`/`matchEpisode`.
/// A failed listing is an empty list (the JS's catch → `[]`); a
/// malformed payload is a structural surprise.
async fn video_files(
    ctx: &ResolveCtx<'_>,
    share_code: &str,
    is_tv: bool,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<Vec<ShareFile>, SourceError> {
    let top = list_share_files(ctx, share_code, None).await?;
    let mut files: Vec<ShareFile> = top.iter().filter(|file| !file.is_dir()).cloned().collect();
    if !files.iter().any(|file| is_video(&file.file_name())) {
        // Walk up to 5 subdirectories, one level deep — the JS's
        // `dirs.slice(0, 5)` bound (the comment says 3, the code says 5).
        for dir in top.iter().filter(|file| file.is_dir()).take(5) {
            for item in list_share_files(ctx, share_code, Some(&dir.fid())).await? {
                if !item.is_dir() {
                    files.push(item);
                }
            }
        }
    }

    let mut videos: Vec<ShareFile> = files
        .into_iter()
        .filter(|file| is_video(&file.file_name()))
        .collect();
    if is_tv && let (Some(season), Some(episode)) = (season, episode) {
        // Keep the requested episode when any file matches
        // (`if (filtered.length > 0) targetFiles = filtered`).
        let matching: Vec<ShareFile> = videos
            .iter()
            .filter(|file| episode_matches(&file.file_name(), season, episode))
            .cloned()
            .collect();
        if !matching.is_empty() {
            videos = matching;
        }
    }
    Ok(videos)
}

/// `file/file_share_list` — ports `listShareFiles` (fetch failures are
/// the JS's catch → `[]`; `code !== 1` likewise).
async fn list_share_files(
    ctx: &ResolveCtx<'_>,
    share_code: &str,
    parent_id: Option<&str>,
) -> Result<Vec<ShareFile>, SourceError> {
    let url = Url::parse(&format!("{FEBBOX_BASE}/file/file_share_list")).map_err(|error| {
        SourceError::scrape(ID, format!("invalid file_share_list URL: {error}"))
    })?;
    let mut url = url;
    url.query_pairs_mut().append_pair("share_key", share_code);
    if let Some(parent) = parent_id {
        url.query_pairs_mut().append_pair("parent_id", parent);
    }
    let request = FetchRequest::get(url)
        .with_header("Accept", "application/json")
        .with_header("Referer", format!("{FEBBOX_BASE}/share/{share_code}"))
        .with_timeout(LIST_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Ok(Vec::new());
    };
    let data: ShareListResponse = serde_json::from_str(&response.body).map_err(|error| {
        SourceError::scrape(ID, format!("invalid file_share_list payload: {error}"))
    })?;
    if data.code != 1 {
        return Ok(Vec::new());
    }
    Ok(data.data.map(|data| data.file_list).unwrap_or_default())
}

/// Qualities → streams for the first two files — ports the
/// `filesToProcess` loop. Per-file failures are skipped (the JS's
/// per-file try/catch → `[]`); URLs are deduplicated across files
/// (`seenUrls`), unparseable ones dropped.
async fn quality_streams(
    ctx: &ResolveCtx<'_>,
    share_code: &str,
    cookie: &str,
    files: &[ShareFile],
    title_base: &str,
) -> Vec<Stream> {
    let mut streams = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for file in files.iter().take(2) {
        // The filename's metadata is parsed once, shared by every
        // quality row of the file.
        let file_meta = FileNameMeta::parse(&file.file_name());
        for quality in get_video_qualities(ctx, share_code, &file.fid(), cookie).await {
            if quality.url.is_empty() || !seen.insert(quality.url.clone()) {
                continue;
            }
            let Ok(url) = Url::parse(&quality.url) else {
                continue;
            };
            let is_org = quality.quality.as_deref() == Some("ORG");
            let quality_label = if is_org {
                "Original"
            } else {
                quality.quality.as_deref().unwrap_or("HD")
            };
            let mut stream = Stream::new(
                url,
                // ORG rows are the direct MKV (mapped to the mp4 format
                // upstream), everything else is transcoded HLS.
                if is_org { Format::Mp4 } else { Format::Hls },
            )
            .with_ttl(TTL)
            .with_label(format!("{title_base} ({LABEL} {quality_label})"));
            stream.meta = StreamMeta {
                quality: file_meta.quality.clone(),
                resolution: quality.quality.as_deref().and_then(parse_height),
                codec: file_meta.codec.clone(),
                audio: file_meta
                    .audio
                    .map(|audio| vec![audio.to_string()])
                    .unwrap_or_default(),
                languages: file_meta.languages.clone(),
                size: quality.size.as_deref().and_then(parse_size),
                size_label: quality
                    .size
                    .clone()
                    .filter(|size| parse_size(size).is_none()),
                source_id: Some(ID.to_string()),
                source_label: Some(LABEL.to_string()),
                ..StreamMeta::default()
            };
            streams.push(stream);
        }
    }
    streams
}

/// `console/video_quality_list` with the `FebBox` cookie — ports
/// `getVideoQualities` (cheerio over the `html` payload); all failures
/// are the JS's catch → `[]` for that file.
async fn get_video_qualities(
    ctx: &ResolveCtx<'_>,
    share_code: &str,
    fid: &str,
    cookie: &str,
) -> Vec<Quality> {
    let Ok(url) = Url::parse(&format!("{FEBBOX_BASE}/console/video_quality_list")) else {
        return Vec::new();
    };
    let mut url = url;
    url.query_pairs_mut()
        .append_pair("fid", fid)
        .append_pair("share_key", share_code);
    let request = FetchRequest::get(url)
        .with_header("Accept", "application/json")
        .with_header("Cookie", cookie.to_string())
        .with_header("Referer", format!("{FEBBOX_BASE}/share/{share_code}"))
        .with_timeout(QUALITY_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<QualityListResponse>(&response.body) else {
        return Vec::new();
    };
    if data.code != 1 || data.html.is_empty() {
        return Vec::new();
    }
    let document = Html::parse_document(&data.html);
    document
        .select(&FILE_QUALITY)
        .map(|row| {
            let size = row
                .select(&SIZE_CELL)
                .next()
                .map(|cell| cell.text().collect::<String>().trim().to_string());
            Quality {
                url: row.value().attr("data-url").unwrap_or_default().to_string(),
                quality: row.value().attr("data-quality").map(str::to_string),
                size,
            }
        })
        .filter(|quality| !quality.url.is_empty())
        .collect()
}

/// A JSON string-or-number as a string — the JS duck-typing when ids
/// flow into query parameters.
fn loose_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

/// `\.(mkv|mp4|m4v|mov|avi|ts|webm)$` — an extension probe.
fn is_video(name: &str) -> bool {
    let extension = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "mkv" | "mp4" | "m4v" | "mov" | "avi" | "ts" | "webm"
    )
}

/// The first `[._\s-]s(\d{1,2})e(\d{1,3})[._\s-]` marker in `name` — a
/// hand-rolled scan of the upstream regex (this crate carries no regex
/// dependency); the greedy-then-backtracking digit lengths mirror the
/// regex engine.
fn episode_marker(name: &str) -> Option<(u32, u32)> {
    let text = name.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let is_separator = |byte: u8| matches!(byte, b'.' | b'_' | b' ' | b'-');
    let digits = |slice: &[u8]| slice.iter().all(u8::is_ascii_digit);
    for i in 0..bytes.len() {
        if bytes[i] != b's' || i == 0 || !is_separator(bytes[i - 1]) {
            continue;
        }
        for season_len in [2, 1] {
            if i + 1 + season_len >= bytes.len() {
                continue;
            }
            let season_digits = &bytes[i + 1..i + 1 + season_len];
            if !digits(season_digits) {
                continue;
            }
            let e_at = i + 1 + season_len;
            if bytes[e_at] != b'e' {
                continue;
            }
            for episode_len in [3, 2, 1] {
                if e_at + 1 + episode_len >= bytes.len() {
                    continue;
                }
                let episode_digits = &bytes[e_at + 1..e_at + 1 + episode_len];
                if !digits(episode_digits) {
                    continue;
                }
                let after = e_at + 1 + episode_len;
                if !is_separator(bytes[after]) {
                    continue;
                }
                let season = std::str::from_utf8(season_digits)
                    .ok()
                    .and_then(|digits| digits.parse().ok())?;
                let episode = std::str::from_utf8(episode_digits)
                    .ok()
                    .and_then(|digits| digits.parse().ok())?;
                return Some((season, episode));
            }
        }
    }
    None
}

/// `matchEpisode` — the first marker must carry the wanted season and
/// episode.
fn episode_matches(name: &str, want_season: u32, want_episode: u32) -> bool {
    episode_marker(name)
        .is_some_and(|(season, episode)| season == want_season && episode == want_episode)
}

/// Quality string → height, ports `parseHeight` (anything containing
/// `org` has no height; `4k`/`2160` and the p-sizes map directly).
fn parse_height(quality: &str) -> Option<u16> {
    let quality = quality.to_ascii_lowercase();
    if quality.contains("org") {
        return None;
    }
    if quality.contains("4k") || quality.contains("2160") {
        return Some(2160);
    }
    if quality.contains("1080") {
        return Some(1080);
    }
    if quality.contains("720") {
        return Some(720);
    }
    if quality.contains("480") {
        return Some(480);
    }
    if quality.contains("360") {
        return Some(360);
    }
    None
}

/// `([\d.]+)\s*(GB|MB)` → bytes, ports `parseSize` — integer math on the
/// integer and fractional digits (truncating like the JS's float math).
fn parse_size(size: &str) -> Option<u64> {
    let text = size.trim().to_ascii_lowercase();
    let (unit_at, multiplier) = if let Some(unit_at) = text.find("gb") {
        (unit_at, 1024 * 1024 * 1024)
    } else {
        let unit_at = text.find("mb")?;
        (unit_at, 1024 * 1024)
    };
    // The numeric run immediately before the unit (`\s*` skipped).
    let head = text[..unit_at].trim_end();
    let number: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let (int_part, fraction) = number.split_once('.').unwrap_or((&number, ""));
    let mut bytes = int_part.parse::<u64>().ok()?.checked_mul(multiplier)?;
    if !fraction.is_empty() {
        let fraction_digits = fraction.parse::<u64>().ok()?;
        let scale = u64::from(10u32).checked_pow(u32::try_from(fraction.len()).ok()?)?;
        let scaled = fraction_digits
            .checked_mul(multiplier)?
            .checked_div(scale)?;
        bytes = bytes.checked_add(scaled)?;
    }
    Some(bytes)
}

/// `first\s*second` — the `web\s*dl`-style separators.
fn spaced_pair(text: &str, first: &str, second: &str) -> bool {
    text.match_indices(first)
        .any(|(at, _)| text[at + first.len()..].trim_start().starts_with(second))
}

/// Filename-derived stream metadata — the JS's regex probes, hand-rolled
/// (substring patterns for the unanchored regexes, word tokens for the
/// `\b`-anchored ones) with the same precedence.
struct FileNameMeta {
    /// `hevc|x265|h\.?265` → HEVC, `x264|h264|avc` → AVC, with the
    /// `10…bit` probe folded in (`bitDepth` upstream).
    codec: Option<String>,
    /// `remux`/`bluRay`/`web…dl`/… source type, with the `hdr` probes
    /// folded in (`sourceType` and `hdr` upstream).
    quality: Option<String>,
    /// `truehd`/`atmos`/`dd+`/… family (`audioCodec` upstream).
    audio: Option<&'static str>,
    /// `multi` first, then every `\b(language)\b` hit in upstream order.
    languages: Vec<CountryCode>,
}

/// The `\b(language)\b` probes, in upstream order.
const LANGUAGE_WORDS: &[(&[&str], CountryCode)] = &[
    (&["hindi", "hin"], CountryCode::Hi),
    (&["english", "eng"], CountryCode::En),
    (&["japanese", "jpn"], CountryCode::Ja),
    (&["korean", "kor"], CountryCode::Ko),
    (&["tamil", "tam"], CountryCode::Ta),
    (&["telugu", "tel"], CountryCode::Te),
];

impl FileNameMeta {
    /// Probe a file name.
    fn parse(name: &str) -> Self {
        let text = name.to_ascii_lowercase();
        let tokens: Vec<&str> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect();
        let has_word = |word: &str| tokens.contains(&word);

        let codec = if text.contains("hevc")
            || text.contains("x265")
            || text.contains("h265")
            || text.contains("h.265")
        {
            Some("HEVC")
        } else if text.contains("x264") || text.contains("h264") || text.contains("avc") {
            Some("AVC")
        } else {
            None
        };
        let source_type = if text.contains("remux") {
            Some("BluRay Remux")
        } else if text.contains("bluray") || text.contains("bdrip") {
            Some("BluRay")
        } else if spaced_pair(&text, "web", "dl")
            || text.contains("web-dl")
            || text.contains("webdl")
        {
            Some("WebDL")
        } else if spaced_pair(&text, "web", "rip") || text.contains("webrip") {
            Some("WebRip")
        } else if spaced_pair(&text, "hd", "rip") || text.contains("hdrip") {
            Some("HDRip")
        } else {
            None
        };
        let hdr = if spaced_pair(&text, "dolby", "vision") || has_word("dv") {
            Some("Dolby Vision")
        } else if text.contains("hdr10+") {
            Some("HDR10+")
        } else if has_word("hdr") {
            Some("HDR")
        } else {
            None
        };
        let ten_bit =
            spaced_pair(&text, "10", "bit") || text.contains("10bit") || text.contains("10-bit");
        let audio = if text.contains("truehd") {
            Some("TrueHD")
        } else if text.contains("atmos") {
            Some("Atmos")
        } else if text.contains("dd+") || text.contains("ddp") || text.contains("eac3") {
            Some("DD+")
        } else if has_word("dd") || has_word("ac3") {
            Some("DD")
        } else if has_word("dts") {
            Some("DTS")
        } else {
            None
        };

        let mut languages = vec![CountryCode::Multi];
        for (words, code) in LANGUAGE_WORDS {
            if words.iter().any(|word| has_word(word)) {
                languages.push(*code);
            }
        }

        Self {
            codec: match (codec, ten_bit) {
                (Some(codec), true) => Some(format!("{codec} 10-bit")),
                (Some(codec), false) => Some(codec.to_string()),
                (None, true) => Some("10-bit".to_string()),
                (None, false) => None,
            },
            quality: match (source_type, hdr) {
                (Some(source), Some(hdr)) => Some(format!("{source} {hdr}")),
                (Some(source), None) => Some(source.to_string()),
                (None, Some(hdr)) => Some(hdr.to_string()),
                (None, None) => None,
            },
            audio,
            languages,
        }
    }
}

/// The mapping-proxy response.
#[derive(Deserialize)]
struct ProxyResponse {
    /// Whether the mapping succeeded.
    #[serde(default)]
    success: bool,
    /// The `ShowBox` id (string or number on the wire).
    #[serde(default)]
    id: serde_json::Value,
}

/// `index/share_link` response.
#[derive(Deserialize)]
struct ShareLinkResponse {
    /// 1 on success.
    #[serde(default)]
    code: i64,
    /// The share payload.
    #[serde(default)]
    data: Option<ShareLinkData>,
}

/// `index/share_link`'s `data`.
#[derive(Deserialize, Default)]
struct ShareLinkData {
    /// The share link (`https://www.febbox.com/s/{code}`).
    #[serde(default)]
    link: Option<String>,
}

/// `file/file_share_list` response.
#[derive(Deserialize)]
struct ShareListResponse {
    /// 1 on success.
    #[serde(default)]
    code: i64,
    /// The listing payload.
    #[serde(default)]
    data: Option<ShareListData>,
}

/// `file/file_share_list`'s `data`.
#[derive(Deserialize, Default)]
struct ShareListData {
    /// The share's entries.
    #[serde(default)]
    file_list: Vec<ShareFile>,
}

/// A share listing entry.
#[derive(Deserialize, Default, Clone)]
struct ShareFile {
    /// The file id (string or number on the wire).
    #[serde(default)]
    fid: serde_json::Value,
    /// The file name.
    #[serde(default)]
    file_name: serde_json::Value,
    /// `1` (number or string) for directories.
    #[serde(default)]
    is_dir: serde_json::Value,
}

impl ShareFile {
    /// The id as a string — the JS passes whatever JSON gave it into the
    /// query parameters.
    fn fid(&self) -> String {
        loose_string(&self.fid).unwrap_or_default()
    }

    /// The file name, empty when missing.
    fn file_name(&self) -> String {
        loose_string(&self.file_name).unwrap_or_default()
    }

    /// `is_dir === 1 || is_dir === '1'`.
    fn is_dir(&self) -> bool {
        self.is_dir.as_i64() == Some(1) || self.is_dir.as_str() == Some("1")
    }
}

/// `console/video_quality_list` response.
#[derive(Deserialize)]
struct QualityListResponse {
    /// 1 on success.
    #[serde(default)]
    code: i64,
    /// The quality table as HTML.
    #[serde(default)]
    html: String,
}

/// One `div.file_quality` row.
struct Quality {
    /// `data-url` — the direct file/HLS URL.
    url: String,
    /// `data-quality` — `ORG`/`4K`/`1080p`/…
    quality: Option<String>,
    /// The `.size` cell text (`1.4 GB`).
    size: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use url::Url;
    use vsources_core::error::{FetchError, SourceError};
    use vsources_core::tmdb::TmdbClient;
    use vsources_core::traits::{
        FetchRequest, FetchResponse, Fetcher, ResolveCtx, ResolvedMedia, Source,
    };
    use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType};

    use super::Peckle;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or `path?query`)
    /// in call order — the last body repeats — recording every request.
    /// Query-bearing lookups fall back to the bare path, so TMDB requests
    /// (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<String>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `body`; earlier registrations pop first.
        fn page(self, key: impl Into<String>, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push(body.into());
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// The requests whose lookup key is exactly `key`.
        fn requests_for(&self, key: &str) -> Vec<FetchRequest> {
            self.requests()
                .into_iter()
                .filter(|request| key_of(&request.url) == key)
                .collect()
        }

        /// A header of the first request whose lookup key is `key`.
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests_for(key).into_iter().find_map(|request| {
                request
                    .headers
                    .iter()
                    .find(|(header, _)| header.eq_ignore_ascii_case(name))
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
            let body = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let body = match body {
                Some(body) => Some(body),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(request.url.path())
                    .map(|bodies| bodies[0].clone()),
            };
            match body {
                Some(body) => Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "text/html".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    // -- fixtures ------------------------------------------------------------

    /// The provider over a TMDB client sharing the scripted fetcher,
    /// with the `FebBox` cookie.
    fn provider(mock: &Arc<ScriptedFetcher>, cookie: Option<&str>) -> Peckle {
        let tmdb = TmdbClient::new("test-key", mock.clone());
        Peckle::new(cookie.map(str::to_string), Arc::new(tmdb))
    }

    /// A context over the scripted fetcher, optionally with media.
    fn ctx_for(fetcher: &Arc<ScriptedFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = fetcher.as_ref();
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// `1.4 GB` in bytes, the way `parseSize` truncates.
    const ONE_POINT_FOUR_GB: u64 = 1024 * 1024 * 1024 + 4 * 1024 * 1024 * 1024 / 10;

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, None);
        let info = provider.info();
        assert_eq!(info.id, "peckle");
        assert_eq!(info.label, "2Peckle");
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
            Some("https://showbox.media/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_qualities_from_the_share() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/3/movie/27205", r#"{"title":"Inception","release_date":"2010-07-16"}"#)
                .page("/api/media/movie/27205", r#"{"success":true,"id":"sb-123"}"#)
                .page(
                    "/index/share_link?id=sb-123&type=1",
                    r#"{"code":1,"data":{"link":"https://www.febbox.com/s/abc123/"}}"#,
                )
                .page(
                    "/file/file_share_list?share_key=abc123",
                    r#"{"code":1,"data":{"file_list":[{"fid":"f1","file_name":"Inception.2010.1080p.BluRay.x265.10bit.Hindi.English.TrueHD.mkv","is_dir":0}]}}"#,
                )
                .page(
                    "/console/video_quality_list?fid=f1&share_key=abc123",
                    r#"{"code":1,"html":"<div class=\"file_quality\" data-url=\"https://hls.shegu.net/abc.m3u8?sign=x&amp;t=1\" data-quality=\"1080p\"><span class=\"size\">1.4 GB</span></div><div class=\"file_quality\" data-url=\"https://usa7-as05.shegu.net/vip/f/movie.mkv\" data-quality=\"ORG\"><span class=\"size\">8.7 GB</span></div>"}"#,
                ),
        );
        let provider = provider(&mock, Some("ui=jwt"));
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        assert_eq!(streams.len(), 2);
        let hls = &streams[0];
        assert_eq!(
            hls.url.as_str(),
            "https://hls.shegu.net/abc.m3u8?sign=x&t=1"
        );
        assert_eq!(hls.format, Format::Hls);
        assert_eq!(
            hls.label.as_deref(),
            Some("Inception (2010) (2Peckle 1080p)")
        );
        assert_eq!(hls.meta.resolution, Some(1080));
        assert_eq!(hls.meta.quality.as_deref(), Some("BluRay"));
        assert_eq!(hls.meta.codec.as_deref(), Some("HEVC 10-bit"));
        assert_eq!(hls.meta.audio, vec!["TrueHD".to_string()]);
        assert_eq!(
            hls.meta.languages,
            vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En]
        );
        assert_eq!(hls.meta.size, Some(ONE_POINT_FOUR_GB));
        assert_eq!(hls.meta.size_label, None);
        assert_eq!(hls.meta.source_id.as_deref(), Some("peckle"));
        assert_eq!(hls.meta.source_label.as_deref(), Some("2Peckle"));
        assert_eq!(hls.ttl, super::TTL);

        let original = &streams[1];
        assert_eq!(
            original.url.as_str(),
            "https://usa7-as05.shegu.net/vip/f/movie.mkv"
        );
        assert_eq!(original.format, Format::Mp4);
        assert_eq!(
            original.label.as_deref(),
            Some("Inception (2010) (2Peckle Original)")
        );
        assert_eq!(original.meta.resolution, None);

        // The wire requests carried the JS's headers and timeout.
        assert_eq!(
            mock.sent_header(
                "/console/video_quality_list?fid=f1&share_key=abc123",
                "Cookie"
            )
            .as_deref(),
            Some("ui=jwt")
        );
        assert_eq!(
            mock.sent_header(
                "/console/video_quality_list?fid=f1&share_key=abc123",
                "Referer"
            )
            .as_deref(),
            Some("https://www.febbox.com/share/abc123")
        );
        assert_eq!(
            mock.sent_header("/index/share_link?id=sb-123&type=1", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            mock.sent_header("/index/share_link?id=sb-123&type=1", "Referer")
                .as_deref(),
            Some("https://showbox.media/")
        );
        assert_eq!(
            mock.requests_for("/api/media/movie/27205")[0].timeout,
            Some(super::PROXY_TIMEOUT)
        );
        Ok(())
    }

    #[tokio::test]
    async fn tv_episodes_filter_by_marker() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/3/tv/1396", r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#)
                .page("/api/media/tv/1396/1/2", r#"{"success":true,"id":8841}"#)
                .page(
                    "/index/share_link?id=8841&type=2",
                    r#"{"code":1,"data":{"link":"https://www.febbox.com/s/tvcode"}}"#,
                )
                .page(
                    "/file/file_share_list?share_key=tvcode",
                    r#"{"code":1,"data":{"file_list":[{"fid":"f1","file_name":"Show.S01E01.720p.mkv","is_dir":0},{"fid":"f2","file_name":"Show.S01E02.1080p.WEB-DL.DDP5.1.mkv","is_dir":0},{"fid":"d1","file_name":"Extras","is_dir":1}]}}"#,
                )
                .page(
                    "/console/video_quality_list?fid=f2&share_key=tvcode",
                    r#"{"code":1,"html":"<div class=\"file_quality\" data-url=\"https://hls.shegu.net/tv.m3u8\" data-quality=\"1080p\"><span class=\"size\">2.1 GB</span></div>"}"#,
                ),
        );
        let provider = provider(&mock, Some("jwt123"));
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        // Only the S01E02 file's qualities were fetched (the S01E01 file
        // never reached the quality list).
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 (2Peckle 1080p)")
        );
        assert_eq!(streams[0].meta.quality.as_deref(), Some("WebDL"));
        assert_eq!(streams[0].meta.audio, vec!["DD+".to_string()]);
        assert!(
            mock.requests_for("/console/video_quality_list?fid=f1&share_key=tvcode")
                .is_empty()
        );
        // The raw cookie was normalized with the `ui=` prefix.
        assert_eq!(
            mock.sent_header(
                "/console/video_quality_list?fid=f2&share_key=tvcode",
                "Cookie"
            )
            .as_deref(),
            Some("ui=jwt123")
        );
        Ok(())
    }

    #[tokio::test]
    async fn walks_subdirectories_when_the_top_level_has_no_videos() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/api/media/movie/447", r#"{"success":true,"id":675}"#)
                .page(
                    "/index/share_link?id=675&type=1",
                    r#"{"code":1,"data":{"link":"https://www.febbox.com/s/walkcode"}}"#,
                )
                .page(
                    "/file/file_share_list?share_key=walkcode",
                    r#"{"code":1,"data":{"file_list":[{"fid":"d1","file_name":"Season 1","is_dir":"1"}]}}"#,
                )
                .page(
                    "/file/file_share_list?share_key=walkcode&parent_id=d1",
                    r#"{"code":1,"data":{"file_list":[{"fid":"f9","file_name":"Movie.2023.720p.WEBRip.x264.mkv","is_dir":0}]}}"#,
                )
                .page(
                    "/console/video_quality_list?fid=f9&share_key=walkcode",
                    r#"{"code":1,"html":"<div class=\"file_quality\" data-url=\"https://hls.shegu.net/walk.m3u8\" data-quality=\"720p\"><span class=\"size\">900 MB</span></div>"}"#,
                ),
        );
        let provider = provider(&mock, Some("ui=jwt"));
        let ctx = ctx_for(
            &mock,
            Some(ResolvedMedia {
                tmdb_id: Some(447),
                imdb_id: None,
                name: "A Movie".to_string(),
                year: Some(2023),
                season: None,
                episode: None,
            }),
        );

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(447)))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), "https://hls.shegu.net/walk.m3u8");
        assert_eq!(streams[0].meta.quality.as_deref(), Some("WebRip"));
        assert_eq!(streams[0].meta.codec.as_deref(), Some("AVC"));
        assert_eq!(streams[0].meta.resolution, Some(720));
        assert_eq!(streams[0].meta.size, Some(900 * 1024 * 1024));
        // The walk fetched the subdirectory with `parent_id`.
        assert_eq!(
            mock.requests_for("/file/file_share_list?share_key=walkcode&parent_id=d1")
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_cookie_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, None);
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing FebBox cookie must be a NotFound, got {other:?}"),
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_proxy_miss_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default().page("/api/media/movie/999", r#"{"success":false}"#),
        );
        let provider = provider(&mock, Some("ui=jwt"));
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(999)))
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("a failed proxy lookup must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_qualities_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/api/media/movie/27205", r#"{"success":true,"id":"sb-123"}"#)
                .page(
                    "/index/share_link?id=sb-123&type=1",
                    r#"{"code":1,"data":{"link":"https://www.febbox.com/s/abc123"}}"#,
                )
                .page(
                    "/file/file_share_list?share_key=abc123",
                    r#"{"code":1,"data":{"file_list":[{"fid":"f1","file_name":"Inception.2010.1080p.BluRay.x265.mkv","is_dir":0}]}}"#,
                )
                .page("/console/video_quality_list?fid=f1&share_key=abc123", r#"{"code":0}"#),
        );
        let provider = provider(&mock, Some("ui=jwt"));
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("a quality-less share must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_proxy_payload_is_a_scrape_error() {
        let mock =
            Arc::new(ScriptedFetcher::default().page("/api/media/movie/27205", "not json at all"));
        let provider = provider(&mock, Some("ui=jwt"));
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await
        {
            Err(SourceError::Scrape { .. }) => {}
            other => panic!("a malformed proxy payload must be a Scrape, got {other:?}"),
        }
    }

    #[test]
    fn parses_episode_markers() {
        let cases = [
            ("Show.S01E02.720p.WEB-DL.mkv", Some((1, 2))),
            ("show s1e2 x265", Some((1, 2))),
            ("Show.s12e034.720p", Some((12, 34))),
            // No trailing separator after the marker — no match.
            ("Show.S01E02", None),
            ("Movie.2023.1080p.mkv", None),
            ("s01e02x720", None),
        ];
        for (name, expected) in cases {
            assert_eq!(super::episode_marker(name), expected, "marker of `{name}`");
        }
    }

    #[test]
    fn parses_filename_metadata() {
        let meta = super::FileNameMeta::parse(
            "Inception.2010.1080p.BluRay.x265.10bit.Hindi.English.TrueHD.mkv",
        );
        assert_eq!(meta.codec.as_deref(), Some("HEVC 10-bit"));
        assert_eq!(meta.quality.as_deref(), Some("BluRay"));
        assert_eq!(meta.audio, Some("TrueHD"));
        assert_eq!(
            meta.languages,
            vec![CountryCode::Multi, CountryCode::Hi, CountryCode::En]
        );

        let meta = super::FileNameMeta::parse("Movie.2160p.UHD.BluRay.REMUX.HDR10+.HEVC.DV.mkv");
        assert_eq!(meta.quality.as_deref(), Some("BluRay Remux Dolby Vision"));
        assert_eq!(meta.codec.as_deref(), Some("HEVC"));
        assert_eq!(meta.languages, vec![CountryCode::Multi]);

        let meta = super::FileNameMeta::parse("Show.S01E02.480p.WEBRip.AAC2.0.H264.mkv");
        assert_eq!(meta.quality.as_deref(), Some("WebRip"));
        assert_eq!(meta.codec.as_deref(), Some("AVC"));
        assert_eq!(meta.audio, None);
    }

    #[test]
    fn normalizes_the_febbox_cookie() {
        assert_eq!(super::normalize_cookie("jwt"), Some("ui=jwt".to_string()));
        assert_eq!(
            super::normalize_cookie("ui=jwt"),
            Some("ui=jwt".to_string())
        );
        assert_eq!(
            super::normalize_cookie(" jwt123 , ui=other "),
            Some("ui=jwt123".to_string())
        );
        assert_eq!(super::normalize_cookie(" , "), None);
    }
}
