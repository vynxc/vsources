//! `AniMoTVSlash`: animotvslash.org — hardsub + softsub anime servers
//! and movie sub/dub.
//!
//! Ports `src/source/AniMoTVSlash.js` + `src/nuvio/animotvslash.cjs`
//! (movies, series; the site maxes at 1080p — no 4K exists upstream,
//! cards label honestly):
//!
//! 1. TMDB → title/year → the site's wp-json search over a four-rung
//!    query ladder (raw, punctuation-stripped, first-6-words,
//!    first-3-words) whose rungs are **merged** and deduped by slug —
//!    WP search quirkily returns only the Season-2 posts for the raw
//!    title while the stripped rung also surfaces the base post.
//! 2. Season-aware ranking of the `/anime/<slug>/` posts (exact /
//!    contains / overlap scoring, `-season-N` matched anywhere in the
//!    slug plus word-form ordinals, dub-variant tie-breaking), then a
//!    walk of the near-equal top band (within 5 points of the leader,
//!    max 3): the best-scoring post alone can be P2P-only while a
//!    sibling carries real players.
//! 3. The detail page → the episode-page link (a three-regex ladder
//!    tolerating re-uploaded `-episode-N-<counter>` slugs).
//! 4. The episode page's `select.mirror` — one base64-encoded embed
//!    per option, bucketed Sub/Softsub/Dub by the option label. Site
//!    player-config DIVs resolve directly: `jw` (rumble HLS master),
//!    `plyr` (videas hlsv1 — an **Origin-only** inverted hotlink
//!    gate), `vidstack` (videas MP4 tiers), plus the aiovg
//!    `player-embed` direct-MP4 variant. IFRAME embeds resolve per
//!    host: `vidara.to` (POST `/api/stream`), `VidHide` (p,a,c,k,e,d-js
//!    unpack + probed master playlist), and `megaplay.buzz`
//!    (`getSourcesNew` + the shared AES decrypt).
//! 5. WebTorrent/P2P and the unresolvable heads (Moon, Hydrax,
//!    tryembed, `VidNest`, the site's own `.ru` SPA) are skipped
//!    honestly; the survivors flow through
//!    `build_stream_results`
//!    with the audio-track stamp (`Japanese` for sub/softsub,
//!    `English` for dub) driving the per-card language flags.
//!
//! Direct-play notes (the scraper's live mapping): the videas hlsv1
//! host 403s under any `Referer` (Origin only), the vidhide and
//! megaplay CDNs verify under their stamped `Referer`s, and the
//! megaplay CDN (`fetch.nexabloom.top`) 403s all datacenter IPs — the
//! shared `NO_REFERER_HOSTS` routing ships it direct so the player's
//! residential IP fetches it.
//!
//! Cuts for the library port:
//!
//! - The scraper's own TMDB details fetch (title/year) — the shared
//!   [`TmdbClient`] serves it (`cineby`/`framex` precedent).
//! - The detail-page metadata scrape keeps only `og:title` (the
//!   display title); poster/synopsis/genres/year fed only console
//!   logs and unused metadata upstream.
//! - `pickBestSeries` (exported, never called — `getStreams` walks
//!   the ranked band) is not ported. The aiovg base64's
//!   `.replace(/\./g, '')` is a no-op upstream (the capture class
//!   excludes dots) and stays one here; `buildStream`'s `isHls` check
//!   matched every URL (empty regex alternative), so its `meta.type`
//!   was always `hls` — only `meta.category` is attached, the one
//!   field the shared layer reads.
//! - The wrapper's 30 s `callNuvioProvider` timeout race rides
//!   `with_deadline`; the child-process
//!   plumbing itself has no equivalent (the scraper is folded in).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fancy_regex::Regex;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_core::unpack::unpack_eval;

use crate::nuvio::megaplay::decrypt_megaplay_enc;
use crate::nuvio::{BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline};

/// The provider id, upstream `this.id`.
const ID: &str = "animotvslash";
/// The display label, upstream `this.label`.
const LABEL: &str = "AniMoTVSlash";
/// The site origin, upstream `this.baseUrl` / the scraper's `BASE`.
const BASE_URL: &str = "https://animotvslash.org";
/// Upstream `this.ttl` — 5 min (HLS masters are short-lived).
const TTL: Duration = Duration::from_mins(5);
/// The upstream browser UA (the scraper's `UA`).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// One scraper fetch (upstream `fetchText` default).
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
/// The detail and episode pages (upstream 15 s overrides).
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// One `VidHide` master-playlist probe (upstream 6 s).
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
/// The wrapper's whole-scraper race (`timeoutMs: 30000`).
const SWEEP_DEADLINE: Duration = Duration::from_secs(30);
/// The megaplay sources API.
const MEGAPLAY_API: &str = "https://megaplay.buzz/stream/getSourcesNew";

/// The merged skip rules — P2P/torrent plus every head the scraper
/// cannot resolve headlessly (upstream `SKIP_HOSTS`, reasons in its
/// comments: the no-torrent rule, Moon's uploader SPA, Hydrax's
/// AES-CTR obfuscation, tryembed's server-side signatures, `VidNest`'s
/// chunked SPA, and the site's own-platform player).
static SKIP_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)p2pplay\.pro|webtorrent|openwebtorrent|bysezoxexe\.com|abyssplayer\.com|hydrax|tryembed\.us\.cc|vidnest\.fun|animotvslash\.ru")
        .unwrap_or_else(|e| panic!("valid skip-host pattern: {e}"))
});

/// A series post's URL must end in `/anime/<slug>/`.
static ANIME_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/anime/[^/]+/?$").unwrap_or_else(|e| panic!("valid anime-path pattern: {e}"))
});

/// `<meta property="og:title" content="…">` — the detail display
/// title.
static OG_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<meta\s+property="og:title"\s+content="([^"]+)""#)
        .unwrap_or_else(|e| panic!("valid og-title pattern: {e}"))
});

/// The mirror `<select>` of the episode page.
static MIRROR_SELECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<select[^>]*class="[^"]*mirror[^"]*"[^>]*>(.*?)</select>"#)
        .unwrap_or_else(|e| panic!("valid mirror-select pattern: {e}"))
});

/// One mirror `<option>` — the base64 embed value plus the label.
static OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<option[^>]*value="([^"]*)"[^>]*>([^<]*)<"#)
        .unwrap_or_else(|e| panic!("valid option pattern: {e}"))
});

/// The site player-config path: kind + base64 JSON payload.
static PLAYER_CONFIG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(jw-player|plyr-player|vidstack-player)/([A-Za-z0-9+/=_-]+)")
        .unwrap_or_else(|e| panic!("valid player-config pattern: {e}"))
});

/// The aiovg `player-embed`'s direct-MP4 parameter.
static MP4_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[?&]mp4=([A-Za-z0-9+/=_-]+)")
        .unwrap_or_else(|e| panic!("valid mp4-param pattern: {e}"))
});

/// An iframe's `src` inside a decoded mirror embed.
static IFRAME_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"src="([^"]+)""#).unwrap_or_else(|e| panic!("valid iframe-src pattern: {e}"))
});

/// The megaplay embed page's stream id.
static DATA_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-id="(\d+)""#).unwrap_or_else(|e| panic!("valid data-id pattern: {e}"))
});

/// Direct master playlists inside unpacked `VidHide` JavaScript.
static MASTER_URLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s"'\\]+master\.[a-z0-9]{2,4}[^\s"'\\]*"#)
        .unwrap_or_else(|e| panic!("valid master-url pattern: {e}"))
});

/// `(\d{3,4})p` — the quality inside an aiovg direct URL.
static QUALITY_P: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})p").unwrap_or_else(|e| panic!("valid quality pattern: {e}"))
});

/// A `-season-N` suffix anywhere in a slug.
static SLUG_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)-season-(\d+)(?:-|$)")
        .unwrap_or_else(|e| panic!("valid slug-season pattern: {e}"))
});

/// A word-form ordinal season (`jujutsu-kaisen-2nd-season`).
static SLUG_ORDINAL_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)-(\d+)(?:nd|rd|th)-season(?:-|$)")
        .unwrap_or_else(|e| panic!("valid ordinal-season pattern: {e}"))
});

/// Special/OVA/movie/ONA slugs — penalized for S1 requests.
static SLUG_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-special|-ova|-movie-|-ona")
        .unwrap_or_else(|e| panic!("valid special-slug pattern: {e}"))
});

/// A trailing year in a slug (`-2024`).
static SLUG_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-(\d{4})(?:-|$)").unwrap_or_else(|e| panic!("valid slug-year pattern: {e}"))
});

/// An HTML entity — replaced before the character filter so entity
/// digits never leak into the normalized title.
static HTML_ENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&[a-z]+;|&#\d+;").unwrap_or_else(|e| panic!("valid entity pattern: {e}"))
});

/// The bucket prefix of a mirror label: `^(sub|softsub|dub)\s*-\s*`.
static BUCKET_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:sub|softsub|dub)\s*-\s*")
        .unwrap_or_else(|e| panic!("valid bucket-prefix pattern: {e}"))
});

/// A trailing parenthesized qualifier with optional surrounding
/// whitespace — the wrapper's `cleanTitle` strip.
static TRAILING_PARENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(.*?\)\s*$").unwrap_or_else(|e| panic!("valid trailing-parens pattern: {e}"))
});

/// The title cleaner's punctuation class.
const TITLE_PUNCT: [char; 11] = ['-', '–', '—', '_', ':', '(', ')', '\'', '"', '’', '‘'];

/// The audio bucket of a mirror option — its label prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    /// `Sub - …` — hardsub, Japanese audio.
    Sub,
    /// `Softsub - …` — Japanese audio plus selectable subs.
    SoftSub,
    /// `Dub - …` — English audio.
    Dub,
}

impl Bucket {
    /// The display label (`buildStream`'s `bucketLabel`).
    fn label(self) -> &'static str {
        match self {
            Self::Sub => "SUB",
            Self::SoftSub => "SOFTSUB",
            Self::Dub => "DUB",
        }
    }

    /// The bucket sort order (upstream `order`).
    fn order(self) -> u8 {
        match self {
            Self::Sub => 0,
            Self::SoftSub => 1,
            Self::Dub => 2,
        }
    }

    /// The `meta.category` key.
    fn key(self) -> &'static str {
        match self {
            Self::Sub => "sub",
            Self::SoftSub => "soft_sub",
            Self::Dub => "dub",
        }
    }
}

/// One `/anime/<slug>/` search post.
#[derive(Debug, Clone)]
struct SeriesPost {
    /// The post title (HTML entities decoded).
    title: String,
    /// The detail page URL.
    url: String,
    /// The last path segment of `url`.
    slug: String,
}

/// A search post carrying its match score.
#[derive(Debug, Clone)]
struct RankedPost {
    /// The post.
    post: SeriesPost,
    /// The normalized-title match score.
    score: i32,
}

/// One raw subtitle of a resolved mirror.
#[derive(Debug, Clone)]
struct ResolvedSubtitle {
    /// The subtitle file URL.
    url: String,
    /// The track language, when the host sent one.
    lang: Option<String>,
}

/// One resolved mirror entry — a playable URL with its quality,
/// hotlink headers, and bucket.
#[derive(Debug, Clone)]
struct ResolvedEntry {
    /// The stream URL.
    url: String,
    /// The quality label.
    quality: String,
    /// Per-stream request headers (`Referer`/`Origin`).
    headers: Vec<(String, String)>,
    /// Subtitle tracks.
    subtitles: Vec<ResolvedSubtitle>,
    /// The audio bucket.
    bucket: Bucket,
    /// The server display name.
    server: String,
}

/// The `AniMoTVSlash` provider.
pub struct AniMoTVSlash {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl AniMoTVSlash {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// A provider over the shared TMDB client. The scraper resolves
    /// every embed itself (vidara/vidhide/megaplay inline, exactly
    /// like the `.cjs`), so no extractor registry is needed.
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
impl Source for AniMoTVSlash {
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

impl AniMoTVSlash {
    /// The scraper's whole sweep: search ladder → ranking → the
    /// near-equal top band → the first post with resolvable servers —
    /// the port of `animotvslash.cjs` `getStreams`.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        clean_title: &str,
        year: Option<u16>,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Vec<NuvioStream> {
        let results = search_series_ladder(ctx, clean_title).await;
        if results.is_empty() {
            return Vec::new();
        }
        let ranked = rank_series(&results, clean_title, year, season);
        if ranked.is_empty() {
            return Vec::new();
        }

        // Walk the near-equal top candidates (within 5 points of the
        // leader — effectively sub/dub variants of the SAME season
        // post) — ship the first that yields resolvable servers.
        let top_score = ranked[0].score;
        let band: Vec<&RankedPost> = ranked
            .iter()
            .filter(|post| post.score >= top_score - 5)
            .take(3)
            .collect();
        for best in band {
            let (ep_url, detail_title) = find_episode_url(ctx, &best.post.url, episode).await;
            let Some(ep_url) = ep_url else {
                continue;
            };
            let anime_title = detail_title.unwrap_or_else(|| best.post.title.clone());

            let resolved = parse_episode_page(ctx, &ep_url).await;
            if resolved.is_empty() {
                continue;
            }

            // Dedupe by URL, sub first, then softsub, then dub.
            let mut seen = HashSet::new();
            let mut ordered = resolved;
            ordered.sort_by_key(|entry| entry.bucket.order());
            let mut streams = Vec::new();
            for entry in ordered {
                if !seen.insert(entry.url.clone()) {
                    continue;
                }
                streams.push(build_stream(&entry, &anime_title, episode));
            }
            if !streams.is_empty() {
                return streams;
            }
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Search + ranking
// ---------------------------------------------------------------------------

/// One wp-json search — the port of `searchSeries`.
async fn search_series(ctx: &ResolveCtx<'_>, query: &str) -> Vec<SeriesPost> {
    let mut url = Url::parse(&format!("{BASE_URL}/wp-json/wp/v2/search"))
        .unwrap_or_else(|e| panic!("valid search URL: {e}"));
    url.query_pairs_mut()
        .append_pair("search", query)
        .append_pair("per_page", "20")
        .append_pair("_fields", "title,url");
    let Some(data) = fetch_json(ctx, &url, None, FETCH_TIMEOUT).await else {
        return Vec::new();
    };
    let Some(posts) = data.as_array() else {
        return Vec::new();
    };
    posts
        .iter()
        .filter_map(|post| {
            let url = post.get("url")?.as_str()?;
            if !ANIME_PATH.is_match(url).unwrap_or(false) {
                return None;
            }
            let title = post
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .replace("&#8217;", "'")
                .replace("&amp;", "&");
            let slug = url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            Some(SeriesPost {
                title: title.trim().to_string(),
                url: url.to_string(),
                slug,
            })
        })
        .collect()
}

/// The query ladder, all rungs merged and deduped by slug — the port
/// of `searchSeriesLadder` (Task 41b: merging beats stopping at the
/// first productive rung).
async fn search_series_ladder(ctx: &ResolveCtx<'_>, title: &str) -> Vec<SeriesPost> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for query in ladder_queries(title) {
        for post in search_series(ctx, &query).await {
            if seen.insert(post.slug.clone()) {
                out.push(post);
            }
        }
    }
    out
}

/// The ladder's queries: raw → punctuation-stripped → first-6-words →
/// first-3-words, deduped, each at least 3 chars.
fn ladder_queries(title: &str) -> Vec<String> {
    let cleaned = collapse_spaces(&title.replace(TITLE_PUNCT, " "));
    let words: Vec<&str> = cleaned.split(' ').filter(|w| !w.is_empty()).collect();
    let first_six = words.iter().take(6).copied().collect::<Vec<_>>().join(" ");
    let first_three = words.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    let mut attempts = vec![title.to_string(), cleaned, first_six, first_three];
    let mut seen = HashSet::new();
    attempts.retain(|query| query.chars().count() >= 3 && seen.insert(query.clone()));
    attempts
}

/// Score and season-adjust the search posts — the port of
/// `rankSeries` (the ranked list, empty when nothing scores ≥ 50).
fn rank_series(
    results: &[SeriesPost],
    tmdb_title: &str,
    tmdb_year: Option<u16>,
    season: Option<u32>,
) -> Vec<RankedPost> {
    let query = normalize_title(tmdb_title);
    let mut ranked: Vec<RankedPost> = Vec::new();
    for post in results {
        let title = normalize_title(&post.title);
        let score = if title == query {
            100
        } else if title.contains(&query) {
            80
        } else if query.contains(&title) {
            70
        } else {
            // The overlap of the query's long words over the title's.
            let query_words: Vec<&str> = query
                .split(' ')
                .filter(|word| word.chars().count() > 2)
                .collect();
            let title_words: Vec<&str> = title
                .split(' ')
                .filter(|word| word.chars().count() > 2)
                .collect();
            let overlap = query_words
                .iter()
                .filter(|word| title_words.contains(word))
                .count();
            i32::try_from(overlap.saturating_mul(60) / query_words.len().max(1)).unwrap_or(0)
        };
        if score <= 0 {
            continue;
        }
        ranked.push(RankedPost {
            post: post.clone(),
            score,
        });
    }

    // Season awareness — `-season-N` anywhere in the slug, word-form
    // ordinals, dub suffixes neutral, base slugs penalized when an
    // S>1 post exists (and vice versa).
    for entry in &mut ranked {
        let slug = entry.post.slug.as_str();
        let slug_season = slug_season_of(slug);
        if let Some(season) = season.filter(|season| *season > 1) {
            match slug_season {
                Some(s) if s == season => entry.score += 25,
                Some(_) => entry.score -= 30,
                None => entry.score -= 10,
            }
        } else {
            if slug_season.is_some_and(|s| s != 1) {
                entry.score -= 25;
            }
            if SLUG_SPECIAL.is_match(slug).unwrap_or(false) {
                entry.score -= 30;
            }
        }
        // Year sanity when the slug carries one.
        if let Some(year) = tmdb_year
            && SLUG_YEAR
                .captures(slug)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .and_then(|group| group.as_str().parse::<u16>().ok())
                == Some(year)
        {
            entry.score += 5;
        }
    }

    // Score desc; ties break toward the non-dub post (sub posts carry
    // more server variety on this site).
    ranked.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            a.post
                .slug
                .ends_with("-dub")
                .cmp(&b.post.slug.ends_with("-dub"))
        })
    });
    if ranked.is_empty() || ranked[0].score < 50 {
        return Vec::new();
    }
    ranked
}

/// The season number a slug advertises — `-season-N` anywhere, then
/// word-form ordinals (`-2nd-season`).
fn slug_season_of(slug: &str) -> Option<u32> {
    for pattern in [&*SLUG_SEASON, &*SLUG_ORDINAL_SEASON] {
        if let Some(captures) = pattern.captures(slug).ok().flatten()
            && let Some(group) = captures.get(1)
            && let Ok(season) = group.as_str().parse::<u32>()
        {
            return Some(season);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Detail + episode pages
// ---------------------------------------------------------------------------

/// The detail page's episode link plus its `og:title` — the port of
/// `findEpisodeUrl` (movies fall back to the `episode-1` page).
async fn find_episode_url(
    ctx: &ResolveCtx<'_>,
    detail_url: &str,
    episode: Option<u32>,
) -> (Option<Url>, Option<String>) {
    let Ok(url) = Url::parse(detail_url) else {
        return (None, None);
    };
    let Some(html) = fetch_text(ctx, &url, None, PAGE_TIMEOUT).await else {
        return (None, None);
    };
    let title = OG_TITLE
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string());

    let episode = episode.unwrap_or(1).max(1);
    for pattern in episode_link_regexes(episode) {
        if let Some(captures) = pattern.captures(&html).ok().flatten()
            && let Some(group) = captures.get(1)
            && let Ok(link) = Url::parse(group.as_str())
        {
            return (Some(link), title);
        }
    }
    (None, title)
}

/// The episode-link regex ladder — exact, relaxed, then re-uploaded
/// `-episode-N-<counter>` slugs (Task 67), `/i` on all three.
fn episode_link_regexes(episode: u32) -> Vec<Regex> {
    let escaped = episode.to_string();
    let patterns = [
        format!(r#"href="(https://animotvslash\.org/[a-z0-9%-]*-episode-{escaped}/)""#),
        format!(r#"href="(https://animotvslash\.org/[^"]*-episode-{escaped}/?)""#),
        format!(r#"href="(https://animotvslash\.org/[a-z0-9%-]*-episode-{escaped}-\d+/?)""#),
    ];
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(&format!("(?i){pattern}")).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// The episode page's mirror list
// ---------------------------------------------------------------------------

/// The episode page's mirrors, resolved in parallel — the port of
/// `parseEpisodePage`.
async fn parse_episode_page(ctx: &ResolveCtx<'_>, ep_url: &Url) -> Vec<ResolvedEntry> {
    let referer = format!("{BASE_URL}/");
    let Some(html) = fetch_text(ctx, ep_url, Some(&referer), PAGE_TIMEOUT).await else {
        return Vec::new();
    };
    let Some(select) = MIRROR_SELECT
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())
    else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for captures in OPTION.captures_iter(&select).flatten() {
        let value = captures.get(1).map_or("", |group| group.as_str());
        if value.is_empty() {
            continue;
        }
        let Some(embed_html) =
            decode_base64_lenient(value).and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        let raw_label = captures.get(2).map_or("", |group| group.as_str());
        let label = collapse_spaces(raw_label);
        // Bucket from the label prefix: "Sub - X" | "Softsub - X" |
        // "Dub - X".
        let bucket = if starts_with_ci(&label, "softsub") {
            Bucket::SoftSub
        } else if starts_with_ci(&label, "dub") {
            Bucket::Dub
        } else {
            Bucket::Sub
        };
        let server = strip_bucket_prefix(&label);
        entries.push(MirrorEntry {
            embed_html,
            bucket,
            server,
        });
    }

    // Classify + resolve in parallel; a div config can fan out to
    // multiple quality tiers, so flatten.
    let settled =
        futures::future::join_all(entries.iter().map(|entry| resolve_entry(ctx, entry))).await;
    settled.into_iter().flatten().flatten().collect()
}

/// One mirror option awaiting resolution.
struct MirrorEntry {
    /// The base64-decoded embed HTML.
    embed_html: String,
    /// The audio bucket from the option label.
    bucket: Bucket,
    /// The server display name from the option label.
    server: String,
}

/// Resolve one mirror entry — the port of `resolveEntry`.
async fn resolve_entry(ctx: &ResolveCtx<'_>, entry: &MirrorEntry) -> Option<Vec<ResolvedEntry>> {
    // The torrent/P2P exclusion first (the no-torrent rule), plus the
    // unresolvable heads.
    if SKIP_HOSTS.is_match(&entry.embed_html).unwrap_or(false) {
        return None;
    }

    // Site player-config DIVs — direct URLs.
    if entry.embed_html.trim_start().starts_with("<div") {
        return resolve_site_player(entry);
    }

    // IFRAME embeds.
    let src = IFRAME_SRC
        .captures(&entry.embed_html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())?;
    let iframe_url = src.replace("&#038;", "&").replace("&amp;", "&");
    let lower = iframe_url.to_lowercase();
    if lower.contains("vidara.to") {
        return resolve_vidara_post(ctx, &iframe_url, entry)
            .await
            .map(|resolved| vec![resolved]);
    }
    if lower.contains("minochinos.com")
        || lower.contains("vidhide")
        || lower.contains("callistanise")
    {
        return resolve_vidhide(ctx, &iframe_url, entry)
            .await
            .map(|resolved| vec![resolved]);
    }
    if lower.contains("megaplay.buzz") {
        return resolve_megaplay(ctx, &iframe_url, entry)
            .await
            .map(|resolved| vec![resolved]);
    }
    None
}

/// One direct-URL candidate of a site player config: the URL, its
/// quality label, and its hotlink headers.
type DirectCandidate = (String, String, Vec<(String, String)>);

/// The site player-config DIVs and the aiovg player-embed variant —
/// the direct-URL shapes of a `<div>` embed (no fetches: the config
/// base64 carries the URLs).
fn resolve_site_player(entry: &MirrorEntry) -> Option<Vec<ResolvedEntry>> {
    let Some((kind, config)) = decode_player_config(&entry.embed_html) else {
        // The aiovg player-embed variant: `/player-embed/id/N/
        // ?mp4=<base64 url>` — the parameter IS the direct MP4 URL.
        if let Some(captures) = MP4_PARAM.captures(&entry.embed_html).ok().flatten()
            && let Some(b64) = captures.get(1)
            && let Some(direct) = decode_base64_lenient(b64.as_str())
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|direct| direct.starts_with("http"))
        {
            let quality = QUALITY_P
                .captures(&direct)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map_or_else(|| "1080p".to_string(), |group| group.as_str().to_string());
            return Some(vec![ResolvedEntry {
                url: direct,
                quality,
                headers: referer_headers(),
                subtitles: Vec::new(),
                bucket: entry.bucket,
                server: entry.server.clone(),
            }]);
        }
        return None;
    };

    let mut out: Vec<DirectCandidate> = Vec::new();
    match kind {
        "jw" => {
            if let Some(url) = string_field(&config, "url").filter(|url| url.starts_with("http")) {
                out.push((url.to_string(), "1080p".to_string(), referer_headers()));
            }
        }
        "plyr" => {
            if let Some(url) = string_field(&config, "url").filter(|url| url.starts_with("http")) {
                // videas hlsv1 has an INVERTED hotlink gate: 200 with
                // the Origin alone, 403 the moment a Referer rides
                // along. Ship Origin only.
                out.push((url.to_string(), "720p".to_string(), origin_headers()));
            }
        }
        _ => {
            // vidstack: the MP4 tiers, best first.
            for key in ["url_1080", "url_720", "url_480", "url"] {
                if let Some(url) = string_field(&config, key).filter(|url| url.starts_with("http"))
                {
                    let quality = if key == "url" {
                        "1080p".to_string()
                    } else {
                        format!("{}p", &key[4..])
                    };
                    out.push((url.to_string(), quality, referer_headers()));
                }
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(
        out.into_iter()
            .map(|(url, quality, headers)| ResolvedEntry {
                url,
                quality,
                headers,
                subtitles: Vec::new(),
                bucket: entry.bucket,
                server: entry.server.clone(),
            })
            .collect(),
    )
}

/// The `{(Referer, site)}` hotlink pair.
fn referer_headers() -> Vec<(String, String)> {
    vec![("Referer".to_string(), format!("{BASE_URL}/"))]
}

/// The `{(Origin, site)}` inverted-gate pair.
fn origin_headers() -> Vec<(String, String)> {
    vec![("Origin".to_string(), BASE_URL.to_string())]
}

/// `jw-player|plyr-player|vidstack-player/<base64 json>` — the port
/// of `decodePlayerConfig`.
fn decode_player_config(div_html: &str) -> Option<(&'static str, Value)> {
    let captures = PLAYER_CONFIG.captures(div_html).ok().flatten()?;
    let kind = match captures.get(1)?.as_str() {
        "jw-player" => "jw",
        "plyr-player" => "plyr",
        _ => "vidstack",
    };
    let payload = captures.get(2)?.as_str();
    let bytes = decode_base64_lenient(payload)?;
    serde_json::from_slice(&bytes)
        .ok()
        .map(|config| (kind, config))
}

/// The `"{key}": "value"` lookup of a player config.
fn string_field<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key)?.as_str()
}

// ---------------------------------------------------------------------------
// Per-host iframe resolvers
// ---------------------------------------------------------------------------

/// `vidara.to/e/<filecode>` → POST `/api/stream` — the port of
/// `resolveVidaraPost` (`device: "desktop"`, the embed's own
/// `Origin`/`Referer`). The API's `title` field is cut (nothing
/// consumed it).
async fn resolve_vidara_post(
    ctx: &ResolveCtx<'_>,
    embed_url: &str,
    entry: &MirrorEntry,
) -> Option<ResolvedEntry> {
    let filecode = embed_url.split("/e/").nth(1)?.split(['?', '#']).next()?;
    if filecode.is_empty() {
        return None;
    }
    let parsed = Url::parse(embed_url).ok()?;
    let origin = parsed.origin().ascii_serialization();
    let api = Url::parse(&format!("{origin}/api/stream")).ok()?;
    let body = format!(r#"{{"filecode":"{filecode}","device":"desktop"}}"#);
    let request = FetchRequest::post(api, body)
        .with_header("User-Agent", UA)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_header("Origin", &origin)
        .with_header("Referer", embed_url)
        .with_timeout(FETCH_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let data: Value = serde_json::from_str(&response.body).ok()?;
    let url = data.get("streaming_url")?.as_str()?.to_string();
    let subtitles = data
        .get("subtitles")
        .and_then(Value::as_array)
        .map(|subtitles| {
            subtitles
                .iter()
                .filter_map(|subtitle| {
                    let url = subtitle.get("file_path")?.as_str()?;
                    Some(ResolvedSubtitle {
                        url: url.to_string(),
                        lang: subtitle
                            .get("language")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ResolvedEntry {
        url,
        quality: "1080p".to_string(),
        headers: Vec::new(),
        subtitles,
        bucket: entry.bucket,
        server: entry.server.clone(),
    })
}

/// `VidHide` embed → unpack the packed player → the first master
/// playlist that answers an HLS probe — the port of `resolveVidHide`
/// (m3u8 candidates first; the hls2 host intermittently 403s).
async fn resolve_vidhide(
    ctx: &ResolveCtx<'_>,
    embed_url: &str,
    entry: &MirrorEntry,
) -> Option<ResolvedEntry> {
    let url = Url::parse(embed_url).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Referer", format!("{BASE_URL}/"))
        .with_timeout(FETCH_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let final_url = response.url;
    let unpacked = unpack_eval(&response.body)?;
    // The cjs unescapes `\X` sequences in the packer payload before
    // substituting; the shared unpacker leaves them, so mirror the
    // unescape on the result.
    let unpacked = unescape_backslashes(&unpacked);
    let mut candidates: Vec<String> = MASTER_URLS
        .find_iter(&unpacked)
        .filter_map(|found| found.ok().map(|found| found.as_str().to_string()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // `.m3u8`-bearing candidates first (the JS sort), stable.
    candidates.sort_by_key(|candidate| !candidate.to_lowercase().contains(".m3u8"));

    let origin = final_url.origin().ascii_serialization();
    let referer = format!("{origin}/");
    for candidate in candidates.iter().take(3) {
        if probe_hls(ctx, candidate, &referer).await {
            return Some(ResolvedEntry {
                url: candidate.clone(),
                quality: "1080p".to_string(),
                headers: vec![("Referer".to_string(), referer)],
                subtitles: Vec::new(),
                bucket: entry.bucket,
                server: entry.server.clone(),
            });
        }
    }
    None
}

/// One master-playlist probe: 2xx and `#EXTM3U` in the first 200
/// chars — the port of `probeHls`.
async fn probe_hls(ctx: &ResolveCtx<'_>, url: &str, referer: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Referer", referer)
        .with_timeout(PROBE_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return false;
    };
    if !response.is_success() {
        return false;
    }
    response
        .body
        .chars()
        .take(200)
        .collect::<String>()
        .contains("#EXTM3U")
}

/// `megaplay.buzz/stream/…` → `data-id` → `getSourcesNew` → the AES
/// `enc` decrypt (or the plaintext `sources`) plus the subtitle
/// tracks — the port of `resolveMegaPlay`.
async fn resolve_megaplay(
    ctx: &ResolveCtx<'_>,
    stream_url: &str,
    entry: &MirrorEntry,
) -> Option<ResolvedEntry> {
    let parsed = Url::parse(stream_url).ok()?;
    let referer = format!("{BASE_URL}/");
    let html = fetch_text(ctx, &parsed, Some(&referer), FETCH_TIMEOUT).await?;
    let id = DATA_ID
        .captures(&html)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .map(|group| group.as_str().to_string())?;
    let api = Url::parse(&format!("{MEGAPLAY_API}?id={id}")).ok()?;
    let request = FetchRequest::get(api)
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json")
        .with_header("Referer", stream_url)
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_timeout(FETCH_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let data: Value = serde_json::from_str(&response.body).ok()?;
    let file = if let Some(enc) = data.get("enc").and_then(Value::as_str) {
        decrypt_megaplay_enc(enc)?
    } else {
        match data.get("sources")? {
            Value::Object(object) => object.get("file")?.as_str()?.to_string(),
            Value::String(url) => url.clone(),
            _ => return None,
        }
    };
    let tracks = data
        .get("tracks")
        .and_then(Value::as_array)
        .map(|tracks| {
            tracks
                .iter()
                .filter_map(|track| {
                    let url = track.get("file")?.as_str()?;
                    let lang = track
                        .get("label")
                        .and_then(Value::as_str)
                        .or_else(|| track.get("language").and_then(Value::as_str));
                    Some(ResolvedSubtitle {
                        url: url.to_string(),
                        lang: lang.map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ResolvedEntry {
        url: file,
        quality: "1080p".to_string(),
        headers: vec![("Referer".to_string(), "https://megaplay.buzz/".to_string())],
        subtitles: tracks,
        bucket: entry.bucket,
        server: entry.server.clone(),
    })
}

// ---------------------------------------------------------------------------
// Stream building
// ---------------------------------------------------------------------------

/// One resolved mirror → the raw Nuvio stream — the port of
/// `buildStream` (the audio-track stamp drives the per-card language
/// flags; `meta.category` carries the sub/dub marker).
fn build_stream(entry: &ResolvedEntry, anime_title: &str, episode: Option<u32>) -> NuvioStream {
    let bucket = entry.bucket.label();
    let name = format!("AniMoTV\n{bucket} {} {}", entry.quality, entry.server);
    let title = match episode {
        Some(episode) => format!(
            "{anime_title} - Episode {episode} ({bucket} · {})",
            entry.server
        ),
        None => format!("{anime_title} ({bucket} · {})", entry.server),
    };
    let mut stream = NuvioStream::new(entry.url.clone())
        .with_name(name)
        .with_title(title)
        .with_quality(entry.quality.clone());
    for (header, value) in &entry.headers {
        stream = stream.with_header(header.clone(), value.clone());
    }
    for subtitle in &entry.subtitles {
        let lang = subtitle.lang.clone().unwrap_or_else(|| "en".to_string());
        let name = if subtitle.lang.is_some() {
            lang.clone()
        } else {
            "English".to_string()
        };
        stream = stream.with_subtitle(NuvioSubtitle {
            url: Some(subtitle.url.clone()),
            lang: Some(lang),
            name: Some(name),
            ..NuvioSubtitle::default()
        });
    }
    stream.audio_tracks = Some(Value::Array(vec![Value::String(
        if entry.bucket == Bucket::Dub {
            "English".to_string()
        } else {
            "Japanese".to_string()
        },
    )]));
    stream.meta = Some(serde_json::json!({ "category": entry.bucket.key() }));
    stream
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// GET a URL as text — the scraper's `fetchText` (`None` on any
/// failure; the JS logged and returned `''`).
async fn fetch_text(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_timeout(timeout);
    if let Some(referer) = referer {
        request = request.with_header("Referer", referer);
    }
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    Some(response.body)
}

/// GET a URL as JSON — the scraper's `fetchJson`.
async fn fetch_json(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: Option<&str>,
    timeout: Duration,
) -> Option<Value> {
    let mut request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json")
        .with_timeout(timeout);
    if let Some(referer) = referer {
        request = request.with_header("Referer", referer);
    }
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    serde_json::from_str(&response.body).ok()
}

/// Node's tolerant base64: both alphabets, any padding position —
/// the option values, player configs, and aiovg parameters mix
/// standard and URL-safe forms.
fn decode_base64_lenient(text: &str) -> Option<Vec<u8>> {
    let mut cleaned: String = text
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '=')
        .map(|character| match character {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while !cleaned.len().is_multiple_of(4) {
        cleaned.push('=');
    }
    STANDARD.decode(cleaned).ok()
}

/// `p.replace(/\\(.)/g, '$1')` — the packer-payload unescape; a
/// trailing lone backslash has no match and is dropped.
fn unescape_backslashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\\'
            && let Some(escaped) = chars.next()
        {
            out.push(escaped);
        } else if character != '\\' {
            out.push(character);
        }
    }
    out
}

/// The scraper's `normalizeTitle`: lowercase, combining marks and
/// HTML entities out, non-alphanumerics to spaces, stop words
/// (`the|a|an|tv|season|part|specials?|movie|ova|ona|oad|dub`)
/// dropped, whitespace collapsed. Precomposed accents become spaces —
/// NFD is unavailable without an extra dependency (the `allwish`
/// precedent).
fn normalize_title(text: &str) -> String {
    let lower = text.to_lowercase();
    // `&[a-z]+;|&#\d+;` → ' ' before the character filter, so entity
    // digits never leak into the result.
    let entities = HTML_ENTITY.replace_all(&lower, " ").to_string();
    let mut filtered = String::with_capacity(entities.len());
    for character in entities.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            filtered.push(character);
        } else if ('\u{0300}'..='\u{036f}').contains(&character) {
            // A combining mark — dropped, like the NFD strip.
        } else {
            filtered.push(' ');
        }
    }
    let stopwords = [
        "the", "a", "an", "tv", "season", "part", "special", "specials", "movie", "ova", "ona",
        "oad", "dub",
    ];
    filtered
        .split(' ')
        .filter(|word| !word.is_empty() && !stopwords.contains(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The wrapper's `cleanTitle` — a trailing parenthesized qualifier
/// stripped; the raw title survives an empty result.
fn clean_title(raw: &str) -> String {
    let stripped = TRAILING_PARENS.replace(raw, "").trim().to_string();
    if stripped.is_empty() {
        raw.trim().to_string()
    } else {
        stripped
    }
}

/// `\s+` → single spaces, trimmed.
fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case-insensitive `startsWith`.
fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.to_lowercase().starts_with(prefix)
}

/// `label.replace(/^(sub|softsub|dub)\s*-\s*/i, '').trim() ||
/// 'mirror'` — the server name behind the bucket prefix.
fn strip_bucket_prefix(label: &str) -> String {
    let stripped = BUCKET_PREFIX.replace(label, "").trim().to_string();
    if stripped.is_empty() {
        "mirror".to_string()
    } else {
        stripped
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

impl AniMoTVSlash {
    async fn resolve_by_title(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let title = display_title(&name, year, media);
        // The scraper searches on the title with a trailing
        // parenthesized qualifier stripped.
        let clean = clean_title(&name);

        let sweep = async {
            self.sweep(ctx, &clean, year, media.season, media.episode)
                .await
        };
        let raw = with_deadline(sweep, SWEEP_DEADLINE)
            .await
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(SourceError::NotFound);
        }
        // Per-card language flags arrive via the audio-track stamp
        // (Japanese for sub/softsub, English for dub); the source-level
        // codes only apply when a card carries none.
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
    /// path, so search requests and TMDB calls can be scripted by
    /// path alone.
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

    /// The fixture media: Solo Leveling S01E02.
    const TMDB_ID: u64 = 123_456;

    /// The megaplay ground-truth `enc` blob (from the
    /// `nuvio::megaplay` decrypt corpus) — decrypts to the master URL
    /// below.
    const MEGAPLAY_ENC: &str = "wdeBruh3qqn_i5wUNnyaPW3GxFWAz0PzUtHz-gGMUfX7M-F6BNk8LV386Hu9tS4mFUNr-s_AJy1VBvh7NDicJy9Oxe9Oq___572UgI-DEIA";
    /// What `MEGAPLAY_ENC` decrypts to.
    const MEGAPLAY_FILE: &str = "https://megap.akirax.buzz/hls/abc123/master.m3u8?token=xyz";

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> AniMoTVSlash {
        AniMoTVSlash::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
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

    /// Base64 of a player-config payload (the site's alphabet mixes
    /// standard and URL-safe forms; both decode).
    fn b64(payload: &str) -> String {
        STANDARD.encode(payload)
    }

    /// The `VidHide` packed-JS page — the `.txt` master appears first in
    /// the unpacked text so the m3u8-first sort has to reorder.
    /// Payload tokens: `1` → the m3u8 URL, `2` → the `.txt` mirror.
    fn vidhide_page() -> String {
        r#"<html><body><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\b'+e(c)+'\b','g'),k[c])}}return p}('sources:[{file:"2"},{file:"1"}]',10,4,'0|https://acek-cdn.example/hls2/master.m3u8|https://mirror.example/hls3/master.txt|'.split('|'),0,{}))</script></body></html>"#.to_string()
    }

    /// The episode page: nine mirror options — every resolver family
    /// plus the skipped P2P and unresolvable embeds.
    fn episode_page() -> String {
        let jw = format!(
            r#"<div class="player" data-config="jw-player/{}"></div>"#,
            b64(r#"{"url":"https://rumble.example/hls/master.m3u8"}"#)
        );
        let plyr = format!(
            r#"<div class="player" data-config="plyr-player/{}"></div>"#,
            b64(r#"{"url":"https://videas.example/hlsv1/playlist.m3u8"}"#)
        );
        let vidstack = format!(
            r#"<div class="player" data-config="vidstack-player/{}"></div>"#,
            b64(
                r#"{"url_1080":"https://videas.example/d/1080.mp4","url_720":"https://videas.example/d/720.mp4","url_480":"https://videas.example/d/480.mp4"}"#
            )
        );
        let aiovg = format!(
            r#"<div><iframe src="/player-embed/id/123/?mp4={}"></iframe></div>"#,
            b64("https://videas.example/d/animo-o-720p.mp4")
        );
        let vidara = r#"<iframe src="https://vidara.to/e/abc123?z=1"></iframe>"#.to_string();
        let vidhide =
            r#"<iframe src="https://minochinos.com/embed/vidhide123"></iframe>"#.to_string();
        let megaplay =
            r#"<iframe src="https://megaplay.buzz/stream/ani/21/2/dub"></iframe>"#.to_string();
        let p2p =
            r#"<iframe src="https://animotvslash.p2pplay.pro/embed/xyz"></iframe>"#.to_string();
        let moon = r#"<iframe src="https://bysezoxexe.com/e/xyz"></iframe>"#.to_string();

        let option = |label: &str, embed: &str| {
            format!(r#"<option value="{}">{}</option>"#, b64(embed), label)
        };
        let options = [
            option("Sub - ANIMO-M", &jw),
            option("Softsub - ANIMO-H", &plyr),
            option("Sub - ANIMO-D", &vidstack),
            option("Sub - ANIMO-O", &aiovg),
            option("Sub - Vidara", &vidara),
            option("Sub - VidHide", &vidhide),
            option("Dub - Megaplay", &megaplay),
            option("Sub - Animo", &p2p),
            option("Sub - Moon", &moon),
        ]
        .join("\n");
        format!(
            r#"<html><body><select class="mirror" name="mirror">{options}</select></body></html>"#
        )
    }

    /// TMDB + site pages for the fixture media.
    fn pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"Solo Leveling","first_air_date":"2024-01-06"}"#,
        )
        .page(
            "/wp-json/wp/v2/search",
            200,
            r#"[{"title":"Solo Leveling","url":"https://animotvslash.org/anime/solo-leveling/"},{"title":"Solo Leveling Season 2","url":"https://animotvslash.org/anime/solo-leveling-season-2/"}]"#,
        )
        .page(
            "/anime/solo-leveling/",
            200,
            r#"<html><head><meta property="og:title" content="Solo Leveling"></head><body><a href="https://animotvslash.org/solo-leveling-episode-2/">Ep 2</a></body></html>"#,
        )
        .page("/solo-leveling-episode-2/", 200, episode_page())
        .page(
            "/api/stream",
            200,
            r#"{"streaming_url":"https://vidara.example/hls/ep2.m3u8","title":"Solo Leveling E2","subtitles":[{"file_path":"https://vidara.example/subs/en.vtt","language":"English"}]}"#,
        )
        .page("/embed/vidhide123", 200, vidhide_page())
        .page(
            "/hls2/master.m3u8",
            200,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000\nhls2/index.m3u8\n",
        )
        .page(
            "/stream/ani/21/2/dub",
            200,
            r#"<html><body><div class="player" data-id="54321"></div></body></html>"#,
        )
        .page(
            "/stream/getSourcesNew",
            200,
            format!(
                r#"{{"enc":"{MEGAPLAY_ENC}","tracks":[{{"file":"https://megaplay.buzz/subs/en.vtt","label":"English"}},{{"file":"https://megaplay.buzz/subs/ja.vtt","label":"Japanese"}}]}}"#
            ),
        )
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "animotvslash");
        assert_eq!(info.label, "AniMoTVSlash");
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
            Some("https://animotvslash.org/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_every_server_family_and_skips_the_rest() -> Result<(), SourceError> {
        let mock = Arc::new(pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        // jw + plyr + 3 vidstack tiers + aiovg + vidara + vidhide +
        // megaplay = 9 cards; the p2p and Moon embeds are skipped.
        assert_eq!(streams.len(), 9);

        // The jw card: 1080p HLS with the site Referer and the
        // Japanese (sub) language flag.
        let jw = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://rumble.example/hls/master.m3u8")
            .unwrap_or_else(|| panic!("the jw card exists"));
        assert_eq!(jw.format, Format::Hls);
        assert_eq!(jw.meta.resolution, Some(1080));
        assert_eq!(
            jw.meta.request_headers.get("Referer").map(String::as_str),
            Some("https://animotvslash.org/")
        );
        assert!(jw.meta.languages.contains(&CountryCode::Ja));
        assert!(
            jw.label
                .as_deref()
                .is_some_and(|label| label.contains("SUB · ANIMO-M"))
        );

        // The plyr card: Origin-only inverted gate — no Referer rides
        // the stream (the shared layer attaches Origin only under a
        // Referer).
        let plyr = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://videas.example/hlsv1/playlist.m3u8")
            .unwrap_or_else(|| panic!("the plyr card exists"));
        assert!(plyr.meta.request_headers.is_empty());

        // The vidstack MP4 tiers.
        for (url, height) in [
            ("https://videas.example/d/1080.mp4", 1080),
            ("https://videas.example/d/720.mp4", 720),
            ("https://videas.example/d/480.mp4", 480),
        ] {
            let stream = streams
                .iter()
                .find(|stream| stream.url.as_str() == url)
                .unwrap_or_else(|| panic!("the {url} tier exists"));
            assert_eq!(stream.format, Format::Mp4);
            assert_eq!(stream.meta.resolution, Some(height));
        }

        // The aiovg direct MP4 — quality from the URL.
        let aiovg = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://videas.example/d/animo-o-720p.mp4")
            .unwrap_or_else(|| panic!("the aiovg card exists"));
        assert_eq!(aiovg.meta.resolution, Some(720));

        // The vidara card: HLS + its API's subtitle, no headers (the
        // scraper stamps none for this host).
        let vidara = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://vidara.example/hls/ep2.m3u8")
            .unwrap_or_else(|| panic!("the vidara card exists"));
        assert_eq!(vidara.format, Format::Hls);
        assert!(vidara.meta.request_headers.is_empty());
        assert_eq!(
            vidara.meta.subtitles[0].url.as_str(),
            "https://vidara.example/subs/en.vtt"
        );

        // The vidhide card: the probed m3u8 (not the `.txt` master)
        // with the embed origin's Referer.
        let vidhide = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://acek-cdn.example/hls2/master.m3u8")
            .unwrap_or_else(|| panic!("the vidhide card exists"));
        assert_eq!(
            vidhide
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://minochinos.com/")
        );

        // The megaplay card: the decrypted master with the megaplay
        // Referer, the English (dub) language flag, and both subtitle
        // tracks. The dub bucket sorts last.
        let megaplay = streams
            .iter()
            .find(|stream| stream.url.as_str() == MEGAPLAY_FILE)
            .unwrap_or_else(|| panic!("the megaplay card exists"));
        assert_eq!(
            megaplay
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        assert!(megaplay.meta.languages.contains(&CountryCode::En));
        assert_eq!(megaplay.meta.subtitles.len(), 2);
        assert_eq!(streams.last(), Some(megaplay));

        // Bucket order: sub entries first, softsub second, dub last —
        // the plyr card is the second-to-last.
        let plyr_index = streams
            .iter()
            .position(|stream| stream.url.as_str() == "https://videas.example/hlsv1/playlist.m3u8")
            .unwrap_or_else(|| panic!("the plyr card is ordered"));
        assert_eq!(plyr_index, streams.len() - 2);

        // The skipped hosts never fetched beyond the episode page.
        assert!(!mock.requests().iter().any(|request| {
            request
                .url
                .host_str()
                .is_some_and(|host| host.contains("p2pplay") || host.contains("bysezoxexe"))
        }));
        // Every card carries the provider identity and the 5 min ttl.
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("animotvslash"))
        );
        assert_eq!(streams[0].ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn a_movie_reference_resolves_the_episode_one_page() -> Result<(), SourceError> {
        // The movie variant: a jw-only episode page, TMDB movie
        // details, and no episode suffix on the label.
        let embed = format!(
            r#"<div data-config="jw-player/{}"></div>"#,
            b64(r#"{"url":"https://rumble.example/hls/suzume.m3u8"}"#)
        );
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/movie/{TMDB_ID}"),
                    200,
                    r#"{"title":"Suzume","release_date":"2022-11-11"}"#,
                )
                .page(
                    "/wp-json/wp/v2/search",
                    200,
                    r#"[{"title":"Suzume","url":"https://animotvslash.org/anime/suzume/"}]"#,
                )
                .page(
                    "/anime/suzume/",
                    200,
                    r#"<meta property="og:title" content="Suzume (Movie)"><a href="https://animotvslash.org/suzume-episode-1/">Watch</a>"#,
                )
                .page(
                    "/suzume-episode-1/",
                    200,
                    format!(
                        r#"<select class="mirror"><option value="{}">Sub - ANIMO-M</option></select>"#,
                        b64(&embed)
                    ),
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://rumble.example/hls/suzume.m3u8"
        );
        // Movies carry no " - Episode N" in the title.
        assert!(
            !streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("Episode"))
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
            name: "Solo Leveling".to_string(),
            year: Some(2024),
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

        assert_eq!(streams.len(), 9);
        assert!(mock.requests().iter().all(|request| {
            !request
                .url
                .host_str()
                .is_some_and(|host| host.contains("themoviedb"))
        }));
        Ok(())
    }

    #[tokio::test]
    async fn a_search_without_a_confident_match_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Solo Leveling","first_air_date":"2024-01-06"}"#,
                )
                .page(
                    "/wp-json/wp/v2/search",
                    200,
                    r#"[{"title":"Frieren","url":"https://animotvslash.org/anime/frieren/"}]"#,
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
    async fn a_band_sibling_rescues_a_p2p_only_leader() -> Result<(), SourceError> {
        // The top-scoring post resolves zero servers (all P2P); the
        // sibling within 5 points carries the real player.
        let p2p_only = r#"<iframe src="https://animotvslash.p2pplay.pro/embed/xyz"></iframe>"#;
        let real = format!(
            r#"<div data-config="jw-player/{}"></div>"#,
            b64(r#"{"url":"https://rumble.example/hls/frieren.m3u8"}"#)
        );
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren","first_air_date":"2024-01-05"}"#,
                )
                .page(
                    "/wp-json/wp/v2/search",
                    200,
                    r#"[{"title":"Frieren","url":"https://animotvslash.org/anime/frieren/"},{"title":"Frieren","url":"https://animotvslash.org/anime/frieren-sub/"}]"#,
                )
                .page(
                    "/anime/frieren/",
                    200,
                    r#"<meta property="og:title" content="Frieren"><a href="https://animotvslash.org/frieren-episode-1/">Watch</a>"#,
                )
                .page(
                    "/anime/frieren-sub/",
                    200,
                    r#"<meta property="og:title" content="Frieren"><a href="https://animotvslash.org/frieren-sub-episode-1/">Watch</a>"#,
                )
                .page(
                    "/frieren-episode-1/",
                    200,
                    format!(
                        r#"<select class="mirror"><option value="{}">Sub - Animo</option></select>"#,
                        b64(p2p_only)
                    ),
                )
                .page(
                    "/frieren-sub-episode-1/",
                    200,
                    format!(
                        r#"<select class="mirror"><option value="{}">Sub - ANIMO-M</option></select>"#,
                        b64(&real)
                    ),
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 1))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://rumble.example/hls/frieren.m3u8"
        );
        assert!(
            streams[0]
                .label
                .as_deref()
                .is_some_and(|label| label.contains("ANIMO-M"))
        );
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

    #[test]
    fn builds_the_ladder_queries() {
        // Raw → punctuation-stripped → first-6 → first-3, deduped.
        let queries = ladder_queries("Demon Slayer: Kimetsu no Yaiba");
        assert_eq!(queries[0], "Demon Slayer: Kimetsu no Yaiba");
        assert_eq!(queries[1], "Demon Slayer Kimetsu no Yaiba");
        // A short raw title collapses to a single rung.
        assert_eq!(ladder_queries("Naruto").len(), 1);
        assert!(ladder_queries("ab").is_empty());
    }

    #[test]
    fn ranks_with_season_awareness_and_ordinals() {
        let post = |slug: &str| SeriesPost {
            title: format!("Title {slug}"),
            url: format!("https://animotvslash.org/anime/{slug}/"),
            slug: slug.to_string(),
        };
        let posts = vec![
            post("title"),
            post("title-season-2"),
            post("title-2nd-season"),
        ];

        // An S2 request prefers the season posts.
        let ranked = rank_series(&posts, "Title", None, Some(2));
        assert!(
            ranked[0].post.slug == "title-season-2" || ranked[0].post.slug == "title-2nd-season"
        );
        // An S1 request penalizes them.
        let ranked = rank_series(&posts, "Title", None, Some(1));
        assert_eq!(ranked[0].post.slug, "title");

        // No confident match → empty (best score below 50). A post
        // merely containing the query word would still score 80 — the
        // weak title shares no word at all.
        let weak = vec![SeriesPost {
            title: "Completely Different".to_string(),
            url: "https://animotvslash.org/anime/completely-different/".to_string(),
            slug: "completely-different".to_string(),
        }];
        assert!(rank_series(&weak, "Title", None, None).is_empty());

        // A tie breaks toward the non-dub slug.
        let twins = vec![post("twin-dub"), post("twin")];
        let ranked = rank_series(&twins, "Twin", None, None);
        assert_eq!(ranked[0].post.slug, "twin");
    }

    #[test]
    fn cleans_and_normalizes_titles() {
        assert_eq!(clean_title("Movie Name (2024)"), "Movie Name");
        // An empty strip falls back to the raw title.
        assert_eq!(clean_title("(2024)"), "(2024)");
        // `movie`/`dub`/`the`/`a` are all on the stopword list — the
        // normalized form is empty.
        assert_eq!(normalize_title("The Movie: A Dub"), "");
        assert_eq!(normalize_title("Solo Leveling"), "solo leveling");
        // Entities drop out; precomposed accents become spaces (NFD
        // is unavailable).
        assert_eq!(normalize_title("A&#8217;B &amp; C"), "b c");
        assert_eq!(normalize_title("Shippūden"), "shipp den");
    }

    #[test]
    fn decodes_lenient_base64() {
        // Standard, URL-safe, and unpadded forms all decode.
        assert_eq!(
            decode_base64_lenient(&b64("jw")).as_deref(),
            Some(b"jw".as_slice())
        );
        assert_eq!(
            decode_base64_lenient("anN3").as_deref(),
            Some(b"jsw".as_slice())
        );
        assert!(decode_base64_lenient("!!").is_none());
    }

    #[test]
    fn strips_bucket_prefixes_into_server_names() {
        assert_eq!(strip_bucket_prefix("Sub - ANIMO-M"), "ANIMO-M");
        assert_eq!(strip_bucket_prefix("Softsub - ANIMO-H"), "ANIMO-H");
        assert_eq!(strip_bucket_prefix("Dub - Megaplay"), "Megaplay");
        assert_eq!(strip_bucket_prefix("Plain"), "Plain");
        assert_eq!(strip_bucket_prefix("Sub - "), "mirror");
    }
}
