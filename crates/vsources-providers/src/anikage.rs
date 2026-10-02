//! `AniKage`: direct anime HLS/MP4 from the `anikage.cc` JSON API.
//!
//! Ports `src/source/AniKage.js` — a clean JSON API (no scraping) with
//! direct streams behind a token-gated CDN proxy:
//!
//! 1. search `GET /api/media/anime/browse?q={title}` →
//!    `{data: [{slug, title: {english, romaji, …}}]}`;
//! 2. episodes `GET /api/media/anime/{slug}/episodes` → an array, or an
//!    object with an `episodes` field;
//! 3. servers `GET /api/media/anime/{slug}/episodes/{N}/servers` →
//!    `{servers: [{id, subTypes: ["sub"|"dub"]}]}`;
//! 4. sources `GET /api/media/anime/{slug}/episodes/{N}/sources?provider={id}&lang={sub|dub}`
//!    → `{sources: [{url, quality, isM3U8}]}` — `url` is a token, the
//!    relay is read from the site's `PUBLIC_PROXY_URL` runtime setting;
//!    tokens become `/m3u8/{token}` (HLS) or `/stream/{token}` (MP4).
//!
//! Providers: `neko` (default, sub+dub), `koto` (sub+dub, often 1080p),
//! `dib` (BD, sub only), `wave` (Vidplay, sub+dub). `megg` was removed
//! upstream — its `/stream/` MP4 tokens expire too quickly and cause
//! 502s; the other providers' HLS is reliable.
//!
//! The API endpoints sit behind Cloudflare: the requests carry the
//! `Origin`/`Sec-Fetch-*` header set the upstream
//! `headerGeneratorOptions` produced (a full Chrome header set comes
//! from the fetcher layer here).
//!
//! Mappings and cuts (vs. upstream):
//! - Relay configuration is cached for five minutes. The engine validates
//!   media with bounded probes; this provider does not fetch media bodies.
//! - The stream is direct — no extractor hop, no `/proxy` route; the
//!   hotlink `Referer: https://anikage.cc/` travels on
//!   `meta.request_headers` exactly where upstream set it.
//! - `meta.title` (`{title} (Sub · {provider} · {quality})`) →
//!   [`Stream::label`]; `meta.countryCodes` → `meta.languages`,
//!   `meta.height` → `meta.resolution`.
//! - Stream tokens expire within minutes: the upstream 3min `this.ttl`
//!   override is the stream ttl here (the parent's `CachedSource`
//!   honors the shorter of the two).
//! - `apiGet` swallows *every* failure upstream (try/catch → null), so
//!   all fetch errors map to misses → [`SourceError::NotFound`]; only a
//!   structurally broken URL is a scrape error.
//! - `getTmdbId`/`getTmdbNameAndYear` → `ctx.media`; missing media (no
//!   title to search) answers [`SourceError::NotFound`].
//! - The explicit Chrome `User-Agent`/`Accept-Language` are dropped —
//!   the fetcher layer already sends browser-like headers and solves
//!   Cloudflare challenges.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; `normalize` still strips combining marks (U+0300–036F),
//!   and precomposed accents drop out.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::mappings::MappingService;
use vsources_core::traits::{FetchRequest, ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, Format, MediaRef, MediaType, SourceInfo, Stream};

/// The site root, upstream `BASE_URL`.
const BASE_URL: &str = "https://anikage.cc";
/// The stream CDN proxy, upstream `PROXY_BASE`.
const PROXY_BASE: &str = "https://prox.anikage.cc";
static PUBLIC_PROXY: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#""PUBLIC_PROXY_URL"\s*:\s*"([^"\r\n]+)""#)
        .unwrap_or_else(|e| panic!("proxy config pattern: {e}"))
});
/// This provider's id, for scrape diagnostics.
const PROVIDER_ID: &str = "anikage";
/// Provider priority — neko (default) first, then koto (often 1080p),
/// dib (BD), wave (Vidplay).
const PROVIDER_PRIORITY: [&str; 4] = ["neko", "koto", "dib", "wave"];
/// Upstream `timeout: { request: 25000 }` (Cloudflare slows the API).
const TIMEOUT: Duration = Duration::from_secs(25);
/// Upstream `this.ttl` override: stream tokens expire within minutes.
const TTL: Duration = Duration::from_mins(3);

/// The `anikage.cc` provider: direct HLS/MP4 through its configured relay.
pub struct AniKage {
    /// Static descriptor.
    info: SourceInfo,
    /// The deployed relay changes independently of the site hostname.
    proxy: moka::future::Cache<(), Url>,
    /// The shared id-mapping service (arm/anilist); `None` keeps the
    /// name-scoring path only.
    mappings: Option<MappingService>,
}

impl AniKage {
    /// A new provider with a short-lived cache for the deployed relay.
    #[must_use]
    pub fn new() -> Self {
        Self::build(None)
    }

    /// A provider over the shared id-mapping service — the id-first
    /// fast path (arm → anilist id equality on browse candidates).
    #[must_use]
    pub fn with_mappings(mappings: MappingService) -> Self {
        Self::build(Some(mappings))
    }

    /// The shared constructor.
    fn build(mappings: Option<MappingService>) -> Self {
        Self {
            mappings,
            proxy: moka::future::Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_mins(5))
                .build(),
            info: SourceInfo {
                id: PROVIDER_ID.to_string(),
                label: "AniKage".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(
                    Url::parse(BASE_URL)
                        .unwrap_or_else(|e| panic!("the AniKage base URL must parse: {e}")),
                ),
                priority: 0,
                // Upstream leaves `this.domainKey` unset.
                domain_key: None,
            },
        }
    }
}

impl Default for AniKage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Source for AniKage {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // Upstream resolves TMDB (getTmdbId + getTmdbNameAndYear) and
        // searches by name; without pre-resolved media there is no title.
        let Some(resolved) = ctx.media.as_ref() else {
            return Err(SourceError::NotFound);
        };
        let title = display_title(&resolved.name, resolved.year, media);
        let target = target_episode(media);

        // Step 1: search for the anime — id-first when the shared mapping
        // service can resolve the season's AniList id, name-score
        // otherwise. AniKage catalogs each season as a separate entry
        // ("… Season 2"), so the name search for a later season can land
        // on the wrong (S1) entry; the id path picks the exact one.
        // No id match (or the mapping services failed): the title search
        // still works for S1 and movies.
        let slug = match find_slug_verified(self, ctx, &resolved.name, resolved).await? {
            Some(slug) => slug,
            None => match find_slug(ctx, &resolved.name).await? {
                Some(slug) => slug,
                None => return Err(SourceError::NotFound),
            },
        };

        // Step 2: the episodes list (an array or an `episodes` field).
        let episodes_url = format!("{BASE_URL}/api/media/anime/{slug}/episodes");
        let Some(payload) = api_get(ctx, &site_url(&episodes_url)?).await? else {
            return Err(SourceError::NotFound);
        };
        let episodes = match payload {
            Value::Array(list) => list,
            other => other
                .get("episodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        };
        if episodes.is_empty() {
            return Err(SourceError::NotFound);
        }

        // Step 3: the requested episode (movies fall back to the first).
        let Some(episode) = episodes
            .iter()
            .find(|ep| {
                ep.get("number")
                    .and_then(Value::as_u64)
                    .is_some_and(|number| number == u64::from(target))
            })
            .or_else(|| episodes.first())
        else {
            return Err(SourceError::NotFound);
        };
        let Some(episode_number) = episode.get("number").and_then(Value::as_u64) else {
            return Err(SourceError::NotFound);
        };

        // Step 4: the available servers for this episode.
        let servers_url =
            format!("{BASE_URL}/api/media/anime/{slug}/episodes/{episode_number}/servers");
        let Some(payload) = api_get(ctx, &site_url(&servers_url)?).await? else {
            return Err(SourceError::NotFound);
        };
        let Some(servers) = payload.get("servers").and_then(Value::as_array) else {
            return Err(SourceError::NotFound);
        };
        if servers.is_empty() {
            return Err(SourceError::NotFound);
        }

        // Step 5: for each provider + sub/dub, fetch the sources.
        self.streams_for_servers(ctx, &slug, episode_number, servers, &title)
            .await
    }
}

impl AniKage {
    /// Resolve the same public runtime setting that the site's player uses.
    async fn proxy_base(&self, ctx: &ResolveCtx<'_>) -> Url {
        self.proxy
            .get_with((), async {
                let fallback =
                    site_url(PROXY_BASE).unwrap_or_else(|e| panic!("constant proxy URL: {e}"));
                let Ok(url) = site_url(BASE_URL) else {
                    return fallback;
                };
                let Ok(response) = ctx
                    .fetcher
                    .request(FetchRequest::get(url).with_timeout(TIMEOUT))
                    .await
                else {
                    return fallback;
                };
                PUBLIC_PROXY
                    .captures(&response.body)
                    .ok()
                    .flatten()
                    .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
                    .and_then(|value| Url::parse(&value).ok())
                    .filter(|url| matches!(url.scheme(), "https" | "http"))
                    .unwrap_or(fallback)
            })
            .await
    }

    /// Step 5: for each priority provider and sub/dub language, fetch
    /// the sources and build one direct stream per token.
    async fn streams_for_servers(
        &self,
        ctx: &ResolveCtx<'_>,
        slug: &str,
        episode_number: u64,
        servers: &[Value],
        title: &str,
    ) -> Result<Vec<Stream>, SourceError> {
        let proxy = self.proxy_base(ctx).await;
        let mut streams = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut seen_labels: HashSet<String> = HashSet::new();
        for provider in PROVIDER_PRIORITY {
            let Some(server) = servers
                .iter()
                .find(|server| server.get("id").and_then(Value::as_str) == Some(provider))
            else {
                continue;
            };
            let sub_types = server
                .get("subTypes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for lang in sub_types {
                let Some(lang) = lang.as_str() else {
                    continue;
                };
                if lang != "sub" && lang != "dub" {
                    continue;
                }

                let sources_url = format!(
                    "{BASE_URL}/api/media/anime/{slug}/episodes/{episode_number}/sources?provider={provider}&lang={lang}"
                );
                let Some(payload) = api_get(ctx, &site_url(&sources_url)?).await? else {
                    continue;
                };
                let Some(sources) = payload.get("sources").and_then(Value::as_array) else {
                    continue;
                };

                for source in sources {
                    let Some(stream) = self.stream_for_source(
                        source,
                        &proxy,
                        provider,
                        lang,
                        title,
                        &mut seen_urls,
                        &mut seen_labels,
                    ) else {
                        continue;
                    };
                    streams.push(stream);
                }
            }
        }
        // The engine validates each card with bounded probes; a failed first
        // URL must not suppress every other server or download an entire MP4.
        Ok(streams)
    }

    /// One direct stream from a source token, or `None` when it dedupes
    /// against an earlier one.
    #[allow(clippy::too_many_arguments)]
    fn stream_for_source(
        &self,
        source: &Value,
        proxy: &Url,
        provider: &str,
        lang: &str,
        title: &str,
        seen_urls: &mut HashSet<String>,
        seen_labels: &mut HashSet<String>,
    ) -> Option<Stream> {
        let token = source.get("url").and_then(Value::as_str)?;
        // Build the direct stream URL from the token.
        let is_m3u8 = source
            .get("isM3U8")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        let stream_path = if is_m3u8 {
            format!("/m3u8/{token}")
        } else {
            format!("/stream/{token}")
        };
        let stream_url = proxy.join(&stream_path).ok()?;
        if !seen_urls.insert(stream_url.as_str().to_string()) {
            return None;
        }

        let quality = source
            .get("quality")
            .and_then(Value::as_str)
            .filter(|quality| !quality.is_empty())
            .unwrap_or("HD");
        let label_key = format!("{provider}_{lang}_{quality}");
        if !seen_labels.insert(label_key) {
            return None;
        }

        let audio = if lang == "dub" { "Dub" } else { "Sub" };
        let languages = if lang == "dub" {
            vec![CountryCode::Multi, CountryCode::En]
        } else {
            vec![CountryCode::Multi, CountryCode::Ja]
        };
        let mut stream = Stream::new(stream_url, if is_m3u8 { Format::Hls } else { Format::Mp4 })
            .with_ttl(TTL)
            .with_referer(format!("{BASE_URL}/"))
            .with_label(format!("{title} ({audio} · {provider} · {quality})"));
        // The relay Origin-gates requests (upstream routes playback
        // through its own /proxy for exactly this); the Referer alone
        // is not enough for a client-side player.
        stream
            .meta
            .request_headers
            .insert("Origin".to_string(), BASE_URL.to_string());
        // Parse the height from the quality string
        // ("hardsub HD-1" → 720, "1080p" → 1080).
        if let Some(height) = height_from_quality(quality) {
            stream.meta.resolution = Some(height);
        }
        stream.meta.languages = languages;
        stream.meta.dubbed = Some(lang == "dub");
        stream.meta.subbed = Some(lang == "sub");
        stream.meta.source_id = Some(self.info.id.clone());
        stream.meta.source_label = Some(self.info.label.clone());
        Some(stream)
    }
}

/// Search and return the slug whose ids match the media — `Some(slug)`
/// on a verified match, `None` when nothing lines up.
///
/// The id-first ladder:
///
/// 1. `arm` (through the shared [`MappingService`]) resolves the
///    (show, season) to its `AniList` id — one deduped request shared by
///    every anime provider resolving the same show;
/// 2. the browse candidates carry their own `anilistId`, so exact id
///    equality picks the entry — deterministic even when both seasons
///    share one `IMDb` id and score identically on title;
/// 3. the verification tier (used when step 2 is ambiguous or absent):
///    the detail endpoint's `trackers` block pins the show
///    (`imdbId`/`tmdbId`), and the episodes payload's `seasonNumber`
///    pins the season — the discriminator the trackers alone cannot
///    provide. The TUI retains episode titles/air dates separately;
///    this adapter currently verifies database ids and season numbers;
/// 4. anything the id sources do not know falls back to name scoring
///    (the caller's `find_slug`) — today's behavior, preserved.
///
/// A missing [`MappingService`] or an arm/anilist outage answers `None`
/// (transient failures are never cached as misses) so the fallback tier
/// keeps working.
async fn find_slug_verified(
    provider: &AniKage,
    ctx: &ResolveCtx<'_>,
    name: &str,
    resolved: &ResolvedMedia,
) -> Result<Option<String>, SourceError> {
    let Some(mappings) = provider.mappings.as_ref() else {
        return Ok(None);
    };
    // The target AniList id for this (show, season) — arm first, the
    // AniList title search when arm has no entry.
    let season = resolved.season.unwrap_or(1);
    let target = match resolved.imdb_id.as_deref() {
        Some(imdb) => mappings.season_ids_by_imdb(imdb, season, Some(name)).await,
        None => match resolved.tmdb_id {
            Some(tmdb) => mappings.season_ids_by_tmdb(tmdb, season, Some(name)).await,
            None => return Ok(None),
        },
    }
    .ok()
    .flatten()
    .map(|ids| ids.anilist_id);

    let candidates = browse_candidates(ctx, name).await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    // The fast path: exact anilistId equality on the browse candidates —
    // no detail request needed in the common case.
    if let Some(target) = target
        && let Some(slug) = candidates
            .iter()
            .find(|candidate| candidate_anilist_id(candidate) == Some(target))
            .and_then(|candidate| candidate.slug.clone())
    {
        return Ok(Some(slug));
    }

    // The verification tier: trackers pin the show, and the episodes
    // payload pins the season (its `seasonNumber` field). Both seasons
    // of a split share one imdbId/tmdbId, so the season check is the
    // discriminator — and the episodes fetch is already on the resolve
    // path, making this tier free when the fast path missed.
    for candidate in candidates.iter().take(CANDIDATE_LIMIT) {
        let Some(slug) = candidate.slug.as_deref() else {
            continue;
        };
        let detail_url = format!("{BASE_URL}/api/media/anime/{slug}");
        let Ok(url) = site_url(&detail_url) else {
            continue;
        };
        let Some(payload) = api_get(ctx, &url).await? else {
            continue;
        };
        let Some(anime) = payload.get("anime") else {
            continue;
        };

        // When the target id is known, the detail's own anilist id
        // (trackers.malId is the MAL one; anime.anilistId is the
        // AniList one) must agree — the strongest single check.
        if let Some(target) = target
            && candidate_anilist_id(candidate) != Some(target)
            && anime.get("anilistId").and_then(Value::as_u64) != Some(target)
        {
            continue;
        }

        let imdb_ok = resolved
            .imdb_id
            .as_deref()
            .is_none_or(|imdb| trackers_imdb(anime).is_some_and(|t| t == imdb));
        let tmdb_ok = resolved
            .tmdb_id
            .is_none_or(|tmdb| trackers_tmdb(anime).is_some_and(|t| t == tmdb));
        // A tracker must match *something* to count as verified — ids
        // absent on both sides is a name-only tie, not a confirmation.
        let any_id = resolved.imdb_id.is_some() || resolved.tmdb_id.is_some();
        if !any_id || !imdb_ok || !tmdb_ok {
            continue;
        }
        // The season discriminator: the episodes payload's
        // `seasonNumber` (fetched free below on the resolve path).
        if let Some(season) = resolved.season
            && let Ok(episodes_url) =
                site_url(&format!("{BASE_URL}/api/media/anime/{slug}/episodes"))
            && let Some(payload) = api_get(ctx, &episodes_url).await?
            && let Some(season_number) = episode_season_number(&payload)
            && season_number != season
        {
            continue;
        }
        return Ok(candidate.slug.clone());
    }
    Ok(None)
}

/// One browse result, kept as the raw JSON so detail-specific fields
/// (title, slug) survive without a full struct.
struct BrowseCandidate {
    /// The catalog slug.
    slug: Option<String>,
    /// The raw result object.
    raw: Value,
}

/// The `anilistId` field of a browse candidate (the detail payload
/// carries the same field at its `anime` root).
fn candidate_anilist_id(candidate: &BrowseCandidate) -> Option<u64> {
    candidate.raw.get("anilistId").and_then(Value::as_u64)
}

/// The episodes payload's `seasonNumber`, from either shape (an array of
/// episode objects or an `{episodes: [...]}` object).
fn episode_season_number(payload: &Value) -> Option<u32> {
    let episodes = match payload {
        Value::Array(list) => list.first(),
        other => other
            .get("episodes")
            .and_then(Value::as_array)
            .and_then(|list| list.first()),
    }?;
    let number = episodes.get("seasonNumber").and_then(Value::as_u64)?;
    u32::try_from(number).ok()
}

/// The `trackers.imdbId` field of a detail payload.
fn trackers_imdb(anime: &Value) -> Option<&str> {
    anime
        .pointer("/trackers/imdbId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// The `trackers.tmdbId` field of a detail payload.
fn trackers_tmdb(anime: &Value) -> Option<u64> {
    anime
        .pointer("/trackers/tmdbId")
        .and_then(Value::as_u64)
        .or_else(|| {
            anime
                .pointer("/trackers/tmdbId")
                .and_then(Value::as_i64)
                .and_then(|v| u64::try_from(v).ok())
        })
}

/// How many browse candidates get the (network-costly) detail fetch.
const CANDIDATE_LIMIT: usize = 4;

/// All browse results for a query (best-first is not assumed).
async fn browse_candidates(
    ctx: &ResolveCtx<'_>,
    name: &str,
) -> Result<Vec<BrowseCandidate>, SourceError> {
    let mut out = Vec::new();
    for query in query_variants(name) {
        let browse = format!(
            "{BASE_URL}/api/media/anime/browse?q={}",
            encode_component(&query)
        );
        let Some(payload) = api_get(ctx, &site_url(&browse)?).await? else {
            continue;
        };
        let Some(results) = payload.get("data").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            out.push(BrowseCandidate {
                slug: result
                    .get("slug")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                raw: result.clone(),
            });
        }
        if !out.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// Search by name and return the best-matching slug (score ≥ 60) — the
/// browse API returns titles in english/romaji/native/userPreferred.
async fn find_slug(ctx: &ResolveCtx<'_>, name: &str) -> Result<Option<String>, SourceError> {
    let name_norm = normalize(name);
    for query in query_variants(name) {
        let browse = format!(
            "{BASE_URL}/api/media/anime/browse?q={}",
            encode_component(&query)
        );
        let Some(payload) = api_get(ctx, &site_url(&browse)?).await? else {
            continue;
        };
        let Some(results) = payload.get("data").and_then(Value::as_array) else {
            continue;
        };

        let mut best: Option<String> = None;
        let mut best_score = 0.0_f64;
        for result in results {
            for field in ["english", "romaji", "native", "userPreferred"] {
                let Some(candidate) = result
                    .pointer(&format!("/title/{field}"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let candidate_norm = normalize(candidate);
                if candidate_norm.is_empty() {
                    continue;
                }
                let score = if candidate_norm == name_norm {
                    100.0
                } else if candidate_norm.contains(&name_norm) || name_norm.contains(&candidate_norm)
                {
                    inclusion_score(&candidate_norm, &name_norm)
                } else {
                    0.0
                };
                if score > best_score {
                    best_score = score;
                    best = result
                        .get("slug")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
        }
        if best.is_some() && best_score >= 60.0 {
            return Ok(best);
        }
    }
    Ok(None)
}

/// `apiGet`: JSON from the anikage API. Every failure is a miss — the
/// upstream wraps the whole call in try/catch and returns `null` for
/// non-200 answers, malformed JSON, and transport errors alike.
///
/// The header set matches the Cloudflare-bypassing request upstream
/// sends (`Origin` + `Sec-Fetch-*`; the Chrome `User-Agent` and
/// `Accept-Language` come from the fetcher layer).
async fn api_get(ctx: &ResolveCtx<'_>, url: &Url) -> Result<Option<Value>, SourceError> {
    let request = FetchRequest::get(url.clone())
        .with_timeout(TIMEOUT)
        .with_header("Accept", "application/json, text/plain, */*")
        .with_header("Referer", format!("{BASE_URL}/"))
        .with_header("Origin", BASE_URL)
        .with_header("Sec-Fetch-Dest", "empty")
        .with_header("Sec-Fetch-Mode", "cors")
        .with_header("Sec-Fetch-Site", "same-origin");
    match ctx.fetcher.request(request).await {
        Ok(response) if response.status == 200 => Ok(response.json::<Value>().ok()),
        Ok(_) | Err(_) => Ok(None),
    }
}

/// Parse a height from a quality label — ports
/// `qualityStr.match(/(\d{3,4})p?/)` (first run of 3–4 digits, capped
/// at 4 like the regex), with the `HD` → 720 fallback.
fn height_from_quality(quality: &str) -> Option<u16> {
    let run = quality
        .split(|c: char| !c.is_ascii_digit())
        .find(|run| run.len() >= 3);
    match run {
        // JS spreads `...(height && {height})` — a zero height drops.
        Some(run) => run[..run.len().min(4)]
            .parse::<u16>()
            .ok()
            .filter(|height| *height > 0),
        // The HD fallback only runs when no digit run matched.
        None => quality.contains("HD").then_some(720),
    }
}

/// The display title — `name S01E02` for episodes, `name (year)` for
/// movies (ports `getTmdbNameAndYear` + `formatSeasonAndEpisode`).
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{name} {}", media.format_season_and_episode())
    } else {
        format!(
            "{name} ({})",
            year.map(|y| y.to_string()).unwrap_or_default()
        )
    }
}

/// The episode number to look for — the reference's episode for series,
/// 1 for movies (`tmdbId.season ? (tmdbId.episode || 1) : 1`).
fn target_episode(media: &MediaRef) -> u32 {
    media.season.map_or(1, |_| media.episode.unwrap_or(1))
}

/// Normalize for fuzzy title matching — lowercase, diacritics stripped,
/// non-alphanumerics dropped, whitespace collapsed.
fn normalize(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Search query variants, deduped in order — the name as-is, the
/// diacritics-stripped name, and the punctuation-spaced name.
fn query_variants(name: &str) -> Vec<String> {
    let stripped: String = name
        .chars()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .collect();
    let spaced: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.is_ascii_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let spaced = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut variants: Vec<String> = Vec::new();
    for query in [name.to_string(), stripped, spaced] {
        if !query.is_empty() && !variants.contains(&query) {
            variants.push(query);
        }
    }
    variants
}

/// One-sided containment score: 90·(min/max) of the normalized lengths.
fn inclusion_score(a: &str, b: &str) -> f64 {
    let (a_len, b_len) = (len_f64(a), len_f64(b));
    a_len.min(b_len) / a_len.max(b_len) * 90.0
}

/// A char count as an exact `f64` (clamped at `u32::MAX`; normalized
/// titles are far below), avoiding a lossy `usize` cast.
fn len_f64(s: &str) -> f64 {
    f64::from(u32::try_from(s.chars().count()).unwrap_or(u32::MAX))
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

/// Parse a runtime-built URL — a structural surprise, not a miss.
fn site_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw).map_err(|error| {
        SourceError::scrape(PROVIDER_ID, format!("unparsable URL {raw:?}: {error}"))
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::sync::Arc;

    use crate::testing::{ScriptedFetcher, key_of};
    use vsources_core::traits::{Fetcher, ResolvedMedia};
    use vsources_core::types::MediaId;

    /// The searched title.
    const NAME: &str = "Frieren: Beyond Journey's End";

    /// The page key for a URL string, canonicalized exactly like the
    /// provider's wire requests (the `url` crate percent-encodes some
    /// query characters, e.g. `'`).
    fn url_key(raw: &str) -> String {
        let url = Url::parse(raw).unwrap_or_else(|e| panic!("valid URL {raw:?}: {e}"));
        key_of(&url)
    }

    /// Resolved TMDB metadata for the searched title.
    fn resolved_media(season: Option<u32>, episode: Option<u32>) -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(154_587),
            imdb_id: None,
            name: NAME.to_string(),
            year: Some(2023),
            season,
            episode,
        }
    }

    /// The media reference matching [`resolved_media`].
    fn media_ref(season: Option<u32>, episode: Option<u32>) -> MediaRef {
        MediaRef {
            id: MediaId::Tmdb(154_587),
            kind: MediaType::Series,
            season,
            episode,
        }
    }

    /// A resolve context over the scripted fetcher.
    fn ctx_for(fetcher: &ScriptedFetcher, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// The browse fixture: the first entry matches exactly.
    const BROWSE_PAGE: &str = r#"{"data":[
        {"slug":"frieren-beyond-journeys-end","title":{"english":"Frieren: Beyond Journey's End","romaji":"Sousou no Frieren","native":"葬送のフリーレン","userPreferred":"Sousou no Frieren"}},
        {"slug":"frieren-spinoff","title":{"english":"Frieren Spinoff","romaji":"Spinoff","native":"","userPreferred":"Spinoff"}}
    ]}"#;

    /// The episodes fixture (object form).
    const EPISODES_PAGE: &str = r#"{"total":2,"episodes":[{"number":1},{"number":2}]}"#;

    /// The servers fixture: neko (sub+dub) and koto (sub only).
    const SERVERS_PAGE: &str =
        r#"{"servers":[{"id":"neko","subTypes":["sub","dub"]},{"id":"koto","subTypes":["sub"]}]}"#;

    /// The full fixture set for a series episode resolve.
    fn scripted_pages() -> ScriptedFetcher {
        ScriptedFetcher::new()
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component(NAME)
                )),
                BROWSE_PAGE,
            )
            .page_url("anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes", EPISODES_PAGE)
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/servers",
                SERVERS_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=sub",
                r#"{"sources":[{"url":"TOKEN1","quality":"1080p","isM3U8":true}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=dub",
                r#"{"sources":[{"url":"TOKEN2","quality":"hardsub HD-1","isM3U8":true}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=koto&lang=sub",
                r#"{"sources":[{"url":"TOKEN3","quality":"1080p","isM3U8":true},{"url":"TOKEN3","quality":"1080p","isM3U8":true}]}"#,
            )
            // The relay is alive: the liveness probe fetches the first
            // card and must see a playlist.
            .page_url("prox.anikage.cc/m3u8/TOKEN1", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n3600/index.m3u8\n")
    }

    #[tokio::test]
    async fn resolves_direct_streams_per_provider_and_language() -> Result<(), SourceError> {
        let fetcher = scripted_pages();
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AniKage::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the happy path must resolve: {e}"));
        // neko sub + dub, koto sub (its duplicate token dedupes).
        assert_eq!(streams.len(), 3);

        let sub = &streams[0];
        assert_eq!(sub.url.as_str(), "https://prox.anikage.cc/m3u8/TOKEN1");
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(sub.ttl, TTL);
        assert_eq!(
            sub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · neko · 1080p)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.resolution, Some(1080));
        assert_eq!(sub.meta.source_id.as_deref(), Some("anikage"));
        assert_eq!(sub.meta.source_label.as_deref(), Some("AniKage"));
        assert_eq!(
            sub.meta.request_headers.get("Referer").map(String::as_str),
            Some("https://anikage.cc/")
        );

        let dub = &streams[1];
        assert_eq!(dub.meta.dubbed, Some(true));
        assert_eq!(dub.meta.subbed, Some(false));
        assert_eq!(dub.url.as_str(), "https://prox.anikage.cc/m3u8/TOKEN2");
        assert_eq!(
            dub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Dub · neko · hardsub HD-1)")
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(dub.meta.resolution, Some(720));
        assert_eq!(
            streams[2].url.as_str(),
            "https://prox.anikage.cc/m3u8/TOKEN3"
        );

        // The API calls carried the Cloudflare-bypassing header set.
        assert_eq!(
            fetcher
                .sent_header(
                    "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/servers",
                    "Origin"
                )
                .as_deref(),
            Some("https://anikage.cc")
        );
        assert_eq!(
            fetcher
                .sent_header(
                    "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=sub",
                    "Sec-Fetch-Site"
                )
                .as_deref(),
            Some("same-origin")
        );
        Ok(())
    }

    #[tokio::test]
    async fn mp4_tokens_use_the_stream_path() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component(NAME)
                )),
                BROWSE_PAGE,
            )
            .page_url("anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes", EPISODES_PAGE)
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/servers",
                SERVERS_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=sub",
                r#"{"sources":[{"url":"MPTOKEN","quality":"","isM3U8":false}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=dub",
                r#"{"sources":[]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=koto&lang=sub",
                r#"{"sources":[]}"#,
            )
            .page_url("prox.anikage.cc/stream/MPTOKEN", "MP4DATA");
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AniKage::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the mp4 path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://prox.anikage.cc/stream/MPTOKEN"
        );
        assert_eq!(streams[0].format, Format::Mp4);
        // A missing quality defaults to "HD" in the label.
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · neko · HD)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_array_episode_list_is_accepted() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component(NAME)
                )),
                BROWSE_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes",
                r#"[{"number":1},{"number":2}]"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/servers",
                SERVERS_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=sub",
                r#"{"sources":[{"url":"TOKEN1","quality":"1080p","isM3U8":true}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=neko&lang=dub",
                r#"{"sources":[]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/frieren-beyond-journeys-end/episodes/2/sources?provider=koto&lang=sub",
                r#"{"sources":[]}"#,
            )
            .page_url("prox.anikage.cc/m3u8/TOKEN1", "#EXTM3U\n");
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AniKage::new()
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the array form must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn current_runtime_relay_is_used_without_fetching_media_bodies() -> Result<(), SourceError>
    {
        let fetcher = scripted_pages().page_url(
            "anikage.cc/",
            r#"<script>env: {"PUBLIC_PROXY_URL":"https://og.bakayaro.live"}</script>"#,
        );
        fetcher.remove_url("prox.anikage.cc/m3u8/TOKEN1");
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let streams = AniKage::new()
            .resolve(&ctx, &media_ref(Some(1), Some(2)))
            .await?;
        assert!(!streams.is_empty());
        assert!(
            streams
                .iter()
                .all(|s| s.url.host_str() == Some("og.bakayaro.live"))
        );
        assert!(
            fetcher
                .requests()
                .iter()
                .all(|r| r.url.host_str() != Some("og.bakayaro.live"))
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some(BASE_URL)
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_search_without_a_match_is_not_found() {
        let fetcher = ScriptedFetcher::new().page_url(
            url_key(&format!(
                "https://anikage.cc/api/media/anime/browse?q={}",
                encode_component(NAME)
            )),
            r#"{"data":[{"slug":"other","title":{"english":"Something Else Entirely","romaji":"","native":"","userPreferred":""}}]}"#,
        );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match AniKage::new().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a search miss must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let ctx = ctx_for(&fetcher, None);
        let media = media_ref(Some(1), Some(2));

        match AniKage::new().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("no resolved media must be NotFound, got {other:?}"),
        }
    }

    /// The S1/S2 browse fixture: two season entries sharing one
    /// `imdbId`/`tmdbId`, exactly like `AniKage` catalogs "Reincarnated as
    /// a Sword".
    const SWORD_BROWSE: &str = r#"{"data":[
        {"slug":"S1SLUG","anilistId":139587,"title":{"english":"Reincarnated as a Sword","romaji":"Tensei Shitara Ken Deshita"}},
        {"slug":"S2SLUG","anilistId":159042,"title":{"english":"Reincarnated as a Sword Season 2","romaji":"Tensei Shitara Ken Deshita 2nd Season"}}
    ]}"#;

    /// The arm fixture: both season rows for the Sword (captured from
    /// arm.haglund.dev).
    const ARM_SWORD: &str = r#"[
        {"anidb":16785,"anilist":139587,"myanimelist":49891,"imdb":"tt15483602","themoviedb":134667,"themoviedb-season":1},
        {"anidb":17789,"anilist":159042,"myanimelist":53913,"imdb":"tt15483602","themoviedb":134667,"themoviedb-season":2}
    ]"#;

    /// The shared fetcher with the arm page: the mapping service and the
    /// provider both resolve through it.
    fn sword_pages() -> ScriptedFetcher {
        ScriptedFetcher::new()
            .page_url("arm.haglund.dev/api/v2/imdb?id=tt15483602", ARM_SWORD)
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component("Reincarnated as a Sword")
                )),
                SWORD_BROWSE,
            )
    }

    /// The Sword resolved media: imdb key, both seasons under one id.
    fn sword_media(season: Option<u32>, episode: Option<u32>) -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(134_667),
            imdb_id: Some("tt15483602".to_string()),
            name: "Reincarnated as a Sword".to_string(),
            year: Some(2022),
            season,
            episode,
        }
    }

    /// A provider whose episodes/servers/sources pages hang off one slug.
    fn sword_stream_pages(fetcher: ScriptedFetcher, slug: &str) -> ScriptedFetcher {
        fetcher
            .page_url(
                format!("anikage.cc/api/media/anime/{slug}/episodes"),
                r#"[{"number":1,"seasonNumber":2}]"#,
            )
            .page_url(
                format!("anikage.cc/api/media/anime/{slug}/episodes/1/servers"),
                SERVERS_PAGE,
            )
            .page_url(
                format!(
                    "anikage.cc/api/media/anime/{slug}/episodes/1/sources?provider=neko&lang=sub"
                ),
                r#"{"sources":[{"url":"S2TOKEN","quality":"1080p","isM3U8":true}]}"#,
            )
            .page_url(
                format!(
                    "anikage.cc/api/media/anime/{slug}/episodes/1/sources?provider=neko&lang=dub"
                ),
                r#"{"sources":[]}"#,
            )
            .page_url(
                format!(
                    "anikage.cc/api/media/anime/{slug}/episodes/1/sources?provider=koto&lang=sub"
                ),
                r#"{"sources":[]}"#,
            )
            .page_url(
                "prox.anikage.cc/m3u8/S2TOKEN",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n3600/index.m3u8\n",
            )
    }

    /// A resolve with the shared mapping service wired in — the fetcher
    /// is shared by the provider context and the mapping service so both
    /// see (and record) the same traffic.
    async fn resolve_with_mappings(
        fetcher: &Arc<ScriptedFetcher>,
        resolved: ResolvedMedia,
        media: MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let mappings = MappingService::new(Arc::clone(fetcher) as Arc<dyn Fetcher>);
        let ctx = ctx_for(fetcher.as_ref(), Some(resolved));
        AniKage::with_mappings(mappings).resolve(&ctx, &media).await
    }

    #[tokio::test]
    async fn s2_media_resolves_to_the_s2_slug_not_s1() -> Result<(), SourceError> {
        // The id-first path: arm's S2 row → anilistId 159042 → the S2
        // browse candidate. Name scoring alone would pick the exact-match
        // S1 entry — the regression this test pins.
        let fetcher = Arc::new(sword_stream_pages(sword_pages(), "S2SLUG"));
        let streams = resolve_with_mappings(
            &fetcher,
            sword_media(Some(2), Some(1)),
            MediaRef {
                id: MediaId::Imdb("tt15483602".into()),
                kind: MediaType::Series,
                season: Some(2),
                episode: Some(1),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("the S2 id path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        // The episodes page is only scripted for S2SLUG — resolving the
        // S1 slug would miss the pages and answer NotFound.
        assert_eq!(
            streams[0].url.as_str(),
            "https://prox.anikage.cc/m3u8/S2TOKEN"
        );
        // No detail fetch was needed — the fast path is id equality.
        assert!(
            !fetcher
                .requests()
                .iter()
                .any(|r| r.url.path().contains("S1SLUG")),
            "the S1 entry must never be fetched for an S2 request"
        );
        Ok(())
    }

    #[tokio::test]
    async fn s1_media_resolves_to_the_s1_slug() -> Result<(), SourceError> {
        // The episodes page carries seasonNumber 1 for the S1 entry.
        let fetcher = ScriptedFetcher::new()
            .page_url("arm.haglund.dev/api/v2/imdb?id=tt15483602", ARM_SWORD)
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component("Reincarnated as a Sword")
                )),
                SWORD_BROWSE,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes",
                r#"[{"number":1,"seasonNumber":1}]"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/servers",
                SERVERS_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=neko&lang=sub",
                r#"{"sources":[{"url":"S1TOKEN","quality":"1080p","isM3U8":true}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=neko&lang=dub",
                r#"{"sources":[]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=koto&lang=sub",
                r#"{"sources":[]}"#,
            )
            .page_url(
                "prox.anikage.cc/m3u8/S1TOKEN",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n3600/index.m3u8\n",
            );
        let fetcher = Arc::new(fetcher);
        let streams = resolve_with_mappings(
            &fetcher,
            sword_media(Some(1), Some(1)),
            MediaRef {
                id: MediaId::Imdb("tt15483602".into()),
                kind: MediaType::Series,
                season: Some(1),
                episode: Some(1),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("the S1 id path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://prox.anikage.cc/m3u8/S1TOKEN"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_arm_falls_back_to_anilist_search() -> Result<(), SourceError> {
        // arm answers empty; the anilist search page resolves the S2 id
        // through the title fallback.
        let fetcher = ScriptedFetcher::new()
            .page_url("arm.haglund.dev/api/v2/imdb?id=tt15483602", "[]")
            .page_url(
                "graphql.anilist.co/",
                r#"{"data":{"Page":{"media":[
                    {"id":139587,"idMal":49891,"title":{"romaji":"Tensei Shitara Ken Deshita"}},
                    {"id":159042,"idMal":53913,"title":{"romaji":"Tensei Shitara Ken Deshita 2nd Season"}}
                ]}}}"#,
            )
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component("Reincarnated as a Sword")
                )),
                SWORD_BROWSE,
            );
        let fetcher = Arc::new(sword_stream_pages(fetcher, "S2SLUG"));
        let streams = resolve_with_mappings(
            &fetcher,
            sword_media(Some(2), Some(1)),
            MediaRef {
                id: MediaId::Imdb("tt15483602".into()),
                kind: MediaType::Series,
                season: Some(2),
                episode: Some(1),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("the fallback path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://prox.anikage.cc/m3u8/S2TOKEN"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_mapping_outage_falls_back_to_name_scoring() {
        // No arm, no anilist, no anilistId on the candidates: the
        // name-score path must still resolve the exact-title S1 entry.
        let browse = r#"{"data":[
            {"slug":"S1SLUG","title":{"english":"Reincarnated as a Sword","romaji":"Tensei Shitara Ken Deshita"}},
            {"slug":"S2SLUG","title":{"english":"Reincarnated as a Sword Season 2","romaji":"Tensei Shitara Ken Deshita 2nd Season"}}
        ]}"#;
        let fetcher = ScriptedFetcher::new()
            .page(
                |url| {
                    url.host_str() == Some("arm.haglund.dev")
                        || url.host_str() == Some("graphql.anilist.co")
                },
                r#"{"data":{}}"#,
            )
            .page_url(
                url_key(&format!(
                    "https://anikage.cc/api/media/anime/browse?q={}",
                    encode_component("Reincarnated as a Sword")
                )),
                browse,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes",
                r#"[{"number":1}]"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/servers",
                SERVERS_PAGE,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=neko&lang=sub",
                r#"{"sources":[{"url":"TOKENX","quality":"1080p","isM3U8":true}]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=neko&lang=dub",
                r#"{"sources":[]}"#,
            )
            .page_url(
                "anikage.cc/api/media/anime/S1SLUG/episodes/1/sources?provider=koto&lang=sub",
                r#"{"sources":[]}"#,
            )
            .page_url("prox.anikage.cc/m3u8/TOKENX", "#EXTM3U\n");
        let fetcher = Arc::new(fetcher);
        let media = MediaRef {
            id: MediaId::Imdb("tt15483602".into()),
            kind: MediaType::Series,
            season: Some(1),
            episode: Some(1),
        };
        let streams = resolve_with_mappings(&fetcher, sword_media(Some(1), Some(1)), media)
            .await
            .unwrap_or_else(|e| panic!("the name fallback must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://prox.anikage.cc/m3u8/TOKENX"
        );
    }

    #[test]
    fn parses_heights_from_quality_labels() {
        for (quality, expected) in [
            ("1080p", Some(1080)),
            ("720p", Some(720)),
            ("hardsub HD-1", Some(720)),
            ("VidPlay-1 auto", None),
            ("480", Some(480)),
        ] {
            assert_eq!(
                height_from_quality(quality),
                expected,
                "quality {quality:?}"
            );
        }
    }
}
