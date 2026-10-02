//! `AniChan`: anichan.to — anime sub/dub HLS streams (up to 1080p).
//!
//! Ports `src/source/AniChan.js` — an `AniList`-keyed JSON API, no `.cjs`
//! scraper behind this one:
//!
//! 1. resolve the `AniList` id for the TMDB title — the shared
//!    mapping service's `AniList` client (`POST https://graphql.anilist.co`,
//!    `Page(perPage: 5)` of
//!    `media(type: ANIME, sort: [SEARCH_MATCH, POPULARITY_DESC])`
//!    `{ id idMal title { romaji english } format }`), falling back to
//!    anichan's own search index (`GET /search?q=` →
//!    `/anime/{anilistId}/{slug}` links) and finally Jikan
//!    (`api.jikan.moe/v4/anime`), each converted to the `AniList` shape;
//! 2. score candidates on their English/romaji titles (exact = 100,
//!    containment = 90·len-ratio, first-word = 65 + length bonus);
//!    nothing below 60 ships;
//! 3. `GET /api/watch/episodes?anilistId=` → `{ dubAvailable }` (an
//!    unparseable answer zeroes the whole source, like upstream);
//! 4. `GET /api/watch/servers?anilistId=&episode=&type={sub|dub}` →
//!    `{ servers: [{ stream }] }` — relative `stream` paths join
//!    `anichan.to`; two bounded retries (800 ms / 1600 ms backoff) when
//!    the servers payload comes back empty, one per type;
//! 5. each server card ships the ORIGINAL stream URL (it carries its own
//!    `sig`/`exp` auth and plays without a cookie) as HLS with
//!    height 1080.
//!
//! The 2026-09 session gate: `/api/watch/episodes` and `/api/watch/servers`
//! answer `401 {"detail":"session"}` without the `anichan_ws` cookie from
//! `POST /api/watch/session` (body `{"token":""}` — the Turnstile token may
//! be empty). The cookie value embeds its own expiry as the leading
//! dot-separated epoch, so it is cached and refreshed 5 minutes early; a
//! session rejection drops the cached cookie, mints a fresh one, and retries
//! once (upstream observed cached cookies 401-ing long before their embedded
//! expiry). Watch-API requests also need the
//! `Referer: https://anichan.to/watch/{anilistId}` + `X-Requested-With`
//! markers.
//!
//! Cuts and mappings (vs. upstream):
//!
//! - No `/proxy` routing: upstream flagged each card
//!   `nuvioProvider`/`nuvioForceHls` so its `NuvioExtractor` claimed the
//!   URL, buffered the m3u8, and rewrote the RELATIVE variant URLs
//!   (`/api/watch/m3u8?sh=…`) against `anichan.to`. There is no server
//!   here, so the direct URL ships and only the ambiguous-URL force-HLS
//!   hint survives as the `nuvioForceHls` behavior hint (the
//!   [`nuvio`](crate::nuvio) convention); clients resolving the playlist
//!   themselves must join variants against `anichan.to`.
//! - `meta.title` (`{title} (AniChan SUB)`) → [`Stream::label`].
//! - The Jikan fallback is ported verbatim even though it can never
//!   yield streams: its entries carry `id: null` (MAL ids, not `AniList`
//!   ids) and the winner without an `AniList` id bails — upstream oddity
//!   kept for request-shape fidelity.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; `normalize` still strips combining marks (U+0300–036F)
//!   and precomposed accents drop out — fine for romanized anime titles.
//! - The wrapper's per-type `try/catch` swallows are structural in the
//!   Rust port (a failed type contributes no cards, the other
//!   continues), and the three id resolvers map every failure onto an
//!   empty list like their JS `catch { return null }` blocks.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fancy_regex::Regex;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::mappings::MappingService;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaId, MediaRef, MediaType, SourceInfo, Stream, StreamMeta,
};
/// The provider id, upstream `this.id`.
const ID: &str = "anichan";
/// The display label, upstream `this.label`.
const LABEL: &str = "AniChan";
/// The site root (the 2026-09 domain: `anichan.net` 301s here), upstream
/// `BASE`/`this.baseUrl`.
const BASE_URL: &str = "https://anichan.to";
/// The Jikan (`MyAnimeList`) API root, upstream `JIKAN_API`.
const JIKAN_API: &str = "https://api.jikan.moe/v4";
/// The upstream browser UA (`UA`).
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Upstream `this.ttl` — the stream URLs carry short-lived `sig`/`exp`
/// tokens.
const TTL: Duration = Duration::from_mins(5);
/// One API call (upstream `timeout: { request: 15000 }`).
const API_TIMEOUT: Duration = Duration::from_secs(15);
/// The `AniChan`-search fallback timeout (upstream `10000`).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The Jikan fallback timeout (upstream `AbortSignal.timeout(10000)`).
const JIKAN_TIMEOUT: Duration = Duration::from_secs(10);
/// How early a still-valid cookie is refreshed (upstream `300` s).
const COOKIE_REFRESH_EARLY: Duration = Duration::from_secs(300);
/// The expiry fallback when the cookie value's leading epoch is not
/// parseable (upstream `now + 7000`).
const COOKIE_FALLBACK_TTL: Duration = Duration::from_secs(7000);
/// The first empty-servers retry backoff (upstream `800` ms).
const SERVERS_BACKOFF_1: Duration = Duration::from_millis(800);
/// The second empty-servers retry backoff (upstream `1600` ms).
const SERVERS_BACKOFF_2: Duration = Duration::from_millis(1600);

/// `/anime/{anilistId}/{slug}` links in the `AniChan` search index.
static ANIME_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/anime/(\d+)/([a-z0-9-]+)")
        .unwrap_or_else(|e| panic!("valid anime-link pattern: {e}"))
});

/// The `anichan_ws` cookie inside a `set-cookie` header.
static SESSION_COOKIE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"anichan_ws=([^;]+)")
        .unwrap_or_else(|e| panic!("valid session-cookie pattern: {e}"))
});

/// One cached session cookie with its expiry (upstream's module-level
/// `_acCookie`/`_acCookieExp` — per provider instance here, which the
/// engine constructs once).
#[derive(Clone)]
struct CachedCookie {
    /// The `anichan_ws` value.
    value: String,
    /// When the cookie stops being trusted.
    expires_at: Instant,
}

/// One `AniList`-shaped candidate from any of the three resolvers.
#[derive(Debug, Clone)]
struct Candidate {
    /// The `AniList` id (`None` for Jikan's MAL-only results).
    id: Option<u64>,
    /// The English title, when known.
    english: Option<String>,
    /// The romaji title, when known.
    romaji: Option<String>,
}

/// The `AniChan` provider.
pub struct AniChan {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The shared anime id-mapping service (arm/anilist).
    mappings: MappingService,
    /// The session cookie cache (upstream `_acCookie`).
    cookie: Mutex<Option<CachedCookie>>,
}

impl AniChan {
    /// A provider over the shared TMDB client and mapping service.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, mappings: MappingService) -> Self {
        Self {
            mappings,
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
            cookie: Mutex::new(None),
        }
    }

    /// The watch-session cookie, minting a fresh one when the cache is
    /// empty or inside the early-refresh window — the port of
    /// `getAniChanCookie` (failures answer `None`, like the JS
    /// fall-through).
    async fn session_cookie(&self, ctx: &ResolveCtx<'_>) -> Option<String> {
        {
            let cache = self
                .cookie
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cache.as_ref()
                && Instant::now() + COOKIE_REFRESH_EARLY < cached.expires_at
            {
                return Some(cached.value.clone());
            }
        }
        let url = Url::parse(&format!("{BASE_URL}/api/watch/session"))
            .unwrap_or_else(|e| panic!("the AniChan session URL must parse: {e}"));
        let request = FetchRequest::post(url, r#"{"token":""}"#.to_string())
            .with_header("User-Agent", UA)
            .with_header("Content-Type", "application/json")
            .with_timeout(API_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() {
            return None;
        }
        // The net layer joins repeated headers with ", " — matching any
        // one `set-cookie` line is enough.
        let joined = response.header("set-cookie").unwrap_or_default();
        let value = SESSION_COOKIE
            .captures(joined)
            .ok()
            .flatten()
            .and_then(|captures| captures.get(1))
            .map(|group| group.as_str().to_string())?;
        // The value's first dot-separated segment is its epoch expiry;
        // an unparseable value falls back to a 7000 s lease.
        let leading_epoch = value
            .split('.')
            .next()
            .unwrap_or_default()
            .parse::<f64>()
            .ok()
            .filter(|exp| exp.is_finite() && *exp > unix_now());
        let expires_at = match leading_epoch {
            Some(exp) => Instant::now() + Duration::from_secs_f64(exp - unix_now()),
            None => Instant::now() + COOKIE_FALLBACK_TTL,
        };
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CachedCookie {
            value: value.clone(),
            expires_at,
        });
        Some(value)
    }

    /// Drop the cached cookie after a session rejection.
    fn drop_cookie(&self) {
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// One watch-API GET with the session-cookie retry — the port of
    /// `apiGet`: `Accept: application/json`, the watch-page `Referer` +
    /// `X-Requested-With` markers when an `AniList` id is known, and the
    /// `anichan_ws` cookie on `/api/watch` paths. A `401 {"detail":
    /// "session"}` drops the cached cookie, mints a fresh one, and
    /// retries once; the answer is `None` on any other miss (the JS
    /// `null`).
    async fn api_get(
        &self,
        ctx: &ResolveCtx<'_>,
        path: &str,
        anilist_id: Option<u64>,
    ) -> Option<Value> {
        let url = Url::parse(&format!("{BASE_URL}{path}"))
            .unwrap_or_else(|e| panic!("the AniChan API URL must parse: {e}"));

        let fetch = |cookie: Option<String>| {
            let mut request = FetchRequest::get(url.clone())
                .with_header("User-Agent", UA)
                .with_header("Accept", "application/json")
                .with_timeout(API_TIMEOUT);
            if let Some(id) = anilist_id {
                request = request
                    .with_header("Referer", format!("{BASE_URL}/watch/{id}"))
                    .with_header("X-Requested-With", "XMLHttpRequest");
            }
            if path.starts_with("/api/watch")
                && let Some(cookie) = cookie
            {
                request = request.with_header("Cookie", format!("anichan_ws={cookie}"));
            }
            async move { ctx.fetcher.request(request).await }
        };

        let mut response = fetch(self.session_cookie(ctx).await).await.ok()?;
        if response.status == 401 && response.body.contains("\"session\"") {
            self.drop_cookie();
            response = fetch(self.session_cookie(ctx).await).await.ok()?;
        }
        if !response.is_success() {
            return None;
        }
        response.json::<Value>().ok()
    }

    /// Step 2 — the target episode number (1 for movies) and whether
    /// a dub exists; `None` when the episodes answer is unparseable
    /// (which zeroes the source, like upstream).
    async fn episode_facts(
        &self,
        ctx: &ResolveCtx<'_>,
        anilist_id: u64,
        media: &MediaRef,
    ) -> Option<(u32, bool)> {
        let episodes = self
            .api_get(
                ctx,
                &format!("/api/watch/episodes?anilistId={anilist_id}"),
                Some(anilist_id),
            )
            .await?;
        let episode_number = media
            .episode
            .filter(|_| media.season.is_some())
            .unwrap_or(1);
        let dub_available = episodes.get("dubAvailable") == Some(&Value::Bool(true));
        Some((episode_number, dub_available))
    }
}

#[async_trait]
impl Source for AniChan {
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

        // Step 1: the AniList id, with the two fallbacks in upstream
        // order (AniList GraphQL → AniChan's own search → Jikan).
        let candidates = resolve_candidates(ctx, &self.mappings, &name).await;
        if candidates.is_empty() {
            return Err(SourceError::NotFound);
        }

        // The wrapper's similarity-ranked pick over the candidate
        // pool (step 1's winner).
        let (best, best_score) = best_candidate(&candidates, &name);
        // Below 60 nothing ships; and without an AniList id the Jikan
        // fallback can never yield streams (upstream bail).
        let Some(anilist_id) = best
            .filter(|_| best_score >= 60.0)
            .and_then(|candidate| candidate.id)
        else {
            return Err(SourceError::NotFound);
        };

        // Step 2: episodes + dub availability.
        let Some((episode_number, dub_available)) =
            self.episode_facts(ctx, anilist_id, media).await
        else {
            return Err(SourceError::NotFound);
        };

        // Steps 3–4: sub (and dub when available) server sweeps, each
        // with the two bounded retries on an empty servers payload.
        let mut streams = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let types: &[&str] = if dub_available {
            &["sub", "dub"]
        } else {
            &["sub"]
        };
        for kind in types {
            let path = format!(
                "/api/watch/servers?anilistId={anilist_id}&episode={episode_number}&type={kind}"
            );
            let mut data = self.api_get(ctx, &path, Some(anilist_id)).await;
            if data.as_ref().is_none_or(empty_servers) {
                tokio::time::sleep(SERVERS_BACKOFF_1).await;
                data = self.api_get(ctx, &path, Some(anilist_id)).await;
            }
            if data.as_ref().is_none_or(empty_servers) {
                tokio::time::sleep(SERVERS_BACKOFF_2).await;
                data = self.api_get(ctx, &path, Some(anilist_id)).await;
            }
            let Some(servers) = data
                .as_ref()
                .and_then(|data| data.get("servers"))
                .and_then(Value::as_array)
                .cloned()
            else {
                continue;
            };
            for server in servers {
                let Some(stream_path) = server.get("stream").and_then(Value::as_str) else {
                    continue;
                };
                let stream_url = if stream_path.starts_with("http") {
                    stream_path.to_string()
                } else {
                    format!("{BASE_URL}{stream_path}")
                };
                if !seen.insert(stream_url.clone()) {
                    continue;
                }
                let Ok(url) = Url::parse(&stream_url) else {
                    continue;
                };

                let audio_label = if *kind == "dub" { "DUB" } else { "SUB" };
                let languages = if *kind == "dub" {
                    vec![CountryCode::Multi, CountryCode::En]
                } else {
                    vec![CountryCode::Multi, CountryCode::Ja]
                };
                streams.push(Stream {
                    url,
                    format: Format::Hls,
                    label: Some(format!("{title} (AniChan {audio_label})")),
                    meta: StreamMeta {
                        dubbed: Some(*kind == "dub"),
                        subbed: Some(*kind == "sub"),
                        languages,
                        resolution: Some(1080),
                        source_id: Some(ID.to_string()),
                        source_label: Some(LABEL.to_string()),
                        ..StreamMeta::default()
                    },
                    ttl: TTL,
                    is_external: false,
                    // The m3u8's RELATIVE variant URLs are ambiguous —
                    // upstream flagged the card for its proxy; keep the
                    // hint for clients (see the module docs).
                    behavior_hints: [("nuvioForceHls".to_string(), "1".to_string())]
                        .into_iter()
                        .collect(),
                });
            }
        }

        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

/// Step 1's candidate pool — the shared `AniList` client (same query,
/// cached and deduped across the anime providers), with the two
/// fallbacks in upstream order (`AniChan`'s own search, then Jikan's
/// MAL results).
async fn resolve_candidates(
    ctx: &ResolveCtx<'_>,
    mappings: &MappingService,
    name: &str,
) -> Vec<Candidate> {
    let mut candidates = match mappings.anilist_search(name).await {
        Ok(media) => media
            .into_iter()
            .map(|entry| Candidate {
                id: entry.id,
                english: entry.english,
                romaji: entry.romaji,
            })
            .collect::<Vec<_>>(),
        // Every resolver failure answers an empty list, like the JS
        // `catch { return null }` blocks.
        Err(_) => Vec::new(),
    };
    if candidates.is_empty() {
        candidates = anichan_search_candidates(ctx, name).await;
    }
    if candidates.is_empty() {
        candidates = jikan_candidates(ctx, name).await;
    }
    candidates
}

/// The best-scoring candidate for `name` with its score — ports the
/// wrapper's `similarity`-ranked pick over every candidate title.
fn best_candidate<'a>(candidates: &'a [Candidate], name: &str) -> (Option<&'a Candidate>, f64) {
    let name_norm = normalize(name);
    let first_word = name_norm.split(' ').next().unwrap_or_default();
    let mut best: Option<&'a Candidate> = None;
    let mut best_score = 0.0;
    for candidate in candidates {
        for title_text in [&candidate.english, &candidate.romaji]
            .into_iter()
            .flatten()
        {
            let title_norm = normalize(title_text);
            if title_norm.is_empty() {
                continue;
            }
            let score = title_score(&name_norm, first_word, &title_norm);
            if score > best_score {
                best_score = score;
                best = Some(candidate);
            }
        }
    }
    (best, best_score)
}

/// One candidate title's similarity score — the JS `similarity` ladder:
/// exact match 100, containment scaled to the shorter title, first-word
/// equality for the sequel/spelling variants containment misses, else 0.
#[allow(clippy::cast_precision_loss)] // char counts ride f64, like the JS ratios
fn title_score(name_norm: &str, first_word: &str, title_norm: &str) -> f64 {
    if title_norm == name_norm {
        100.0
    } else if title_norm.contains(name_norm) || name_norm.contains(title_norm) {
        // Containment — proportional to the shorter title.
        let short = title_norm.chars().count().min(name_norm.chars().count());
        let long = title_norm.chars().count().max(name_norm.chars().count());
        short as f64 / long.max(1) as f64 * 90.0
    } else if first_word.chars().count() >= 4 && title_norm.split(' ').next() == Some(first_word) {
        // First-word equality resolves the sequel/spelling variants
        // the containment rule misses (TMDB "Naruto Shippūden" vs
        // AniList "Naruto: Shippuuden").
        65.0 + first_word.chars().count().min(15) as f64
    } else {
        0.0
    }
}

/// Whether a servers payload has no `servers` array with entries —
/// upstream `!data?.servers?.length`.
fn empty_servers(data: &Value) -> bool {
    data.get("servers")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// The wall clock in unix seconds (the JS `Date.now() / 1000`).
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// `AniChan`'s own search index — the port of `resolveViaAniChanSearch`:
/// the `/search?q=` HTML's `/anime/{id}/{slug}` links become candidates
/// with the slug (hyphens → spaces) as both titles, exactly like
/// upstream.
async fn anichan_search_candidates(ctx: &ResolveCtx<'_>, name: &str) -> Vec<Candidate> {
    let url = Url::parse(&format!("{BASE_URL}/search?q={}", encode_component(name)))
        .unwrap_or_else(|e| panic!("the AniChan search URL must parse: {e}"));
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Accept", "text/html")
        .with_timeout(SEARCH_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if response.status != 200 {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for captures in ANIME_LINK.captures_iter(&response.body).flatten() {
        let Some(id) = captures
            .get(1)
            .and_then(|group| group.as_str().parse::<u64>().ok())
        else {
            continue;
        };
        let slug_title = captures
            .get(2)
            .map(|group| group.as_str().replace('-', " "))
            .unwrap_or_default();
        candidates.push(Candidate {
            id: Some(id),
            english: Some(slug_title.clone()),
            romaji: Some(slug_title),
        });
    }
    candidates
}

/// The Jikan (`MyAnimeList`) fallback — the port of `resolveViaJikan`:
/// MAL results converted to the `AniList` shape with no `AniList` id (the
/// MAL id would ride `idMal`), so they can match but never yield
/// streams.
async fn jikan_candidates(ctx: &ResolveCtx<'_>, name: &str) -> Vec<Candidate> {
    let url = Url::parse(&format!(
        "{JIKAN_API}/anime?q={}&limit=5&sfw=true",
        encode_component(name)
    ))
    .unwrap_or_else(|e| panic!("the Jikan search URL must parse: {e}"));
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Accept", "application/json")
        .with_timeout(JIKAN_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    if !response.is_success() {
        return Vec::new();
    }
    let Ok(data) = response.json::<Value>() else {
        return Vec::new();
    };
    let results = data.get("data").and_then(Value::as_array).cloned();
    let Some(results) = results.filter(|results| !results.is_empty()) else {
        return Vec::new();
    };
    results
        .into_iter()
        .map(|entry| Candidate {
            id: None,
            english: Some(
                entry
                    .get("title_english")
                    .or_else(|| entry.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            romaji: Some(
                entry
                    .get("title_japanese")
                    .or_else(|| entry.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        })
        .collect()
}

/// The shared normalize shape: lowercase, strip combining marks, drop
/// non `[a-z0-9\s]`, collapse whitespace — ports `normalize`.
fn normalize(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};

    use super::*;

    /// A fetcher serving canned `(status, body, headers)` keyed by URL
    /// path (or `path?query`) — later registrations REPLACE earlier
    /// ones, `page_sequence` serves entries in order — recording every
    /// request. Query-bearing lookups fall back to the bare path, so
    /// TMDB requests (`?api_key=…`) are scripted by path alone.
    /// One scripted page: status, body, headers.
    type Page = (u16, String, BTreeMap<String, String>);

    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<Page>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `status`/`body` (replacing any earlier
        /// registration).
        fn page(self, key: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.into(), vec![(status, body.into(), BTreeMap::new())]);
            self
        }

        /// Serve `key` with a `set-cookie` header.
        fn page_with_cookie(
            self,
            key: impl Into<String>,
            status: u16,
            body: impl Into<String>,
            set_cookie: &str,
        ) -> Self {
            let mut headers = BTreeMap::new();
            headers.insert("set-cookie".to_string(), set_cookie.to_string());
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.into(), vec![(status, body.into(), headers)]);
            self
        }

        /// Serve the same key a sequence of entries, popping in order.
        fn page_sequence(
            self,
            key: impl Into<String>,
            entries: Vec<(u16, String, BTreeMap<String, String>)>,
        ) -> Self {
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
                pages.get_mut(&key).map(|entries| {
                    if entries.len() > 1 {
                        entries.remove(0)
                    } else {
                        entries[0].clone()
                    }
                })
            };
            let entry = if let Some(entry) = entry {
                Some(entry)
            } else {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(request.url.path()).map(|entries| {
                    if entries.len() > 1 {
                        entries.remove(0)
                    } else {
                        entries[0].clone()
                    }
                })
            };
            let Some((status, body, headers)) = entry else {
                return Err(FetchError::NotFound { url: request.url });
            };
            Ok(FetchResponse {
                url: request.url,
                status,
                headers,
                body,
            })
        }
    }

    /// The fixture media: Frieren S01E02 (`AniList` 154587).
    const TMDB_ID: u64 = 209_867;
    /// A cookie whose leading epoch is far in the future.
    const FAR_FUTURE_COOKIE: &str = "anichan_ws=9999999999.abcdef; Path=/api/watch; HttpOnly";
    /// The `AniList` GraphQL response (the POST lands on path `/`).
    const ANILIST_BODY: &str = r#"{"data":{"Page":{"media":[{"id":154587,"idMal":52991,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End"},"format":"TV"}]}}}"#;
    /// The sub servers page (query-keyed).
    const SUB_SERVERS: &str = "/api/watch/servers?anilistId=154587&episode=2&type=sub";
    /// The dub servers page (query-keyed).
    const DUB_SERVERS: &str = "/api/watch/servers?anilistId=154587&episode=2&type=dub";
    /// The episodes page (query-keyed).
    const EPISODES: &str = "/api/watch/episodes?anilistId=154587";

    /// The provider over a TMDB client and mapping service sharing the
    /// scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> AniChan {
        AniChan::new(
            Arc::new(TmdbClient::new("test-key", mock.clone())),
            MappingService::new(Arc::clone(mock) as Arc<dyn Fetcher>),
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

    /// The `AniList` + TMDB + watch pages the happy-path fixtures share.
    fn base_pages(mock: ScriptedFetcher) -> ScriptedFetcher {
        mock.page(
            format!("/3/tv/{TMDB_ID}"),
            200,
            r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
        )
        .page("/", 200, ANILIST_BODY)
        .page_with_cookie("/api/watch/session", 200, r#"{"ok":true}"#, FAR_FUTURE_COOKIE)
        .page(EPISODES, 200, r#"{"episodes":28,"dubAvailable":true}"#)
        .page(
            SUB_SERVERS,
            200,
            r#"{"servers":[{"name":"Vidstream","type":"hls","stream":"/api/watch/m3u8?sh=/hls/frieren/1/2/master.m3u8&sig=abc&exp=999"}]}"#,
        )
        .page(
            DUB_SERVERS,
            200,
            r#"{"servers":[{"name":"Vidstream","type":"hls","stream":"/api/watch/m3u8?sh=/hls/frieren/1/2/dub.m3u8&sig=def&exp=777"}]}"#,
        )
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "anichan");
        assert_eq!(info.label, "AniChan");
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
            Some("https://anichan.to/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_cards_with_the_session_cookie() -> Result<(), SourceError> {
        let mock = Arc::new(base_pages(ScriptedFetcher::default()));
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        // One sub + one dub card (dubAvailable), each the original
        // (relative-joined) stream URL.
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].url.as_str(),
            "https://anichan.to/api/watch/m3u8?sh=/hls/frieren/1/2/master.m3u8&sig=abc&exp=999"
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://anichan.to/api/watch/m3u8?sh=/hls/frieren/1/2/dub.m3u8&sig=def&exp=777"
        );
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(streams[0].ttl, TTL);
        assert_eq!(
            streams[0]
                .behavior_hints
                .get("nuvioForceHls")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (AniChan SUB)")
        );
        assert_eq!(
            streams[0].meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            streams[1].meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        // The watch-API requests carried the cookie, the watch-page
        // referer, and the XHR marker.
        assert_eq!(
            mock.sent_header("anilistId=154587", "Cookie").as_deref(),
            Some("anichan_ws=9999999999.abcdef")
        );
        assert_eq!(
            mock.sent_header("anilistId=154587", "Referer").as_deref(),
            Some("https://anichan.to/watch/154587")
        );
        assert_eq!(
            mock.sent_header("anilistId=154587", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        // The session is minted once and reused.
        assert_eq!(mock.hits("/api/watch/session"), 1);
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("anichan"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_stale_cookie_re_mints_and_retries_once() {
        // The first episodes answer 401 {"detail":"session"}; a fresh
        // cookie (the same scripted value) unblocks it, dubAvailable
        // false leaves the sub sweep, and the empty servers sweep
        // exhausts its retries.
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default())
                .page_sequence(
                    EPISODES,
                    vec![
                        (401, r#"{"detail":"session"}"#.to_string(), BTreeMap::new()),
                        (
                            200,
                            r#"{"dubAvailable":false}"#.to_string(),
                            BTreeMap::new(),
                        ),
                    ],
                )
                .page(SUB_SERVERS, 200, r#"{"servers":[]}"#),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        // The session was re-minted after the rejection.
        assert_eq!(mock.hits("/api/watch/session"), 2);
    }

    #[tokio::test]
    async fn an_empty_servers_payload_retries_with_backoff() -> Result<(), SourceError> {
        // Two empty answers then the real one (the 800 ms/1600 ms
        // backoffs are real sleeps).
        let mock = Arc::new(
            base_pages(ScriptedFetcher::default())
                .page(EPISODES, 200, r#"{"dubAvailable":false}"#)
                .page_sequence(
                    SUB_SERVERS,
                    vec![
                        (200, r#"{"servers":[]}"#.to_string(), BTreeMap::new()),
                        (200, r#"{"servers":[]}"#.to_string(), BTreeMap::new()),
                        (
                            200,
                            r#"{"servers":[{"stream":"/api/watch/m3u8?sh=/hls/x.m3u8&sig=a&exp=9"}]}"#
                                .to_string(),
                            BTreeMap::new(),
                        ),
                    ],
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(mock.hits("/api/watch/servers"), 3);
        Ok(())
    }

    #[tokio::test]
    async fn falls_back_to_the_anichan_search_index() -> Result<(), SourceError> {
        // The AniList GraphQL POST is unserved (404) → the search
        // index answers a slug that normalizes to the TMDB name.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page_with_cookie("/api/watch/session", 200, "{}", FAR_FUTURE_COOKIE)
                .page(EPISODES, 200, r#"{"dubAvailable":false}"#)
                .page(
                    SUB_SERVERS,
                    200,
                    r#"{"servers":[{"stream":"/api/watch/m3u8?sh=/hls/x.m3u8&sig=a&exp=9"}]}"#,
                )
                .page(
                    "/search",
                    200,
                    r#"<html><a href="https://anichan.to/anime/154587/frieren-beyond-journeys-end">x</a></html>"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(mock.hits("/search"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn the_jikan_fallback_matches_but_yields_no_streams() {
        // AniList is unserved (404) and the AniChan search answers an
        // empty page; Jikan finds the anime but its MAL-shaped entries
        // carry no AniList id → the upstream zero.
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/tv/{TMDB_ID}"),
                    200,
                    r#"{"name":"Frieren: Beyond Journey's End","first_air_date":"2023-09-29"}"#,
                )
                .page("/search", 200, "<html></html>")
                .page(
                    "/anime",
                    200,
                    r#"{"data":[{"mal_id":52991,"title":"Frieren: Beyond Journey's End","title_english":"Frieren: Beyond Journey's End","title_japanese":"葬送のフリーレン","type":"TV"}]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(TMDB_ID), 1, 2))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        // The AniList-id bail fired before any watch-API call.
        assert_eq!(mock.hits("/api/watch/session"), 0);
        assert_eq!(mock.hits("/api/watch/episodes"), 0);
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
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/", 200, ANILIST_BODY)
                .page_with_cookie("/api/watch/session", 200, "{}", FAR_FUTURE_COOKIE)
                .page(EPISODES, 200, r#"{"dubAvailable":false}"#)
                .page(
                    SUB_SERVERS,
                    200,
                    r#"{"servers":[{"stream":"https://cdn.anichan.to/hls/master.m3u8?sig=1&exp=2"}]}"#,
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
        // An absolute stream URL passes through untouched.
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.anichan.to/hls/master.m3u8?sig=1&exp=2"
        );
        // No TMDB detail fetch.
        assert_eq!(mock.hits_starting_with("/3/tv"), 0);
        Ok(())
    }
}
