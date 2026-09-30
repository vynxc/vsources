//! `AnimeZeY`: anime episode files from the animezey Cloudflare
//! workers.
//!
//! Ports `src/source/AnimeZeY.js` + `src/nuvio/animezey.cjs` (the
//! sanitize wrapper) + `src/nuvio/animezey_orig.cjs` (the obfuscated
//! scraper) — all three folded into one module:
//!
//! 1. the wrapper resolves TMDB (title, year, original title) and
//!    bails without a season (anime-only, series-only);
//! 2. the scraper's query generator (`_generateEpisodeQueries`) walks
//!    the base names (title, original title, their pre-colon and
//!    quote/article-stripped variants) crossed with episode codes
//!    (`S01E02`, `01x02`, `1.02`, and for season 1 the bare `02` /
//!    `002` / `ep02` / `e02`), building up to 10 spaced/dotted query
//!    strings;
//! 3. each query POSTs `{q, page_token: null, page_index: 0}` to
//!    `https://{worker}/1:search` (two worker domains, rotating on
//!    429/5xx/transport failure) → `{ data: { files: [{id, name,
//!    mimeType, link, size}] } }`;
//! 4. files must be video files (MIME or extension) and pass
//!    `_isCorrectEpisode` — the title must match in the filename
//!    (`_matchesSeriesInFilename`/`_titleMatch` with noise-word
//!    filtering) and the episode marker must be the requested one
//!    (code, absolute-episode, or flat-series numbering); at most 2
//!    results are kept;
//! 5. each file's player URL comes from `<source src>` on the
//!    `?a=view` worker page (rotating on failure) or, for
//!    `/download.aspx` links (and as the terminal fallback), from the
//!    download link on the worker that issued its signature
//!    (`file`/`expiry`/`mac` query params);
//! 6. `makeStream` renders the rich emoji card (quality, size,
//!    container, codec, HDR/DV tags, audio mode, the
//!    Brazilian-fork's always-Portuguese audio flags), ranked
//!    4K-first then largest, with the worker `User-Agent`/`Referer`
//!    hotlink headers in `behaviorHints.proxyHeaders.request`;
//! 7. the sanitize wrapper strips the invisible BOM/zero-width sort
//!    tag the scraper prepends (the port never generates it) and
//!    re-parses the quality from the name (`HD` fallback);
//! 8. the raw streams flow through [`build_stream_results`]. The engine
//!    performs bounded media validation with each card's headers; this
//!    provider does not download media bodies to validate them.
//!
//! Cuts and mappings (vs. upstream):
//!
//! - The scraper's TMDB refetch is folded into the wrapper's
//!   resolution — the title, year, and original title come from
//!   `ctx.media`/[`TmdbClient`]. `romaji_title` stays empty (upstream
//!   never fills it), so `_isAnime` reduces to the CJK test on the
//!   title/original title.
//! - The `absolute_episode` computation (a TMDB `seasons` sum) is cut
//!   — the shared [`TmdbClient`] exposes no season list — so `absEp`
//!   stays `None` and the query generator / episode matcher use their
//!   upstream `None`-absEp branches (the `absEp !== null` query and
//!   matcher arms are dead code here).
//! - The scraper's movie branch is unreachable (the wrapper is
//!   series-only) and `onSettings`/`resolveSettings` have no settings
//!   source — the sort is always quality-first.
//! - The invisible sort tag (`\uFEFF`/`\u200B` watermark) is not
//!   generated — upstream's wrapper strips it anyway.
//! - One broken flat-series matcher pattern upstream
//!   (`"\\" + pad(ep,3) + "\\]"` compiles to an octal escape that can
//!   never match a real filename) is dropped; the sibling literal and
//!   lookahead patterns are ported as-is.
//! - The per-call random session UA becomes a round-robin pick
//!   (no RNG dependency); one UA per resolve, threaded like upstream.
//! - `meta.title` (the collapsed emoji card) → [`Stream::label`]; the
//!   card text is also the raw `size` string, so the `💾 X GB` line
//!   feeds `parse_size` exactly like upstream.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{BuildParams, NuvioStream, build_stream_results, with_deadline};

/// The provider id, upstream `this.id`.
const ID: &str = "animezey";
/// The display label, upstream `this.label`.
const LABEL: &str = "AnimeZeY";
/// The catalog origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://animezey.com";
/// Upstream `this.ttl`.
const TTL: Duration = Duration::from_mins(10);
/// The scraper's outer race — `callNuvioProvider`'s `timeoutMs:
/// 25000`.
const SWEEP_DEADLINE: Duration = Duration::from_secs(25);
/// One worker/page fetch (the crate's norm; upstream bounds the sweep
/// with the 25 s race only).
const TIMEOUT: Duration = Duration::from_secs(10);
/// The worker domains, upstream `WORKER_DOMAINS`.
const WORKER_DOMAINS: [&str; 2] = ["1.animezey23112022.workers.dev", "1.animezeydl.workers.dev"];
/// The download.aspx host, upstream `downloadDomain`.
const DOWNLOAD_DOMAIN: &str = "animezey16082023.animezey16082023.workers.dev";
/// The default hotlink referer (upstream's null-domain fallback).
const DEFAULT_REFERER: &str = "https://1.animezey23112022.workers.dev/";
/// The scraper's mobile UAs (`MOBILE_UAS`), rotated per resolve.
const MOBILE_UAS: [&str; 3] = [
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Linux; Android 14; Pixel 8 Pro) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
];
/// The episode-result cap, upstream `MAX_RESULTS_EPISODE`.
const MAX_RESULTS_EPISODE: usize = 2;
/// The query cap, upstream `_generateEpisodeQueries().slice(0, 10)`.
const MAX_QUERIES: usize = 10;

/// A `SxxEyy` or `NNxEE` episode marker in a filename.
static EPISODE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"s\d{2}e\d{2}|\d+x\d{2}")
        .unwrap_or_else(|e| panic!("valid episode-tag pattern: {e}"))
});

/// The tail a matched title may be followed by — upstream
/// `TITLE_END_RE`.
static TITLE_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:s\d{1,2}e\d{1,2}|\[?\d{3,4}p\]?|(?:19|20)\d{2}|ep?\s*\d+|episode\s*\d+|\[(?:dual|dub|leg|sub|pt[\-.]br|bluray|bdrip|webrip|web[\-.]dl|hdtv|x264|x265|hevc|aac|mkv|mp4|avi|wmv|mov)\]|(?:dual|dub|leg|sub|pt[\-.]br|bluray|bdrip|webrip|web[\-.]dl|hdtv|x264|x265|hevc|aac|mkv|mp4|avi|wmv|mov)|\[\d+|\s-\s\d+)")
        .unwrap_or_else(|e| panic!("valid title-end pattern: {e}"))
});

/// A digit run or a noise word between a matched title and the episode
/// tag — upstream `NOISE_WORD_RE`.
static NOISE_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\d{4}|[a-z0-9]+(?:p|k)|bluray|bdrip|webrip|web|hdtv|x264|x265|hevc|aac|mkv|mp4|avi|wmv|mov|hdr|sdr|remux|dual|dub|dublado|leg|legendado|sub|pt[\-.]?br|nf|netflix|hbo|max|hbomax|disney|disneyplus|amazon|prime|paramount|peacock|hulu|apple|appletv|star|globoplay|telecine|crunchyroll|funimation|youtube|vix|pluto|copia|copy|sample|extras?)$")
        .unwrap_or_else(|e| panic!("valid noise-word pattern: {e}"))
});

/// A word-boundary episode code (with the upstream `(?<!\d)(?!\d)`
/// guards), built per code.
static WORD_BOUNDARY: &str = r"(?<!\d){code}(?!\d)";

/// `<source src="…">` on the worker player page.
static SOURCE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<source[^>]+src=["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid source pattern: {e}"))
});

/// CJK characters (the `_isAnime` test).
static CJK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\u3040-\u30ff\u4e00-\u9fff]").unwrap_or_else(|e| panic!("valid CJK pattern: {e}"))
});

/// The `AnimeZeY` provider.
pub struct AnimeZeY {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl AnimeZeY {
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
        }
    }
}

#[async_trait]
impl Source for AnimeZeY {
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
        let original = original_name(ctx, &self.tmdb, media, tmdb_id, &name).await;
        let title = format!("{} {}", name, media.format_season_and_episode());

        // The scraper races the 25 s deadline.
        let session_ua = next_ua();
        let sweep = scrape(ctx, &name, &original, year, season, episode, session_ua);
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }

        let country_codes = vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En];
        let built = build_stream_results(&BuildParams {
            streams: &raw,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &country_codes,
            ttl: TTL,
        });

        // Liveness belongs to the engine's bounded media gate. An ordinary
        // GET here can buffer a multi-gigabyte file before resolution finishes.
        Ok(built)
    }
}

/// One search-API file row.
#[derive(Debug, Clone, Deserialize)]
struct FileRow {
    /// Worker that signed this particular download link.
    #[serde(skip)]
    origin: Option<String>,
    /// The file id (dedup key).
    #[serde(default)]
    id: Option<String>,
    /// The file name.
    #[serde(default)]
    name: Option<String>,
    /// The MIME type.
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    /// The worker file link.
    #[serde(default)]
    link: Option<String>,
    /// The size in bytes.
    #[serde(default, deserialize_with = "file_size")]
    size: Option<f64>,
}

/// Google Drive worker APIs encode byte counts as JSON strings.
fn file_size<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|v| v.is_finite() && *v >= 0.0))
}

/// The scrape state threaded through one resolve — one
/// `AnimeZeyScraper` instance upstream.
struct Scrape<'a> {
    /// The resolve context.
    ctx: &'a ResolveCtx<'a>,
    /// The session UA (upstream `sessionUA`).
    ua: &'a str,
    /// The base names (`_getBaseNames`).
    base_names: Vec<String>,
    /// Whether the content is anime (`_isAnime` — the CJK test, the
    /// romaji title is always empty).
    is_anime: bool,
    /// The release year (the year-gated queries).
    year: Option<u16>,
    season: u32,
    /// The requested episode.
    episode: u32,
    /// The current worker domain index (upstream
    /// `currentDomainIndex`).
    domain_index: usize,
}

impl Scrape<'_> {
    /// The current worker domain.
    fn domain(&self) -> &'static str {
        WORKER_DOMAINS[self.domain_index % WORKER_DOMAINS.len()]
    }

    /// Rotate to the next worker domain.
    fn rotate(&mut self) {
        self.domain_index += 1;
    }

    /// The episode search — `_searchEpisodes`: the generated queries,
    /// each sent as a `POST` with worker rotation, keeping the first video
    /// files that pass the episode matcher (≤ 2).
    async fn search_episodes(&mut self) -> Vec<FileRow> {
        let mut found: Vec<FileRow> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for query in self.episode_queries().into_iter().take(MAX_QUERIES) {
            if found.len() >= MAX_RESULTS_EPISODE {
                break;
            }
            let Some(response) = self.post_search(&query).await else {
                continue;
            };
            let files: Vec<FileRow> = response
                .pointer("/data/files")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|file| serde_json::from_value(file).ok())
                .collect();
            for mut file in files {
                if found.len() >= MAX_RESULTS_EPISODE {
                    break;
                }
                let id = file.id.clone().unwrap_or_default();
                if !id.is_empty() && !seen.insert(id) {
                    continue;
                }
                if is_video_file(&file)
                    && self.is_correct_episode(file.name.as_deref().unwrap_or(""))
                {
                    file.origin = Some(self.domain().to_string());
                    found.push(file);
                }
            }
        }
        found
    }

    /// One search POST with worker rotation — `_postSearch`: 429/5xx
    /// and transport failures rotate and retry the next domain; the
    /// body is `{q, page_token: null, page_index: 0}`.
    async fn post_search(&mut self, query: &str) -> Option<Value> {
        let body =
            serde_json::json!({ "q": query, "page_token": null, "page_index": 0 }).to_string();
        for _ in 0..WORKER_DOMAINS.len() {
            let domain = self.domain();
            let url = format!("https://{domain}/1:search");
            let request = FetchRequest::post(
                Url::parse(&url)
                    .unwrap_or_else(|e| panic!("the AnimeZeY search URL must parse: {e}")),
                body.clone(),
            )
            .with_header("accept", "*/*")
            .with_header("accept-language", "en-US,en;q=0.9")
            .with_header("content-type", "application/json")
            .with_header("Referer", url)
            .with_header("User-Agent", self.ua)
            .with_timeout(TIMEOUT);
            match self.ctx.fetcher.request(request).await {
                Ok(response) => {
                    if response.status == 429 || response.status >= 500 {
                        self.rotate();
                        continue;
                    }
                    if !response.is_success() {
                        self.rotate();
                        continue;
                    }
                    return response.json::<Value>().ok();
                }
                Err(_) => {
                    self.rotate();
                }
            }
        }
        None
    }

    /// The episode query generator — the port of
    /// `_generateEpisodeQueries`.
    fn episode_queries(&self) -> Vec<String> {
        let mut queries: Vec<String> = Vec::new();
        let base_names: Vec<&String> = self.base_names.iter().take(4).collect();
        if base_names.is_empty() {
            return queries;
        }
        let codes = search_codes(self.season, self.episode);
        let tag = format!("S{:02}E{:02}", self.season, self.episode);
        for base in &base_names {
            let variants = name_variants(base);
            queries.push(format!("{}.{}", variants.dots_raw, tag));
            queries.push(format!("{}.{}", variants.dots, tag));
            queries.push(format!("{} {}", variants.raw, tag));
            queries.push(format!("{} {}", variants.clean, tag));
        }
        if self.is_flat_series() {
            for base in &base_names {
                let variants = name_variants(base);
                queries.push(format!("{} - {:03}", variants.clean, self.episode));
                queries.push(format!("{} - {:02}", variants.clean, self.episode));
                queries.push(format!("{}.{:03}", variants.dots, self.episode));
                queries.push(format!("{}.{:02}", variants.dots, self.episode));
                queries.push(format!("{} {:03}", variants.clean, self.episode));
            }
        }
        // The absEp branch is dead here — `absolute_episode` is cut
        // (see the module docs).
        if self.is_anime && self.season > 1 {
            for base in &base_names {
                let variants = name_variants(base);
                queries.push(format!("{} - {:03}", variants.clean, self.episode));
                queries.push(format!("{} - {:02}", variants.clean, self.episode));
                queries.push(format!("{}.{:03}", variants.dots, self.episode));
                queries.push(format!("{}.{:02}", variants.dots, self.episode));
            }
        }
        if self.is_anime && self.season == 1 {
            for base in &base_names {
                let variants = name_variants(base);
                queries.push(format!("{} - {:02}", variants.clean, self.episode));
                queries.push(format!("{} - {:03}", variants.clean, self.episode));
                queries.push(format!("{} - {:02}", variants.dots, self.episode));
                queries.push(format!("{}-{:02}", variants.dots, self.episode));
            }
        }
        for base in &base_names {
            let variants = name_variants(base);
            let selected: Vec<&String> = if self.is_anime && self.season == 1 {
                codes
                    .iter()
                    .filter(|code| code.chars().all(char::is_numeric))
                    .collect()
            } else {
                codes.iter().take(4).collect()
            };
            for code in selected {
                queries.push(format!("{}.{}", variants.dots, code));
                if !code.starts_with('S') {
                    queries.push(format!("{} {}", variants.clean, code));
                }
            }
        }
        if self.year.unwrap_or(0) > 1900 {
            for base in base_names.iter().take(2) {
                let variants = name_variants(base);
                for code in codes.iter().take(2) {
                    queries.push(format!(
                        "{}.{}.{}",
                        variants.dots,
                        self.year.unwrap_or_default(),
                        code
                    ));
                }
                if self.is_anime && self.season == 1 {
                    queries.push(format!(
                        "{} {} - {:02}",
                        variants.clean,
                        self.year.unwrap_or_default(),
                        self.episode
                    ));
                }
            }
        }
        let mut seen = HashSet::new();
        queries
            .into_iter()
            .filter(|query| !query.trim().is_empty() && seen.insert(query.trim().to_string()))
            .map(|query| query.trim().to_string())
            .collect()
    }

    /// Whether the requested episode matches this file — the port of
    /// `_isCorrectEpisode`.
    fn is_correct_episode(&self, file_name: &str) -> bool {
        let lower = file_name.to_lowercase();
        let normalized = remove_accents(&lower);
        if !self.matches_series_in_filename(&lower) {
            return false;
        }
        // The direct SxxEyy / NxEE tags.
        let direct = [
            format!("s{:02}e{:02}", self.season, self.episode),
            format!("{}x{:02}", self.season, self.episode),
        ];
        for marker in direct {
            if normalized.contains(&marker) {
                return true;
            }
        }
        if EPISODE_TAG.is_match(&normalized).unwrap_or(false) {
            // Some OTHER episode tag — wrong episode.
            return false;
        }
        // Bare episode numbers are valid in first-season anime releases,
        // but never reinterpret an explicit different season as season one.
        if let Ok(season_tag) = Regex::new(r"(?i)\bs(?:eason)?[ ._-]*0*([1-9]\d*)\b")
            && let Ok(Some(captures)) = season_tag.captures(&normalized)
            && let Some(found) = captures.get(1).and_then(|m| m.as_str().parse::<u32>().ok())
            && found != self.season
        {
            return false;
        }
        // The bare codes, word-bounded.
        for code in search_codes(self.season, self.episode) {
            let pattern = WORD_BOUNDARY.replace("{code}", &escape_regex(&code));
            if let Ok(regex) = Regex::new(&pattern)
                && regex.is_match(&normalized).unwrap_or(false)
            {
                return true;
            }
        }
        // Anime season ≥ 2 with no absolute episode — the
        // space/dash/bracket episode forms.
        if self.is_anime && self.season > 1 {
            let markers = [
                format!(" - {:02}", self.episode),
                format!(" - {:03}", self.episode),
                format!("- {:02}", self.episode),
                format!("- {:03}", self.episode),
                format!(" {:03}.", self.episode),
                format!(" {:03} ", self.episode),
                format!("[{:03}]", self.episode),
            ];
            if markers.iter().any(|marker| normalized.contains(marker)) {
                return true;
            }
        }
        // The absEp branch is dead here (see the module docs).
        if self.is_flat_series() {
            // The lookahead family (not followed by a digit) plus the
            // literal bracket/dot/space forms.
            let lookaheads = [
                format!(" - {:03}(?!\\d)", self.episode),
                format!(" - {:02}(?!\\d)", self.episode),
                format!("- {:03}(?!\\d)", self.episode),
                format!("- {:02}(?!\\d)", self.episode),
            ];
            for pattern in lookaheads {
                if let Ok(regex) = Regex::new(&pattern)
                    && regex.is_match(&normalized).unwrap_or(false)
                {
                    return true;
                }
            }
            let literals = [
                format!("[{:02}]", self.episode),
                format!(" {:03}.", self.episode),
                format!(" {:02}.", self.episode),
                format!(" {:03} ", self.episode),
                format!(" {:02} ", self.episode),
            ];
            if literals.iter().any(|marker| normalized.contains(marker)) {
                return true;
            }
        }
        false
    }

    /// Whether one of the base names matches inside the filename —
    /// the port of `_matchesSeriesInFilename`.
    fn matches_series_in_filename(&self, file_name: &str) -> bool {
        let normalized_file = normalize_for_compare(file_name);
        for base in self.base_names.iter().take(8) {
            let unaccented = remove_accents(base);
            if unaccented.contains(':') {
                let parts: Vec<&str> = unaccented.split(':').map(str::trim).collect();
                if parts.iter().all(|part| {
                    part.chars().count() <= 2
                        || title_match(part, file_name)
                        || title_match(&normalize_for_compare(part), &normalized_file)
                }) {
                    return true;
                }
            } else if title_match(&unaccented, file_name)
                || title_match(&normalize_for_compare(&unaccented), &normalized_file)
            {
                return true;
            }
        }
        false
    }

    /// Whether this is a non-anime first-season series (`_isFlatSeries`).
    fn is_flat_series(&self) -> bool {
        !self.is_anime && self.season == 1
    }
}

/// The scrape entry point — `getStreams`'s series path: the episode
/// search, the player-URL extraction, and `makeStream`, ranked
/// 4K-first then largest.
async fn scrape(
    ctx: &ResolveCtx<'_>,
    title: &str,
    original: &str,
    year: Option<u16>,
    season: u32,
    episode: u32,
    ua: &str,
) -> Vec<NuvioStream> {
    let is_anime = [original, title]
        .iter()
        .any(|name| !name.is_empty() && CJK.is_match(name).unwrap_or(false));
    let base_names = base_names(title, original, is_anime);
    let mut scrape = Scrape {
        ctx,
        ua,
        base_names,
        is_anime,
        year,
        season,
        episode,
        domain_index: 0,
    };

    let files = scrape.search_episodes().await;
    if files.is_empty() {
        return Vec::new();
    }

    // The player URLs, deduped.
    let mut seen: HashSet<String> = HashSet::new();
    let mut cards: Vec<(FileRow, String)> = Vec::new();
    for file in files {
        let link = file.link.clone().unwrap_or_default();
        if link.is_empty() {
            continue;
        }
        let Some(url) = extract_player_url(&mut scrape, &link, file.origin.as_deref()).await else {
            continue;
        };
        if seen.insert(url.clone()) {
            cards.push((file, url));
        }
    }

    // `makeStream` + the 4K-first, then largest sort.
    let mut ranked: Vec<(u32, u64, NuvioStream)> = cards
        .into_iter()
        .map(|(file, url)| {
            let season_tag = format!("S{:02}E{:02}", scrape.season, scrape.episode);
            let stream = make_stream(
                file.name.as_deref().unwrap_or("AnimeZeY Stream"),
                &url,
                file.size,
                scrape.ua,
                &season_tag,
                title,
                scrape.year,
                scrape.is_anime,
                file.origin.as_deref().unwrap_or_else(|| scrape.domain()),
            );
            let quality = stream.quality.clone().unwrap_or_default();
            let size = stream
                .size
                .clone()
                .and_then(|size| parse_size_mb(&size))
                .unwrap_or(0);
            (quality_rank(&quality), size, stream)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    ranked.into_iter().map(|(_, _, stream)| stream).collect()
}

/// The player URL for one file — the port of `_extractPlayerUrl`:
/// `download.aspx` links rebuild the download URL directly, other
/// links load the `?a=view` worker page for its `<source src>`
/// (rotating on failure, with the download URL as the terminal
/// fallback).
async fn extract_player_url(
    scrape: &mut Scrape<'_>,
    link: &str,
    origin: Option<&str>,
) -> Option<String> {
    // These links already identify the signed media download. Do not fetch
    // them as HTML: the server may ignore a=view and send the entire file.
    if link.starts_with("/download.aspx?") {
        let base = Url::parse(&format!(
            "https://{}/",
            origin.unwrap_or_else(|| scrape.domain())
        ))
        .ok()?;
        return base.join(link).ok().map(|url| url.to_string());
    }

    if link.contains("/download.aspx") {
        return build_download_link(link);
    }
    for _ in 0..WORKER_DOMAINS.len() {
        let domain = scrape.domain();
        let mut url = format!("https://{domain}{link}");
        if !url.contains("a=view") {
            url.push_str(if url.contains('?') {
                "&a=view"
            } else {
                "?a=view"
            });
        }
        let request = FetchRequest::get(
            Url::parse(&url).unwrap_or_else(|e| panic!("the AnimeZeY player URL must parse: {e}")),
        )
        .with_header("User-Agent", scrape.ua)
        .with_header("Accept", "text/html,application/xhtml+xml")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("Referer", format!("https://{domain}/"))
        .with_timeout(TIMEOUT);
        match scrape.ctx.fetcher.request(request).await {
            Ok(response) => {
                if response.status == 429 || response.status >= 500 {
                    scrape.rotate();
                    continue;
                }
                if !response.is_success() {
                    scrape.rotate();
                    continue;
                }
                // The first successful page decides — a missing
                // `<source>` falls through to the download link.
                if let Some(source) = SOURCE_TAG
                    .captures(&response.body)
                    .ok()
                    .flatten()
                    .and_then(|captures| captures.get(1))
                    .map(|group| group.as_str().to_string())
                {
                    return Some(source);
                }
                break;
            }
            Err(_) => {
                scrape.rotate();
            }
        }
    }
    build_download_link(link)
}

/// The legacy fallback `download.aspx` URL — `_buildDownloadLink`: the
/// `file` param (plus `expiry`/`mac` when present) rebuilt against
/// the download worker.
fn build_download_link(link: &str) -> Option<String> {
    if !link.starts_with('/') {
        return None;
    }
    let (path, query) = match link.find('?') {
        Some(index) => (&link[..index], &link[index + 1..]),
        None => (link, ""),
    };
    let mut file: Option<String> = None;
    let mut expiry: Option<String> = None;
    let mut mac: Option<String> = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "file" => file = Some(value.into_owned()),
            "expiry" => expiry = Some(value.into_owned()),
            "mac" => mac = Some(value.into_owned()),
            _ => {}
        }
    }
    let file = file?;
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("file", &file);
    if let Some(expiry) = expiry {
        serializer.append_pair("expiry", &expiry);
    }
    if let Some(mac) = mac {
        serializer.append_pair("mac", &mac);
    }
    Some(format!(
        "https://{DOWNLOAD_DOMAIN}{path}?{}",
        serializer.finish()
    ))
}

/// The raw card — the port of `makeStream` (plus the sanitize
/// wrapper's quality re-parse and whitespace collapse; the invisible
/// sort tag is never generated).
#[allow(clippy::too_many_arguments)]
fn make_stream(
    file_name: &str,
    url: &str,
    size_bytes: Option<f64>,
    ua: &str,
    season_tag: &str,
    anime_title: &str,
    year: Option<u16>,
    is_anime: bool,
    domain: &str,
) -> NuvioStream {
    // The display name: entities decoded, newlines/tabs dropped.
    let name_line = decode_entities(file_name)
        .replace(['\n', '\t'], "")
        .trim()
        .to_string();
    let facts = card_facts(&name_line, url, size_bytes, is_anime);

    // The card lines (the trailing `| ` oddities are upstream's).
    let year_text = year.map_or_else(|| "2026".to_string(), |year| year.to_string());
    let header = if season_tag.is_empty() {
        format!("🍿 {anime_title} - {year_text}")
    } else {
        format!("🍿 {anime_title} - {year_text} | {season_tag}")
    };
    let title = [
        header,
        format!(
            "{} {} | 💾 {} | 🎞️ {}",
            facts.fire,
            facts.quality,
            facts.size_str,
            container_of(url)
        ),
        format!("{}⚡ {} | ", facts.hdr_prefix, facts.codec),
        format!("🌍 {} | 🎧 {}{}", facts.audio_mode, facts.audio, facts.dv),
        format!("🗣️ {} | ", facts.audio_flags),
        format!("🔗 AnimeZeY Server | 🕸️ {}", facts.source_type),
    ]
    .join("\n");
    // The sanitize wrapper's whitespace collapse.
    let title = collapse_whitespace(&title);

    let referer = if domain.is_empty() {
        DEFAULT_REFERER.to_string()
    } else {
        format!("https://{domain}/")
    };

    let mut stream = NuvioStream::new(url.to_string());
    stream.name = Some(format!(
        "AnimeZeY | {} | {}",
        facts.quality, facts.audio_mode
    ));
    stream.title = Some(title.clone());
    stream.quality = Some(facts.quality);
    stream.size = Some(title);
    stream.kind = Some("video/mp4".to_string());
    stream.behavior_hints = Some(serde_json::json!({
        "notWebReady": true,
        "proxyHeaders": { "request": { "User-Agent": ua, "Referer": referer } },
    }));
    stream
}

/// The card's derived facts — the `makeStream` classification block:
/// every tag the card lines are assembled from.
struct CardFacts {
    /// The quality tier.
    quality: String,
    /// The human size (the `💾` line).
    size_str: String,
    /// The fire emoji (`🌟` for 4K).
    fire: &'static str,
    /// The source type label.
    source_type: &'static str,
    /// The codec label.
    codec: &'static str,
    /// The HDR prefix line (empty when untagged).
    hdr_prefix: String,
    /// The Dolby Vision suffix (empty when untagged).
    dv: &'static str,
    /// The audio label (with the Atmos suffix when tagged).
    audio: String,
    /// The audio mode label.
    audio_mode: &'static str,
    /// The audio flags line.
    audio_flags: &'static str,
}

/// The classification block of `makeStream` — the quality tier, the
/// source type, the codec, the HDR/DV tags, and the audio tier,
/// derived from the display name, the file URL, and the byte size.
fn card_facts(name_line: &str, url: &str, size_bytes: Option<f64>, is_anime: bool) -> CardFacts {
    let lower_name = name_line.to_lowercase();
    let lower_url = url.to_lowercase();
    let size_str = format_size(size_bytes);
    let size_mb = size_mb(size_bytes);

    // The quality (the scraper's tiers — the wrapper's re-parse of
    // the name composes to the same value).
    let quality = parse_quality(name_line);
    let is_4k = quality == "2160p" || lower_name.contains("4k");
    let fire = if is_4k { "🌟" } else { "🔥" };

    // The source type.
    let source_type = if word_test(&lower_name, &["bluray", "blu-ray", "bdrip"]) {
        "BluRay"
    } else if word_test(&lower_name, &["hdrip", "webrip"]) {
        "WEBRip"
    } else {
        "WEB-DL"
    };

    // The codec.
    let codec = if word_test(&lower_name, &["x265", "h265"]) || lower_url.contains("x265") {
        "H.265"
    } else if word_test(&lower_name, &["hevc"]) || lower_url.contains("hevc") || is_4k {
        "HEVC"
    } else {
        "H.264"
    };

    // The HDR tag.
    let hdr = if word_test(&lower_name, &["hdr10plus", "hdr10+"]) {
        Some("HDR10+")
    } else if word_test(&lower_name, &["hdr10"]) {
        Some("HDR10")
    } else if word_test(&lower_name, &["hdr"]) {
        Some("HDR")
    } else if word_test(&lower_name, &["10bit", "10-bit"]) {
        Some("10Bit")
    } else {
        None
    };
    let hdr_prefix = hdr.map(|hdr| format!("🌈 {hdr} | ")).unwrap_or_default();

    // Dolby Vision.
    let dv =
        if word_test(&lower_name, &["dolby vision", "dovi", "dv"]) || lower_url.contains("dovi") {
            " | 👁️ DV"
        } else {
            ""
        };

    // The audio label.
    let mut audio = if word_test(&lower_name, &["ddp5.1"]) {
        "DDP5.1".to_string()
    } else if size_str != "N/A" && size_mb < 1300 {
        "Stereo".to_string()
    } else {
        "DD5.1".to_string()
    };
    if word_test(&lower_name, &["atmos"]) || lower_url.contains("atmos") {
        if audio == "DDP5.1" {
            audio = "DDP5.1 • 🔊 Atmos".to_string();
        } else {
            audio = "DD5.1 • 🔊 Atmos".to_string();
        }
    }

    // The audio mode and flags (the Brazilian fork's always-Portuguese
    // flags, verbatim).
    let dual = word_test(
        &lower_name,
        &["dual", "multi", "dubbed", "legendado", "dublado"],
    ) || lower_url.contains("dual");
    let audio_mode = if dual { "Dual-Audio" } else { "Single Audio" };
    let audio_flags = if dual {
        if is_anime {
            "Portuguese 🇧🇷 • Japanese 🇯🇵"
        } else {
            "English 🇺🇸 • Portuguese 🇧🇷"
        }
    } else {
        "Portuguese 🇧🇷"
    };

    CardFacts {
        quality,
        size_str,
        fire,
        source_type,
        codec,
        hdr_prefix,
        dv,
        audio,
        audio_mode,
        audio_flags,
    }
}

/// The size in whole MB — `Math.floor(bytes / 1048576)`, 0 when the
/// size is missing or empty.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // the JS floor port
fn size_mb(size_bytes: Option<f64>) -> u64 {
    match size_bytes.filter(|bytes| *bytes > 0.0) {
        Some(bytes) => (bytes / 1_048_576.0).floor() as u64,
        None => 0,
    }
}

/// The container label — the URL path's `.mp4` test (upstream's
/// case-sensitive compare, verbatim).
#[allow(clippy::case_sensitive_file_extension_comparisons)] // upstream tests `.mp4` exactly
fn container_of(url: &str) -> &'static str {
    if url.split('?').next().unwrap_or(url).ends_with(".mp4") {
        "MP4"
    } else {
        "MKV"
    }
}

/// The base names — the port of `_getBaseNames`: the title ordering
/// (romaji-first for anime — always empty here), the pre-colon parts,
/// the no-quote variants, and the article-stripped variants, deduped.
fn base_names(title: &str, original: &str, is_anime: bool) -> Vec<String> {
    let ordered: Vec<&str> = if is_anime {
        vec!["", original, title]
    } else {
        vec![title, original, ""]
    };
    let mut raw: Vec<String> = Vec::new();
    for name in ordered {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !raw.contains(&trimmed.to_string()) {
            raw.push(trimmed.to_string());
        }
        if let Some((before, _)) = trimmed.split_once(':')
            && !before.trim().is_empty()
            && !raw.contains(&before.trim().to_string())
        {
            raw.push(before.trim().to_string());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for name in raw {
        if !out.contains(&name) {
            out.push(name.clone());
        }
        if name.contains('\'') && !out.contains(&name.replace('\'', "")) {
            out.push(name.replace('\'', ""));
        }
        if !name.contains(':') {
            let lower = name.to_lowercase();
            for article in ["the ", "a ", "an ", "o ", "os ", "as "] {
                if let Some(rest) = lower.strip_prefix(article)
                    && !rest.is_empty()
                {
                    let stripped = name[article.len()..].to_string();
                    if !out.contains(&stripped) {
                        out.push(stripped);
                    }
                    break;
                }
            }
        }
    }
    out
}

/// The episode search codes — the port of `getAnimeSearchCodes`:
/// `S01E02`, `01x02`, `1.02` per pattern, plus the bare `02`/`002`/
/// `ep02`/`e02` for season 1 (and the second-season remap for late
/// season-1 episodes, 12/13+).
fn search_codes(season: u32, episode: u32) -> Vec<String> {
    let mut patterns: Vec<(u32, u32)> = vec![(season, episode)];
    if season == 1 && episode > 11 {
        for split in [12, 13] {
            if episode > split {
                patterns.push((2, episode - split));
            }
        }
    }
    let mut codes: Vec<String> = Vec::new();
    for (season, episode) in patterns {
        let mut push = |code: String| {
            if !codes.contains(&code) {
                codes.push(code);
            }
        };
        push(format!("S{season:02}E{episode:02}"));
        push(format!("{season:02}x{episode:02}"));
        push(format!("{season}.{episode:02}"));
        if season == 1 {
            push(format!("{episode:02}"));
            push(format!("{episode:03}"));
            push(format!("ep{episode:02}"));
            push(format!("e{episode:02}"));
        }
    }
    codes
}

/// The name variants — the port of the query generator's variant
/// helper: `clean` (accents + `['".:]` stripped, ` - ` spaced),
/// `dots`, `raw` (no accent strip), `dots_raw`.
struct NameVariants {
    /// Accents + punctuation stripped, spaces kept.
    clean: String,
    /// `clean` with dots.
    dots: String,
    /// Punctuation stripped only.
    raw: String,
    /// `raw` with dots.
    dots_raw: String,
}

/// Compute the four variants of a base name.
fn name_variants(name: &str) -> NameVariants {
    let strip = |name: &str| {
        name.chars()
            .filter(|c| !matches!(c, '\'' | '"' | '.' | ':'))
            .collect::<String>()
    };
    let spaced = |name: &str| space_dashes(name).trim().to_string();
    let clean = spaced(&remove_accents(&strip(name)));
    let raw = spaced(&strip(name));
    NameVariants {
        dots: clean.replace(' ', "."),
        dots_raw: raw.replace(' ', "."),
        clean,
        raw,
    }
}

/// Replace ` - ` (whitespace-dash-whitespace) with a single space.
fn space_dashes(name: &str) -> String {
    let mut out = String::new();
    let mut chars = name.char_indices().peekable();
    while let Some((_, char)) = chars.next() {
        if char == '-' {
            // Look back/forward for whitespace runs.
            let trailing_ws = out.chars().last().is_some_and(char::is_whitespace);
            let leading_ws = chars.peek().is_some_and(|(_, next)| next.is_whitespace());
            if trailing_ws && leading_ws {
                // Consume the whitespace run after the dash.
                while let Some(&(_, next)) = chars.peek() {
                    if next.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(' ');
                continue;
            }
        }
        out.push(char);
    }
    out
}

/// Whether a file row is a video — `_isVideoFile`.
fn is_video_file(file: &FileRow) -> bool {
    let name = file.name.as_deref().unwrap_or_default().to_lowercase();
    file.mime_type
        .as_deref()
        .is_some_and(|mime| mime.contains("video"))
        || [".mp4", ".mkv", ".avi", ".mov", ".wmv", ".flv", ".webm"]
            .iter()
            .any(|ext| name.ends_with(ext))
}

/// The quality label — the scraper's `parseQuality` tiers (`1080p`
/// default; the wrapper's extra 1440p/360p/240p tiers compose to the
/// same value on these outputs).
fn parse_quality(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("2160p") || lower.contains("4k") || lower.contains("uhd") {
        return "2160p".to_string();
    }
    if lower.contains("1080p") || lower.contains("fullhd") || lower.contains("full hd") {
        return "1080p".to_string();
    }
    if lower.contains("720p") {
        return "720p".to_string();
    }
    if ["dvdrip", "sd", "480p", "tvrip"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return "480p".to_string();
    }
    "1080p".to_string()
}

/// The quality rank — `getQualityRank`.
fn quality_rank(quality: &str) -> u32 {
    let lower = quality.to_lowercase();
    if lower.contains("2160") || lower.contains("4k") || lower.contains("uhd") {
        return 4;
    }
    if lower.contains("1080") || lower.contains("fullhd") || lower.contains("fhd") {
        return 3;
    }
    if lower.contains("720") || lower.contains("hd") {
        return 2;
    }
    if lower.contains("480") || lower.contains("sd") || lower.contains("dvdrip") {
        return 1;
    }
    0
}

/// The human size — `formatSize` (`N/A` for missing sizes).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // the JS floor port
fn format_size(size: Option<f64>) -> String {
    let Some(size) = size.filter(|size| size.is_finite() && *size > 0.0) else {
        return "N/A".to_string();
    };
    if size < 1024.0 {
        format!("{} B", size as u64)
    } else if size < 1_048_576.0 {
        format!("{:.2} KB", size / 1024.0)
    } else if size < 1_073_741_824.0 {
        format!("{:.2} MB", size / 1_048_576.0)
    } else {
        format!("{:.2} GB", size / 1_073_741_824.0)
    }
}

/// `"700.00 MB"` → 700 — the `sizeInMB` parse of the card's size
/// string.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // the JS floor port
fn parse_size_mb(size: &str) -> Option<u64> {
    let lower = size.to_lowercase();
    let (number, unit) = lower
        .rsplit_once(' ')
        .filter(|(number, _)| number.chars().any(|c| c.is_ascii_digit()))
        .map(|(number, unit)| (number.to_string(), unit.to_string()))?;
    let value: f64 = number.parse().ok()?;
    match unit.as_str() {
        "gb" => Some((value * 1024.0).floor() as u64),
        "mb" => Some(value.floor() as u64),
        _ => None,
    }
}

/// The title matcher — the port of `_titleMatch`: a word-bounded
/// occurrence of the base name whose remainder is a title end (or
/// only noise words before an episode tag) and whose prefix is empty
/// or ignorable.
fn title_match(base: &str, file_name: &str) -> bool {
    title_match_inner(base, file_name)
        || file_name
            .trim_start()
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .is_some_and(|(_, rest)| title_match_inner(base, rest.trim_start()))
}

fn title_match_inner(base: &str, file_name: &str) -> bool {
    let needle = normalize_title_match(base);
    let haystack = normalize_title_match(file_name);
    if needle.is_empty() {
        return false;
    }
    let pattern = format!(r"(?<![a-z0-9]){}(?=[^a-z0-9]|$)", escape_regex(&needle));
    let Ok(regex) = Regex::new(&pattern) else {
        return false;
    };
    let haystack_tagged = EPISODE_TAG.is_match(&haystack).unwrap_or(false);
    for captures in regex.captures_iter(&haystack).flatten() {
        let Some(group) = captures.get(0) else {
            continue;
        };
        let remainder = haystack[group.end()..].trim();
        let mut is_end = remainder.is_empty()
            || TITLE_END.is_match(remainder).unwrap_or(false)
            || remainder.chars().next().is_some_and(|first| {
                first.is_ascii_digit()
                    || matches!(first, '-' | '–' | '—')
                        && remainder
                            .chars()
                            .nth(1)
                            .is_none_or(|next| next.is_whitespace() || next.is_ascii_digit())
            });
        if !is_end
            && haystack_tagged
            && let Some(tag) = EPISODE_TAG.find(remainder).ok().flatten()
        {
            // The words between the title and the episode tag must be
            // noise words only.
            let between = &remainder[..tag.start()];
            let meaningful = between
                .split_whitespace()
                .filter(|word| !NOISE_WORD.is_match(word).unwrap_or(false))
                .count();
            if meaningful == 0 {
                is_end = true;
            }
        }
        if !is_end {
            continue;
        }
        let prefix = haystack[..group.start()].trim();
        if prefix.is_empty() {
            return true;
        }
        let meaningful = prefix
            .split_whitespace()
            .filter(|word| {
                !NOISE_WORD.is_match(word).unwrap_or(false) && !IGNORABLE_PREFIX.contains(word)
            })
            .count();
        if meaningful == 0 {
            return true;
        }
    }
    false
}

/// The ignorable leading words — upstream `IGNORABLE_PREFIX_WORDS`.
static IGNORABLE_PREFIX: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "the", "a", "an", "o", "os", "as", "de", "do", "da", "dos", "das", "em", "no", "na", "nos",
        "nas", "um", "uma",
    ]
    .into_iter()
    .collect()
});

/// The title-match normalizer — `_normalizeFn`: lowercase, accents
/// stripped, `.\-_+,:` and brackets spaced, whitespace collapsed.
fn normalize_title_match(value: &str) -> String {
    let unaccented = remove_accents(&value.to_lowercase());
    let spaced: String = unaccented
        .chars()
        .map(|c| {
            if matches!(
                c,
                '.' | '-' | '_' | '+' | ',' | ':' | '[' | ']' | '(' | ')' | '{' | '}'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `normalizeForCompare`: lowercase, accents stripped, non-alphanumerics
/// dropped.
fn normalize_for_compare(value: &str) -> String {
    remove_accents(&value.to_lowercase())
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// Strip combining marks — `removeAccents` (no `NFKD`; precomposed
/// accents survive, matching the crate's other anime ports).
fn remove_accents(value: &str) -> String {
    value
        .chars()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .collect()
}

/// Whether any of the needles appears as a whitespace-delimited word
/// (the JS `\b(word)\b` regexes).
fn word_test(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| {
        let pattern = format!(r"\b{}\b", escape_regex(needle));
        Regex::new(&pattern).is_ok_and(|regex| regex.is_match(haystack).unwrap_or(false))
    })
}

/// Escape a regex (the JS `escapeRegExp`).
fn escape_regex(value: &str) -> String {
    let mut out = String::new();
    for char in value.chars() {
        if matches!(
            char,
            '.' | '+' | '?' | '*' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(char);
    }
    out
}

/// Decode the HTML entities the scraper handles — `decodeEntities`.
fn decode_entities(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let known = [
            ("&nbsp;", " "),
            ("&amp;", "&"),
            ("&quot;", "\""),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&#038;", "&"),
        ]
        .iter()
        .find(|(entity, _)| tail.starts_with(entity));
        if let Some((entity, replacement)) = known {
            out.push_str(replacement);
            rest = &tail[entity.len()..];
            continue;
        }
        if let Some((digits, end)) = numeric_entity(tail) {
            if let Some(char) = char::from_u32(digits) {
                out.push(char);
            }
            rest = &tail[end..];
            continue;
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// `&#123;` → `(123, 5)`.
fn numeric_entity(tail: &str) -> Option<(u32, usize)> {
    let rest = tail.strip_prefix("&#")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let end = 2 + digits.len() + usize::from(tail[2 + digits.len()..].starts_with(';'));
    Some((digits.parse().ok()?, end))
}

/// Collapse all whitespace runs into single spaces — the sanitize
/// wrapper's `\s+` → `" "`.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The next session UA — a round-robin over the three upstream UAs
/// (upstream picks randomly; see the module docs).
fn next_ua() -> &'static str {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CURSOR: AtomicUsize = AtomicUsize::new(0);
    let index = CURSOR.fetch_add(1, Ordering::Relaxed);
    MOBILE_UAS[index % MOBILE_UAS.len()]
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

/// The original (romaji) title — the TMDB `original_name` the scraper
/// read, from the same resolution (best-effort like upstream's
/// `original_title`).
async fn original_name(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
    fallback: &str,
) -> String {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return resolved.name.clone();
    }
    tmdb.name_and_year(tmdb_id, media.kind, None)
        .await
        .ok()
        .and_then(|name| name.original_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_string())
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
    use vsources_core::types::Format;

    use super::*;

    /// A fetcher serving canned `(status, body)` keyed by
    /// `host + path` (or `host + path?query`) — later registrations
    /// REPLACE earlier ones — recording every request. Query-bearing
    /// lookups fall back to the bare `host + path`, so TMDB requests
    /// (`?api_key=…`) are scripted by host + path alone; the
    /// host-qualified keys keep the two worker domains separable.
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

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// The first request whose URL contains `needle`.
        fn first_request(&self, needle: &str) -> Option<FetchRequest> {
            self.requests()
                .into_iter()
                .find(|request| request.url.as_str().contains(needle))
        }

        /// A header of the first request whose URL contains `needle`.
        fn sent_header(&self, needle: &str, name: &str) -> Option<String> {
            self.first_request(needle).and_then(|request| {
                request
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone())
            })
        }

        /// The body of the first request whose URL contains `needle`.
        fn sent_body(&self, needle: &str) -> Option<String> {
            self.first_request(needle).and_then(|request| request.body)
        }

        /// How many requests hit a URL containing `needle`.
        fn hits(&self, needle: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request.url.as_str().contains(needle))
                .count()
        }
    }

    /// The lookup key of a URL: `host + path?query` when a query is
    /// present.
    fn key_of(url: &Url) -> String {
        let host = url.host_str().unwrap_or_default();
        match url.query() {
            Some(query) => format!("{host}{}?{query}", url.path()),
            None => format!("{host}{}", url.path()),
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
                pages.get_mut(&key).cloned()
            };
            let entry = entry.or_else(|| {
                let host = request.url.host_str().unwrap_or_default().to_string();
                let bare = format!("{host}{}", request.url.path());
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&bare).cloned()
            });
            let Some(entries) = entry else {
                return Err(FetchError::NotFound { url: request.url });
            };
            let (status, body) = entries[0].clone();
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    /// The fixture media: Frieren S01E02.
    const TMDB_ID: u64 = 209_867;
    /// The first worker's search endpoint (host-qualified).
    const SEARCH_1: &str = "1.animezey23112022.workers.dev/1:search";
    /// The second worker's search endpoint.
    const SEARCH_2: &str = "1.animezeydl.workers.dev/1:search";
    /// The first worker's file page.
    const FILE_1: &str = "1.animezey23112022.workers.dev/file/f1?a=view";
    /// The direct stream URL the player page serves.
    const STREAM_URL: &str = "https://file-cdn.example/animezey/frieren-02.mkv";

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> AnimeZeY {
        AnimeZeY::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// One search-API file row as JSON.
    fn file_row(id: &str, name: &str, link: &str, size: f64) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "mimeType": "video/x-matroska",
            "link": link,
            "size": size,
        })
    }

    /// The TMDB detail page.
    fn tmdb_page() -> String {
        r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29","original_name":"Sousou no Frieren"}"#.to_string()
    }

    /// The shared happy-path pages: TMDB, the search (one matching
    /// file), and the worker player page. The media fixture must stay unused.
    fn base_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("api.themoviedb.org/3/tv/{TMDB_ID}"),
            200,
            tmdb_page(),
        )
        .page(
            SEARCH_1,
            200,
            serde_json::json!({
                "data": { "files": [file_row("f1", "Frieren 02.mkv", "/file/f1", 734_003_200.0)] }
            })
            .to_string(),
        )
        .page(
            FILE_1,
            200,
            format!(r#"<video controls><source src="{STREAM_URL}" type="video/mp4"></video>"#),
        )
        .page("file-cdn.example/animezey/frieren-02.mkv", 200, "")
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "animezey");
        assert_eq!(info.label, "AnimeZeY");
        assert_eq!(info.content_types, vec![MediaType::Series]);
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://animezey.com/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_an_episode_file_through_the_worker() -> Result<(), SourceError> {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url.as_str(), STREAM_URL);
        assert_eq!(streams[0].format, Format::Mp4);
        // The card's quality (`1080p` default) and size line.
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(streams[0].meta.size, Some(734_003_200));
        // The worker hotlink headers ride the card.
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://1.animezey23112022.workers.dev/")
        );
        let ua = streams[0]
            .meta
            .request_headers
            .get("User-Agent")
            .cloned()
            .unwrap_or_default();
        assert!(
            MOBILE_UAS.iter().any(|candidate| *candidate == ua),
            "the session UA is one of the upstream mobile UAs"
        );
        // The label carries the wrapper title and the rich card.
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
                .is_some_and(|label| label.contains("🍿") && label.contains("💾 700.00 MB"))
        );
        // The Brazilian-fork's always-Portuguese audio flags ride the
        // card text (the upstream quirk, verbatim).
        assert!(streams[0].meta.languages.contains(&CountryCode::Multi));
        assert!(streams[0].meta.languages.contains(&CountryCode::Pt));
        assert_eq!(streams[0].ttl, TTL);
        assert_eq!(streams[0].meta.source_id.as_deref(), Some("animezey"));
        // The search POST carried the paged body and the query.
        let body = mock.sent_body("/1:search").unwrap_or_default();
        assert!(body.contains("\"page_token\":null"));
        assert!(body.contains("\"page_index\":0"));
        assert!(body.contains("Frieren.Beyond.Journeys.End.S01E02"));
        // The player page fetch carried the worker referer; the provider
        // leaves media validation to the engine.
        assert_eq!(
            mock.sent_header("/file/f1", "Referer").as_deref(),
            Some("https://1.animezey23112022.workers.dev/")
        );
        assert_eq!(
            mock.hits("file-cdn.example"),
            0,
            "provider must not download media to validate it"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_rate_limited_worker_rotates_to_the_second_domain() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(format!("api.themoviedb.org/3/tv/{TMDB_ID}"), 200, tmdb_page())
                .page(SEARCH_1, 429, "rate limited")
                .page(
                    SEARCH_2,
                    200,
                    serde_json::json!({
                        "data": { "files": [file_row("f1", "Frieren 02.mkv", "/file/f1", 734_003_200.0)] }
                    })
                    .to_string(),
                )
                .page(
                    "1.animezeydl.workers.dev/file/f1?a=view",
                    200,
                    format!(r#"<video><source src="{STREAM_URL}" type="video/mp4"></video>"#),
                )
                .page("file-cdn.example/animezey/frieren-02.mkv", 200, ""),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        // The hotlink referer is the second worker (the rotation won).
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://1.animezeydl.workers.dev/")
        );
        // Exactly one search hit the rate-limited worker before the
        // rotation won (the remaining queries stay on the second
        // worker — the single unique file never fills the 2-result
        // cap, so the query loop runs them all, like upstream).
        assert_eq!(mock.hits(SEARCH_1), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_download_aspx_link_rebuilds_the_download_url() -> Result<(), SourceError> {
        // The file row's link is a download.aspx URL — no player page
        // is fetched, the download worker link is built directly.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("api.themoviedb.org/3/tv/{TMDB_ID}"),
                    200,
                    tmdb_page(),
                )
                .page(
                    SEARCH_1,
                    200,
                    serde_json::json!({
                        "data": { "files": [file_row(
                            "f1",
                            "Frieren 02.mkv",
                            "/download.aspx?file=abc123&expiry=98765&mac=mmm",
                            734_003_200.0
                        )] }
                    })
                    .to_string(),
                )
                .page(
                    "animezey16082023.animezey16082023.workers.dev/download.aspx",
                    200,
                    "",
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
            "https://1.animezey23112022.workers.dev/download.aspx?file=abc123&expiry=98765&mac=mmm"
        );
        // No player page was fetched for the download link.
        assert_eq!(mock.hits("a=view"), 0);
        Ok(())
    }

    #[tokio::test]
    async fn media_validation_is_left_to_the_bounded_engine_gate() -> Result<(), SourceError> {
        // Both discovered files survive. Their media fixtures would return
        // 500 and 416, but the provider must not fetch either body.
        let dead_url = "https://file-cdn.example/animezey/frieren-02-alt.mkv";
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("api.themoviedb.org/3/tv/{TMDB_ID}"),
                    200,
                    tmdb_page(),
                )
                .page(
                    SEARCH_1,
                    200,
                    serde_json::json!({
                        "data": { "files": [
                            file_row("f1", "Frieren 02.mkv", "/file/f1", 734_003_200.0),
                            file_row("f2", "Frieren 02 alt.mkv", "/file/f2", 1_073_741_824.0),
                        ] }
                    })
                    .to_string(),
                )
                .page(
                    FILE_1,
                    200,
                    format!(r#"<video><source src="{STREAM_URL}" type="video/mp4"></video>"#),
                )
                .page(
                    "1.animezey23112022.workers.dev/file/f2?a=view",
                    200,
                    format!(r#"<video><source src="{dead_url}" type="video/mp4"></video>"#),
                )
                .page("file-cdn.example/animezey/frieren-02.mkv", 416, "")
                .page(
                    "file-cdn.example/animezey/frieren-02-alt.mkv",
                    500,
                    "Decryption failed",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 2);
        assert_eq!(mock.hits("file-cdn.example/animezey/frieren-02"), 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_transient_media_failure_does_not_hide_discovered_files() {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()).page(
            "file-cdn.example/animezey/frieren-02.mkv",
            500,
            "dead",
        ));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(result.is_ok_and(|streams| !streams.is_empty()));
        assert_eq!(mock.hits("file-cdn.example"), 0);
    }

    #[tokio::test]
    async fn a_wrong_episode_file_is_rejected() {
        // The file is episode 5, not the requested episode 2.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(format!("api.themoviedb.org/3/tv/{TMDB_ID}"), 200, tmdb_page())
                .page(
                    SEARCH_1,
                    200,
                    serde_json::json!({
                        "data": { "files": [file_row("f1", "Frieren 05.mkv", "/file/f1", 734_003_200.0)] }
                    })
                    .to_string(),
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert_eq!(mock.hits("/file/"), 0);
    }

    #[tokio::test]
    async fn cards_sort_4k_first() -> Result<(), SourceError> {
        let hd_url = "https://file-cdn.example/animezey/frieren-02-1080.mkv";
        let uhd_url = "https://file-cdn.example/animezey/frieren-02-2160.mkv";
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("api.themoviedb.org/3/tv/{TMDB_ID}"),
                    200,
                    tmdb_page(),
                )
                .page(
                    SEARCH_1,
                    200,
                    serde_json::json!({
                        "data": { "files": [
                            file_row("f1", "Frieren 02 1080p.mkv", "/file/f1", 734_003_200.0),
                            file_row("f2", "Frieren 02 2160p.mkv", "/file/f2", 6_442_450_944.0),
                        ] }
                    })
                    .to_string(),
                )
                .page(
                    FILE_1,
                    200,
                    format!(r#"<video><source src="{hd_url}" type="video/mp4"></video>"#),
                )
                .page(
                    "1.animezey23112022.workers.dev/file/f2?a=view",
                    200,
                    format!(r#"<video><source src="{uhd_url}" type="video/mp4"></video>"#),
                )
                .page("file-cdn.example/animezey/frieren-02-1080.mkv", 200, "")
                .page("file-cdn.example/animezey/frieren-02-2160.mkv", 200, ""),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].url.as_str(), uhd_url);
        assert_eq!(streams[0].meta.resolution, Some(2160));
        assert_eq!(streams[1].meta.resolution, Some(1080));
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
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()));
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
        assert_eq!(mock.hits("api.themoviedb.org/3/tv"), 0);
        Ok(())
    }
    #[test]
    fn live_worker_string_sizes_and_bare_first_episodes_are_supported() {
        let file: FileRow = serde_json::from_value(serde_json::json!({"mimeType":"video/x-matroska","size":"2494076650","name":"[Anitsu] Jujutsu Kaisen - 01 [BD 1080p x265 FLAC].mkv"})).unwrap_or_else(|e| panic!("live file row: {e}"));
        assert_eq!(file.size, Some(2_494_076_650.0));
        let mock = Arc::new(ScriptedFetcher::default());
        let ctx = ctx_for(&mock);
        let scrape = Scrape {
            ctx: &ctx,
            ua: "test",
            base_names: vec!["Jujutsu Kaisen".to_string()],
            is_anime: true,
            year: Some(2020),
            season: 1,
            episode: 1,
            domain_index: 0,
        };
        assert!(scrape.is_correct_episode(file.name.as_deref().unwrap_or("")));
        assert!(!scrape.is_correct_episode("Jujutsu Kaisen S02E01.mkv"));
        assert!(!scrape.is_correct_episode("Jujutsu Kaisen S02 - 01.mkv"));
        assert!(!scrape.is_correct_episode("Jujutsu Kaisen - 02.mkv"));
    }
}
