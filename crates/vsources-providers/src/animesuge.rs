//! `AnimeSuge`: animesuge.at — anime sub/dub HLS through megaplay.
//!
//! Ports `src/source/AnimeSuge.js` + `src/nuvio/animesuge.cjs` (the
//! wrapper and the deobfuscated scraper folded into one module):
//!
//! 1. search `GET /api/animesuge/anime/search?keyword={title}` (with
//!    `X-Requested-With: XMLHttpRequest`) → `{ result: { html } }` with
//!    `href="https://animesuge.at/anime/{slug}"` links; two direct slug
//!    guesses join the candidates (`{slug}`, `{slug}-tv`) because the
//!    search API often misses the main series;
//! 2. the first 8 candidates' pages (`GET /anime/{slug}`) yield the
//!    `data-id`, the real title (`og:title` → `<title>` → slug, cleaned
//!    of entities and the site suffixes), and the premiere year
//!    (`itemprop="dateCreated"` → `Premiered:`);
//! 3. the Task-33 exactness gate: only an exact normalized-title match
//!    (100) or a ≤2-edit spelling variant with the same token count
//!    (95) qualifies, and the candidate's premiere year must match the
//!    requested one (±1, ±3 for season ≥ 2) — a near-exact variant
//!    with an unparseable year is refused; the slug gets a ±5/-10
//!    season-aware nudge;
//! 4. `GET /api/animesuge/server/list?id={animeId}&episode={n}` → the
//!    `data-type`/`data-link`/`data-sv-id` server rows, `data-link`
//!    being a base64 megaplay.buzz URL;
//! 5. each server resolves through megaplay: the stream page's
//!    `data-id` → `GET /stream/getSourcesNew?id={id}` — the 2026-09
//!    encrypted `{ tracks, enc }` shape decrypts through
//!    [`decrypt_megaplay_enc`], the legacy `sources: { file }` (or
//!    bare-string `sources`) shape passes through;
//! 6. the wrapper converts each raw stream directly (not through
//!    `build_stream_results`):
//!    sub/dub languages from
//!    `meta.category`, the megaplay `Referer` hotlink header, HLS
//!    format, and the quality→height mapping (`1080p` default);
//!    the whole scraper races a 20 s deadline.
//!
//! Cuts and mappings (vs. upstream):
//!
//! - The scraper's TMDB refetch (`getTmdbInfo`) is folded into the
//!   wrapper's resolution — the title and year come from
//!   `ctx.media`/[`TmdbClient`].
//! - The wrapper drops the scraper's `subtitles`, `intro`/`outro`, and
//!   its `behaviorHints.headers` block (upstream's result objects
//!   never carry them either — `buildStreamResults` reads
//!   `proxyHeaders.request`, not `headers`, and this wrapper does not
//!   call it at all); the port keeps the same shape.
//! - The candidate page's poster parse is cut — upstream extracts it
//!   and never uses it.
//! - `meta.title` (`{title} (AnimeSuge SUB)`) → [`Stream::label`].
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; the normalizer still strips combining marks
//!   (U+0300–036F) and precomposed accents drop out.
//! - Upstream's `console.log` telemetry is dropped; the per-request
//!   timeout is the crate's (upstream bounds the whole sweep with the
//!   20 s race only).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use fancy_regex::Regex;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta,
};

use crate::nuvio::megaplay::decrypt_megaplay_enc;
use crate::nuvio::{NuvioStream, with_deadline};

/// The provider id, upstream `this.id`.
const ID: &str = "animesuge";
/// The display label, upstream `this.label`.
const LABEL: &str = "AnimeSuge";
/// The site root, upstream `AS_BASE`/`this.baseUrl`.
const BASE_URL: &str = "https://animesuge.at";
/// The AJAX API root, upstream `AS_API`.
const AS_API: &str = "https://animesuge.at/api/animesuge";
/// The megaplay host, upstream `MEGAPLAY`.
const MEGAPLAY: &str = "https://megaplay.buzz";
/// The upstream browser UA (`UA`).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Upstream `this.ttl` — the megaplay URLs carry short-lived tokens.
const TTL: Duration = Duration::from_mins(5);
/// One HTTP call (the crate's norm; upstream bounds the sweep with the
/// 20 s race only).
const TIMEOUT: Duration = Duration::from_secs(10);
/// The scraper's outer race — the wrapper's `Promise.race` sentinel.
const SWEEP_DEADLINE: Duration = Duration::from_secs(20);
/// How many search candidates' pages are fetched (upstream
/// `results.slice(0, 8)`).
const MAX_CANDIDATES: usize = 8;

/// `href="https://animesuge.at/anime/{slug}"` search links.
static SEARCH_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"href="(https://animesuge\.at/anime/([^"]+))""#)
        .unwrap_or_else(|e| panic!("valid search-link pattern: {e}"))
});

/// `data-id="(\d+)"` on the anime page.
static DATA_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-id="(\d+)""#).unwrap_or_else(|e| panic!("valid data-id pattern: {e}"))
});

/// `<meta property="og:title" content="…">`.
static OG_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<meta\s+property="og:title"\s+content="([^"]+)""#)
        .unwrap_or_else(|e| panic!("valid og:title pattern: {e}"))
});

/// `<title>…</title>`.
static TITLE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<title>([^<]+)</title>").unwrap_or_else(|e| panic!("valid title pattern: {e}"))
});

/// `itemprop="dateCreated"…>(19|20)\d{2}` — the premiere year.
static DATE_CREATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"itemprop="dateCreated"[^>]*>[^<]*?((?:19|20)\d{2})"#)
        .unwrap_or_else(|e| panic!("valid dateCreated pattern: {e}"))
});

/// `Premiered:? (Fall )2002` — the year fallback anchor.
static PREMIERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Premiered:?\s*(?:[A-Za-z]+\s+)?((?:19|20)\d{2})")
        .unwrap_or_else(|e| panic!("valid premiered pattern: {e}"))
});

/// The server rows: `data-type` (sub/dub), `data-link` (base64
/// megaplay URL), `data-sv-id`.
static SERVER_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-type="([^"]+)"[\s\S]*?data-link="([^"]+)"[\s\S]*?data-sv-id="([^"]+)""#)
        .unwrap_or_else(|e| panic!("valid server-row pattern: {e}"))
});

/// `(\d{3,4})` — the bare number inside a quality label (the
/// wrapper's height mapping).
static QUALITY_NUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})").unwrap_or_else(|e| panic!("valid quality-number pattern: {e}"))
});

/// One search hit (a candidate slug, real or guessed).
struct SearchResult {
    /// The anime slug.
    slug: String,
}

/// One candidate's page facts — `getAnimeIdAndTitle`'s result.
struct Candidate {
    /// The anime id (`data-id`).
    id: Option<u64>,
    /// The slug.
    slug: String,
    /// The cleaned page title.
    title: String,
    /// The premiere year, when parseable.
    year: Option<i64>,
}

/// One server row — `getServerList`'s result.
struct ServerRow {
    /// `sub` or `dub`.
    kind: String,
    /// The base64-decoded megaplay stream URL.
    link: String,
}

/// One resolved megaplay source — `resolveMegaPlay`'s result.
struct MegaPlaySource {
    /// The stream URL.
    url: String,
}

/// The `AnimeSuge` provider.
pub struct AnimeSuge {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl AnimeSuge {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            mappings: None,
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
impl Source for AnimeSuge {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        if let Some(mapped) =
            crate::anime_mapping::title_context(self.mappings.as_ref(), ctx, media).await
            && let Ok(streams) = self.resolve_by_title(&mapped, media).await
            && !streams.is_empty()
        {
            return Ok(streams);
        }
        self.resolve_by_title(ctx, media).await
    }
}

/// The whole scraper sweep — the port of `getStreams`: search, the
/// candidate walk with the exactness gate, the server list, and the
/// megaplay resolution, deduped by URL and sorted sub-first.
async fn scrape(
    ctx: &ResolveCtx<'_>,
    name: &str,
    year: Option<u16>,
    media: &MediaRef,
    episode: u32,
) -> Vec<NuvioStream> {
    let results = search_animesuge(ctx, name).await;
    if results.is_empty() {
        return Vec::new();
    }

    // The candidate pages (first 8 only).
    let mut candidates = Vec::new();
    for result in results.iter().take(MAX_CANDIDATES) {
        if let Some(candidate) = anime_id_and_title(ctx, &result.slug).await {
            candidates.push(candidate);
        }
    }

    // The Task-33 exactness gate.
    let Some(best) = best_candidate(&candidates, name, year, media) else {
        return Vec::new();
    };
    let Some(anime_id) = best.id else {
        return Vec::new();
    };

    // The server list.
    let servers = server_list(ctx, anime_id, episode).await;
    if servers.is_empty() {
        return Vec::new();
    }

    // Each server resolves through megaplay, deduped by URL.
    let mut streams = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for server in servers {
        let Some(source) = resolve_mega_play(ctx, &server.link).await else {
            continue;
        };
        if !seen.insert(source.url.clone()) {
            continue;
        }
        // The scraper's card shape — `meta.category` drives the
        // wrapper's sub/dub conversion; the other fields document the
        // upstream meta verbatim (including the always-`movie`
        // `mediaType` oddity).
        let audio = if server.kind == "dub" {
            "english"
        } else {
            "japanese"
        };
        let language = if server.kind == "dub" { "en" } else { "ja" };
        let mut stream = NuvioStream::new(source.url)
            .with_name(format!("AnimeSuge\n{} 1080p", server.kind.to_uppercase()))
            .with_title(format!(
                "{} - Episode {episode} ({})",
                best.title,
                server.kind.to_uppercase()
            ))
            .with_quality("1080p");
        stream.meta = Some(serde_json::json!({
            "provider": "AnimeSuge",
            "source": "megaplay.buzz",
            "server": "megaplay.buzz",
            "type": "hls",
            "quality": "1080p",
            "audio": audio,
            "language": [language],
            "category": server.kind,
            "title": best.title,
            "episode": episode,
            "directStream": true,
            "mediaType": "movie",
        }));
        streams.push(stream);
    }

    // Sub first, then dub.
    streams.sort_by(|a, b| match (a.category(), b.category()) {
        (Some("sub"), Some("dub")) => std::cmp::Ordering::Less,
        (Some("dub"), Some("sub")) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    streams
}

/// The Task-33 exactness gate — the best surviving candidate, or
/// `None` when nothing qualifies: exact (100) or single-word
/// near-exact (95) normalized titles only, the year gate, and the
/// season-aware slug preference.
fn best_candidate<'a>(
    candidates: &'a [Candidate],
    name: &str,
    year: Option<u16>,
    media: &MediaRef,
) -> Option<&'a Candidate> {
    let query_norm = normalize_title(name);
    let req_year = year.map(i64::from);
    let mut best: Option<&'a Candidate> = None;
    let mut best_score = 0;
    for candidate in candidates {
        let candidate_norm = normalize_title(&candidate.title);
        if candidate_norm.is_empty() || query_norm.is_empty() {
            continue;
        }
        let mut score = 0;
        if candidate_norm == query_norm {
            score = 100;
        } else if candidate_norm.chars().count() >= 6
            && query_norm.chars().count() >= 6
            // Same token count: an appended/missing word is a
            // DIFFERENT title — only intra-word spelling variants
            // qualify here.
            && candidate_norm.split(' ').count() == query_norm.split(' ').count()
            && levenshtein_within(&candidate_norm, &query_norm, 2)
        {
            score = 95;
        }
        if score == 0 {
            // Every loose tier removed — wrong-content risk.
            continue;
        }

        // The year gate.
        if let (Some(req), Some(found)) = (req_year, candidate.year) {
            let tolerance = if media.season.is_some_and(|season| season >= 2) {
                3
            } else {
                1
            };
            if (found - req).abs() > tolerance {
                continue;
            }
        } else if score == 95 && req_year.is_some() && candidate.year.is_none() {
            // A near-exact variant whose year could not be parsed is
            // refused.
            continue;
        }

        // Season-aware slug preference within the qualifying tier.
        if media.season.is_none_or(|season| season == 1) {
            if candidate.slug.ends_with("-tv") || candidate.slug.contains("-tv-") {
                score += 5;
            }
            if candidate.slug.ends_with("specials")
                || candidate.slug.ends_with("special")
                || candidate.slug.contains("-special-")
                || candidate.slug.contains("-ova-")
                || candidate.slug.contains("-oad-")
                || candidate.slug.contains("0-movie")
            {
                score -= 10;
            }
        } else if let Some(season) = media.season {
            let season = i64::from(season);
            if candidate.slug.contains(&format!("{season}nd-season"))
                || candidate.slug.contains(&format!("{season}rd-season"))
                || candidate.slug.contains(&format!("season-{season}"))
            {
                score += 5;
            }
        }

        if score > best_score {
            best_score = score;
            best = Some(candidate);
        }
    }
    best
}

/// Search animesuge — the port of `searchAnimeSuge`: the AJAX search's
/// HTML links plus the two direct slug guesses.
async fn search_animesuge(ctx: &ResolveCtx<'_>, query: &str) -> Vec<SearchResult> {
    let url = format!("{AS_API}/anime/search?keyword={}", encode_component(query));
    let request = FetchRequest::get(
        Url::parse(&url).unwrap_or_else(|e| panic!("the AnimeSuge search URL must parse: {e}")),
    )
    .with_header("User-Agent", UA)
    .with_header("X-Requested-With", "XMLHttpRequest")
    .with_timeout(TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.is_success() {
        return Vec::new();
    }
    let Ok(data) = response.json::<Value>() else {
        return Vec::new();
    };
    let html = data
        .pointer("/result/html")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for captures in SEARCH_LINK.captures_iter(html).flatten() {
        if let Some(slug) = captures.get(2).map(|group| group.as_str().to_string())
            && seen.insert(slug.clone())
        {
            results.push(SearchResult { slug });
        }
    }

    // The direct slug guesses (the search API often misses the main
    // series).
    let slug_base = slugify(query);
    for guess in [slug_base.clone(), format!("{slug_base}-tv")] {
        if seen.insert(guess.clone()) {
            results.push(SearchResult { slug: guess });
        }
    }
    results
}

/// One candidate's page facts — the port of `getAnimeIdAndTitle`
/// (the poster parse is cut — upstream extracts it and never uses it).
async fn anime_id_and_title(ctx: &ResolveCtx<'_>, slug: &str) -> Option<Candidate> {
    let url = format!("{BASE_URL}/anime/{slug}");
    let request = FetchRequest::get(
        Url::parse(&url).unwrap_or_else(|e| panic!("the AnimeSuge page URL must parse: {e}")),
    )
    .with_header("User-Agent", UA)
    .with_timeout(TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let html = response.body;

    let id = DATA_ID
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse::<u64>().ok());

    // The title: og:title → <title> → the slug, cleaned.
    let mut title = OG_TITLE
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())
        .or_else(|| {
            TITLE_TAG
                .captures(&html)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map(|group| group.as_str().to_string())
        })
        .unwrap_or_else(|| slug.to_string());
    title = clean_title(&title);

    let year = DATE_CREATED
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse::<i64>().ok())
        .or_else(|| {
            PREMIERED
                .captures(&html)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .and_then(|group| group.as_str().parse::<i64>().ok())
        });

    Some(Candidate {
        id,
        slug: slug.to_string(),
        title,
        year,
    })
}

/// Strip the site's entities, suffixes, and prefix — the JS
/// `title.replace(...)` chain.
fn clean_title(title: &str) -> String {
    let title = title
        .replace("&amp;amp;#0?39;", "'")
        .replace("&amp;amp;", "&")
        .replace("&#0?39;", "'")
        .replace("&amp;", "&");
    // `\s*-\s*Watch on AnimeSuge.*$` and `\s*-\s*AnimeSuge.*$`
    // (unanchored — the suffix can follow any text), then the
    // `Watch ` prefix.
    let title = strip_suffix_marker(&title, "- Watch on AnimeSuge")
        .or_else(|| Some(title.clone()))
        .unwrap_or_default();
    let title = strip_suffix_marker(&title, "- AnimeSuge").unwrap_or(title);
    let title = title.strip_prefix("Watch ").unwrap_or(&title);
    title.trim().to_string()
}

/// Cut `marker` and everything after it, trimming the whitespace the
/// JS `\s*-\s*` prefix consumes.
fn strip_suffix_marker(title: &str, marker: &str) -> Option<String> {
    let index = title.find(marker)?;
    let cut = title[..index].trim_end().len();
    Some(title[..cut].to_string())
}

/// The server list — the port of `getServerList`: the AJAX HTML's
/// server rows with base64-decoded links.
async fn server_list(ctx: &ResolveCtx<'_>, anime_id: u64, episode: u32) -> Vec<ServerRow> {
    let url = format!("{AS_API}/server/list?id={anime_id}&episode={episode}");
    let request = FetchRequest::get(
        Url::parse(&url).unwrap_or_else(|e| panic!("the AnimeSuge server URL must parse: {e}")),
    )
    .with_header("User-Agent", UA)
    .with_header("X-Requested-With", "XMLHttpRequest")
    .with_timeout(TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.is_success() {
        return Vec::new();
    }
    let Ok(data) = response.json::<Value>() else {
        return Vec::new();
    };
    let html = data
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut servers = Vec::new();
    for captures in SERVER_ROW.captures_iter(html).flatten() {
        let Some(kind) = captures.get(1).map(|group| group.as_str().to_string()) else {
            continue;
        };
        let Some(link) = captures.get(2).and_then(|group| {
            base64::engine::general_purpose::STANDARD
                .decode(group.as_str())
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        }) else {
            continue;
        };
        servers.push(ServerRow { kind, link });
    }
    servers
}

/// Resolve one megaplay server link — the port of `resolveMegaPlay`:
/// the stream page's `data-id` → `getSourcesNew` (the encrypted `enc`
/// shape decrypted, the legacy `sources` shape passed through). The
/// plaintext `tracks` and `intro`/`outro` ride the scraper's raw shape
/// but the wrapper never surfaces them (see the module docs).
async fn resolve_mega_play(ctx: &ResolveCtx<'_>, stream_url: &str) -> Option<MegaPlaySource> {
    let page_url = Url::parse(stream_url).ok()?;
    let page = ctx
        .fetcher
        .request(
            FetchRequest::get(page_url.clone())
                .with_header("User-Agent", UA)
                .with_header("Referer", format!("{BASE_URL}/"))
                .with_timeout(TIMEOUT),
        )
        .await
        .ok()?;
    if !page.is_success() {
        return None;
    }
    let data_id = DATA_ID
        .captures(&page.body)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())?;

    let sources_url = format!("{MEGAPLAY}/stream/getSourcesNew?id={data_id}");
    let request = FetchRequest::get(
        Url::parse(&sources_url)
            .unwrap_or_else(|e| panic!("the megaplay sources URL must parse: {e}")),
    )
    .with_header("User-Agent", UA)
    .with_header("Referer", stream_url)
    .with_header("X-Requested-With", "XMLHttpRequest")
    .with_timeout(TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let data: Value = response.json().ok()?;

    // `sources` may be an object with a `file`, a bare string URL, or
    // absent (the encrypted `enc` shape).
    let sources = data.get("sources");
    let file = match sources {
        Some(Value::String(url)) => Some(url.clone()),
        Some(_) => data
            .pointer("/sources/file")
            .and_then(Value::as_str)
            .map(str::to_string),
        None => data
            .get("enc")
            .and_then(Value::as_str)
            .and_then(decrypt_megaplay_enc),
    };
    let url = file.filter(|url| !url.is_empty())?;
    Some(MegaPlaySource { url })
}

/// The wrapper's quality→height mapping — `4K`/`2160` → 2160, `720` →
/// 720, `480` → 480, the first 3–4 digit number otherwise, `1080`
/// default.
fn wrapper_height(quality: Option<&str>) -> u16 {
    let quality = quality.unwrap_or_default().to_lowercase();
    if quality.contains("4k") || quality.contains("2160") {
        return 2160;
    }
    if quality.contains("720") {
        return 720;
    }
    if quality.contains("480") {
        return 480;
    }
    QUALITY_NUM
        .captures(&quality)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
        .unwrap_or(1080)
}

/// The title normalizer — the port of `normalizeTitle`: lowercase,
/// strip diacritics, drop punctuation, remove articles and format
/// words, collapse whitespace.
fn normalize_title(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    let mut out: Vec<&str> = Vec::new();
    for word in kept.split_whitespace() {
        if matches!(
            word,
            "the"
                | "a"
                | "an"
                | "tv"
                | "season"
                | "part"
                | "specials"
                | "special"
                | "movie"
                | "ova"
                | "ona"
                | "oad"
        ) {
            continue;
        }
        out.push(word);
    }
    out.join(" ")
}

/// Bounded Levenshtein — the port of `levenshteinWithin`: true when
/// the edit distance is at most `max`, with the length and row-min
/// early exits (it exists purely so a real spelling variant like
/// "Shippuuden"/"Shippuden" is not lost, NOT as a similarity score).
fn levenshtein_within(a: &str, b: &str, max: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a == b {
        return true;
    }
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (index, char_a) in a.iter().enumerate() {
        let mut current = vec![index + 1];
        let mut row_min = index + 1;
        for (offset, char_b) in b.iter().enumerate() {
            let cost = usize::from(char_a != char_b);
            let value = (previous[offset + 1] + 1)
                .min(current[offset] + 1)
                .min(previous[offset] + cost);
            current.push(value);
            row_min = row_min.min(value);
        }
        if row_min > max {
            // Early exit — cannot recover.
            return false;
        }
        previous = current;
    }
    previous[b.len()] <= max
}

/// The direct-slug normalizer — the JS `slugBase` chain: lowercase,
/// strip diacritics, drop non `[a-z0-9\s]`, spaces → hyphens.
fn slugify(query: &str) -> String {
    let kept: String = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    let slugged = kept.replace(' ', "-");
    let mut collapsed = String::new();
    let mut in_hyphen = false;
    for char in slugged.chars() {
        if char == '-' {
            if !in_hyphen {
                collapsed.push('-');
            }
            in_hyphen = true;
        } else {
            collapsed.push(char);
            in_hyphen = false;
        }
    }
    collapsed.trim_matches('-').to_string()
}

/// Percent-encode a query component (the JS `encodeURIComponent`).
fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
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

impl AnimeSuge {
    async fn resolve_by_title(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let title = display_title(&name, year, media);
        let episode = media.episode.unwrap_or(1);

        // The scraper races the 20 s deadline; the deadline winner
        // answers the empty sweep (the JS `null`).
        let sweep = scrape(ctx, &name, year, media, episode);
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }

        // The wrapper's own conversion — every raw stream ships direct
        // with the megaplay Referer (the m3u8 CDN 403s datacenter IPs,
        // so the player's residential IP fetches it).
        let mut streams = Vec::new();
        for stream in &raw {
            let Ok(url) = Url::parse(&stream.url) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            // The category, with the title's DUB marker as the
            // fallback.
            let category = stream.category().map_or_else(
                || {
                    if stream
                        .title
                        .as_deref()
                        .is_some_and(|title| title.contains("DUB"))
                    {
                        "dub".to_string()
                    } else {
                        "sub".to_string()
                    }
                },
                str::to_string,
            );
            let audio_label = if category == "dub" { "DUB" } else { "SUB" };
            let languages = if category == "dub" {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            };
            streams.push(Stream {
                url,
                format: Format::Hls,
                label: Some(format!("{title} (AnimeSuge {audio_label})")),
                meta: StreamMeta {
                    dubbed: Some(category == "dub"),
                    subbed: Some(category == "sub"),
                    languages,
                    resolution: Some(wrapper_height(stream.quality.as_deref())),
                    source_id: Some(ID.to_string()),
                    source_label: Some(LABEL.to_string()),
                    request_headers: [("Referer".to_string(), format!("{MEGAPLAY}/"))]
                        .into_iter()
                        .collect(),
                    ..StreamMeta::default()
                },
                ttl: TTL,
                is_external: false,
                behavior_hints: std::collections::BTreeMap::new(),
            });
        }

        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};

    use super::*;

    /// A fetcher serving canned `(status, body)` keyed by URL path (or
    /// `path?query`) — later registrations REPLACE earlier ones —
    /// recording every request. Query-bearing lookups fall back to
    /// the bare path, so TMDB requests (`?api_key=…`) are scripted
    /// by path alone.
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

    /// The fixture media: Frieren S01E02.
    const TMDB_ID: u64 = 209_867;
    const NAME: &str = "Frieren: Beyond Journey's End";
    /// The search page (query-keyed).
    const SEARCH: &str =
        "/api/animesuge/anime/search?keyword=Frieren%3A%20Beyond%20Journey%27s%20End";
    /// The candidate page.
    const ANIME_PAGE: &str = "/anime/frieren-beyond-journeys-end";
    /// The server list page (query-keyed).
    const SERVER_LIST: &str = "/api/animesuge/server/list?id=154587&episode=2";
    /// The base64-encoded megaplay sub link (data-link).
    const LINK_SUB: &str = "aHR0cHM6Ly9tZWdhcGxheS5idXp6L3N0cmVhbS9hbmkvMTU0NTg3LzIvc3Vi";
    /// The base64-encoded megaplay dub link.
    const LINK_DUB: &str = "aHR0cHM6Ly9tZWdhcGxheS5idXp6L3N0cmVhbS9hbmkvMTU0NTg3LzIvZHVi";
    /// The encrypted sub sources — ground truth generated with Node's
    /// `crypto` using the upstream key/IV (the `nuvio::megaplay`
    /// ground-truth recipe).
    const ENC_SUB: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfU_mcdDzyMudFPrC1OQLhJpZ5ycCp9IOePp3IulIXsS_dmIU7WY-6FC_RFX82wF6a4";
    /// The encrypted dub sources.
    const ENC_DUB: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfV9FstHnQBKGBWYluF-VtF0PD8jt7V9hQnCQYizN6yLQpwJ8f2ORE3oLZAyYlo4swU";
    /// The megaplay stream pages.
    const SUB_PAGE: &str = "/stream/ani/154587/2/sub";
    const DUB_PAGE: &str = "/stream/ani/154587/2/dub";

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> AnimeSuge {
        AnimeSuge::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// The search HTML (the slug link) — the guesses join in code.
    fn search_html() -> String {
        r#"<div class="result"><a href="https://anichan.example/redir">x</a><a href="https://animesuge.at/anime/frieren-beyond-journeys-end">Frieren</a></div>"#.to_string()
    }

    /// The search response — HTML inside JSON, built through `json!`
    /// so the embedded double quotes stay properly escaped.
    fn search_response(html: &str) -> String {
        serde_json::json!({ "result": { "html": html } }).to_string()
    }

    /// The server-list response — one HTML row per `(kind, link)`
    /// pair, built through `json!` for the same reason.
    fn server_list_response(rows: &[(&str, &str)]) -> String {
        let mut row_html = String::new();
        for (kind, link) in rows {
            use std::fmt::Write as _;
            let _ = write!(
                row_html,
                r#"<div data-type="{kind}" data-link="{link}" data-sv-id="1"></div>"#
            );
        }
        serde_json::json!({ "result": row_html }).to_string()
    }
    /// The candidate page: the data-id, the og:title, the premiere
    /// year.
    fn anime_page() -> String {
        r#"<html><head><meta property="og:title" content="Watch Frieren: Beyond Journey's End - AnimeSuge"><meta property="og:image" content="https://animesuge.at/images/x.webp"></head><body><div class="watch" data-id="154587"></div><span itemprop="dateCreated">Sep 29, 2023</span></body></html>"#.to_string()
    }

    /// The shared happy-path pages: TMDB, search, the candidate page,
    /// the server list, both megaplay stream pages, and the encrypted
    /// sources.
    fn base_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
        )
        .page(SEARCH, 200, search_response(&search_html()))
        .page(ANIME_PAGE, 200, anime_page())
        .page(
            SERVER_LIST,
            200,
            server_list_response(&[("sub", LINK_SUB), ("dub", LINK_DUB)]),
        )
        .page(SUB_PAGE, 200, r#"<html><div data-id="8817"></div></html>"#)
        .page(DUB_PAGE, 200, r#"<html><div data-id="8818"></div></html>"#)
        .page(
            "/stream/getSourcesNew?id=8817",
            200,
            format!(r#"{{"tracks":[],"enc":"{ENC_SUB}"}}"#),
        )
        .page(
            "/stream/getSourcesNew?id=8818",
            200,
            format!(r#"{{"tracks":[],"enc":"{ENC_DUB}"}}"#),
        )
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "animesuge");
        assert_eq!(info.label, "AnimeSuge");
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
            Some("https://animesuge.at/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_sorted_sub_first() -> Result<(), SourceError> {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        // Sub first, then dub — both decrypted through the enc blob.
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].url.as_str(),
            "https://megap.akirax.buzz/hls/frieren/sub/master.m3u8?token=sub123"
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://megap.akirax.buzz/hls/frieren/dub/master.m3u8?token=dub456"
        );
        assert_eq!(streams[0].format, Format::Hls);
        // The wrapper's quality→height mapping pins 1080.
        assert_eq!(streams[0].meta.resolution, Some(1080));
        // The megaplay Referer rides every card.
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        // Sub/dub languages from meta.category.
        assert_eq!(
            streams[0].meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            streams[1].meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (AnimeSuge SUB)")
        );
        assert_eq!(streams[0].ttl, TTL);
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("animesuge"))
        );
        // The search and server-list requests carried the XHR marker;
        // the megaplay page fetch carried the animesuge referer and the
        // sources fetch the stream-page referer.
        assert_eq!(
            mock.sent_header("anime/search", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            mock.sent_header("server/list", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            mock.sent_header("/stream/ani/154587/2/sub", "Referer")
                .as_deref(),
            Some("https://animesuge.at/")
        );
        assert_eq!(
            mock.sent_header("getSourcesNew?id=8817", "Referer")
                .as_deref(),
            Some("https://megaplay.buzz/stream/ani/154587/2/sub")
        );
        // The `-tv` slug guess was also walked as a candidate (its
        // page 404s and drops out — the bare slug guess dedupes
        // against the search hit).
        assert_eq!(mock.hits_starting_with("/anime/"), 2);
        Ok(())
    }

    #[tokio::test]
    async fn the_legacy_sources_shape_still_works() -> Result<(), SourceError> {
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default())
                .page(
                    "/stream/getSourcesNew?id=8817",
                    200,
                    r#"{"sources":{"file":"https://megap.akirax.buzz/hls/legacy/master.m3u8"},"tracks":[]}"#,
                )
                // The dub stream page has no data-id → the row drops.
                .page(DUB_PAGE, 200, r"<html>no data-id</html>"),
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
    async fn duplicate_stream_urls_dedup() -> Result<(), SourceError> {
        // Both server rows decode to the same megaplay page → the same
        // decrypted URL → one card.
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default())
                .page(
                    SERVER_LIST,
                    200,
                    server_list_response(&[("sub", LINK_SUB), ("dub", LINK_SUB)]),
                )
                .page(DUB_PAGE, 200, r"<html>no data-id</html>"),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn the_year_gate_rejects_a_wrong_year() {
        // The candidate's premiere year (1985) misses the requested
        // 2023 far beyond the ±1 tolerance.
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default()).page(
                ANIME_PAGE,
                200,
                r#"<html><head><meta property="og:title" content="Frieren: Beyond Journey's End"></head><body><div data-id="154587"></div><span itemprop="dateCreated">Oct 3, 1985</span></body></html>"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        assert_eq!(mock.hits("/api/animesuge/server/list"), 0);
    }

    #[tokio::test]
    async fn a_spelling_variant_with_the_same_token_count_qualifies() -> Result<(), SourceError> {
        // The page title doubles a 'u' — a ≤2-edit variant of the
        // same title (the "Shippuuden"/"Shippuden" case).
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default()).page(
                ANIME_PAGE,
                200,
                r#"<html><head><meta property="og:title" content="Frieren: Beyond Journey'ss End"></head><body><div data-id="154587"></div><span itemprop="dateCreated">Sep 29, 2023</span></body></html>"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_loose_substring_match_is_refused() {
        // The candidate title merely contains the query — every loose
        // tier was removed (the "Mutiny"/"Odin: Starlight Mutiny"
        // class of wrong-content matches).
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default()).page(
                ANIME_PAGE,
                200,
                r#"<html><head><meta property="og:title" content="Frieren: Beyond Journey's End Extra Edition"></head><body><div data-id="154587"></div><span itemprop="dateCreated">Sep 29, 2023</span></body></html>"#,
            ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn the_direct_slug_guess_serves_when_the_search_misses() -> Result<(), SourceError> {
        // The search answers no links; the `{slug}` guess joins the
        // candidates and its page resolves.
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default())
                .page(SEARCH, 200, r#"{"result":{"html":""}}"#)
                .page(
                    "/anime/frieren-beyond-journeys-end-tv",
                    200,
                    r"<html><title>nothing</title></html>",
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(404), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn pre_resolved_media_skips_tmdb() -> Result<(), SourceError> {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let media = ResolvedMedia {
            tmdb_id: Some(TMDB_ID),
            imdb_id: Some("tt22354494".to_string()),
            name: NAME.to_string(),
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

        assert_eq!(streams.len(), 2);
        assert_eq!(mock.hits_starting_with("/3/tv"), 0);
        Ok(())
    }
}
