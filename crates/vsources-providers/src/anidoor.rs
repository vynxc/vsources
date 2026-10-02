//! `AniDoor`: deterministic megaplay embeds from the `anidoor.me` SPA.
//!
//! Ports `src/source/AniDoor.js` — an anime portal that uses `AniList`
//! GraphQL for metadata and a public `sources.json` config for embed
//! URL templates. The embed URLs are fully determined by the `AniList` id
//! (plus the MAL id for some hosts) and the episode number:
//!
//! 1. media lookup — `AniList` GraphQL
//!    (`POST https://graphql.anilist.co`, `SEARCH_MATCH` order), falling
//!    back to Jikan (`GET https://api.jikan.moe/v4/anime?q=…&limit=5`)
//!    and then Kitsu (`GET https://kitsu.app/api/edge/anime?filter[text]=…`)
//!    when `AniList` is down (each stage is best-effort upstream);
//! 2. the best title match among entries whose format matches the
//!    request kind — movies only match `MOVIE`, series only match
//!    `TV`/`TV_SHORT`/`OVA`/`ONA`/`SPECIAL` (this is what stops a
//!    3-minute MUSIC video from matching a live-action movie request) —
//!    with the threshold raised from 60 to 75 because one-sided
//!    containment matches are too loose;
//! 3. `GET https://anidoor.me/assets/sources.json` — embed templates
//!    `{base, path, type, dub}` with `{al}`, `{mal}`, `{s}`, `{e}`
//!    placeholders; only `megaplay.buzz` templates survive the upstream
//!    host filter (vidnest/tryembed/nightslayer/dropfile are dead or
//!    client-side-only, so upstream already skipped them);
//! 4. each built URL is resolved through the shared
//!    [`ExtractorRegistry`] (the upstream resolver's post-source
//!    extraction step, folded into the provider here) with the
//!    provider's metadata merged into the extracted streams.
//!
//! Mappings and cuts (vs. upstream):
//! - `meta.title` (`{title} (Sub · S2)`) → [`Stream::label`]; the
//!   producing extractor stays attributed in `meta.extractor_label`.
//! - `meta.countryCodes` → `meta.languages`.
//! - The upstream module-level `sources.json` cache (24h TTL) is cut:
//!   the parent's `CachedSource` bounds how often this provider (and
//!   therefore the config fetch) re-runs, so a second cache would only
//!   add state.
//! - No result cache here — the parent's `CachedSource` wrapper owns
//!   caching; streams keep the extractor's ttl.
//! - `getTmdbId`/`getTmdbNameAndYear` → `ctx.media`; missing media (no
//!   title to search) answers [`SourceError::NotFound`].
//! - Extractor failures are skipped per embed — upstream
//!   `extractorRegistry.handle(...).catch(() => [])`.
//! - No `/proxy` routing and no `ExternalUrl` fall-through: unresolved
//!   embeds simply drop (the registry's external fallback switch owns
//!   that behavior when enabled).
//! - Explicit `User-Agent`/`AbortSignal` dropped — the fetcher sends
//!   browser-like headers and owns timeouts.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; `normalize` still strips combining marks (U+0300–036F),
//!   and precomposed accents drop out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The SPA root, upstream `BASE_URL`.
const BASE_URL: &str = "https://anidoor.me";
/// The embed-template config, upstream `SOURCES_JSON_URL`.
const SOURCES_JSON_URL: &str = "https://anidoor.me/assets/sources.json";
/// The `AniList` GraphQL endpoint.
const ANILIST_GQL: &str = "https://graphql.anilist.co";
/// The Jikan fallback API.
const JIKAN_API: &str = "https://api.jikan.moe/v4/anime";
/// The Kitsu fallback API.
const KITSU_API: &str = "https://kitsu.app/api/edge/anime";
/// This provider's id, for scrape diagnostics.
const PROVIDER_ID: &str = "anidoor";
/// Upstream `AniList` timeout (`timeout: { request: 15000 }`).
const ANILIST_TIMEOUT: Duration = Duration::from_secs(15);
/// Upstream fallback-API timeouts (`AbortSignal` 10s).
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// `AniList` formats that count as real movies (MUSIC/NOVEL do not —
/// megaplay has no stream for them).
const MOVIE_FORMATS: [&str; 1] = ["MOVIE"];
/// `AniList` formats that count as real episodic anime.
const SERIES_FORMATS: [&str; 5] = ["TV", "TV_SHORT", "OVA", "ONA", "SPECIAL"];

/// The `AniList` GraphQL query, verbatim from upstream.
const ANILIST_QUERY: &str = "
    query($search: String) {
      Page(page: 1, perPage: 10) {
        media(type: ANIME, search: $search, sort: [SEARCH_MATCH, POPULARITY_DESC]) {
          id
          idMal
          title { romaji english native userPreferred }
          format
          episodes
          duration
        }
      }
    }";

/// A media hit from AniList/Jikan/Kitsu, in the common shape the scoring
/// loop reads.
struct MediaEntry {
    /// `AniList` id (Jikan/Kitsu have none).
    anilist_id: Option<u64>,
    /// MAL id (Kitsu has none).
    mal_id: Option<u64>,
    /// Title variants, in upstream scoring order: english, romaji,
    /// userPreferred.
    titles: Vec<String>,
    /// The `AniList` format tag.
    format: Option<String>,
}

/// The `anidoor.me` provider: deterministic megaplay embeds.
pub struct AniDoor {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// Static descriptor.
    info: SourceInfo,
    /// The embed resolver — upstream's post-source extraction stage.
    registry: Arc<ExtractorRegistry>,
}

impl AniDoor {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// A provider resolving megaplay embeds through `registry`.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            mappings: None,
            info: SourceInfo {
                id: PROVIDER_ID.to_string(),
                label: "AniDoor".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(parse_url(BASE_URL)),
                priority: 0,
                // Upstream leaves `this.domainKey` unset.
                domain_key: None,
            },
            registry,
        }
    }
}

#[async_trait]
impl Source for AniDoor {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        if let Some(ids) = crate::anime_mapping::ids(self.mappings.as_ref(), ctx, media).await
            && let Ok(streams) = self.resolve_inner(ctx, media, Some(ids)).await
            && !streams.is_empty()
        {
            return Ok(streams);
        }
        self.resolve_inner(ctx, media, None).await
    }
}

/// The resolved identity an embed template keys on: the best media
/// match's ids, the request's kind, and its episode number.
struct EmbedTarget {
    /// The `AniList` id (`{al}` in template paths).
    anilist_id: Option<u64>,
    /// The MAL id (`{mal}` in template paths).
    mal_id: Option<u64>,
    /// Whether the request wants a movie (`type` "movie") embed.
    want_movie: bool,
    /// The episode number (`{e}` in template paths).
    episode: u32,
}

impl AniDoor {
    /// Steps 4: build the megaplay embed URLs from the templates and
    /// resolve each through the registry, merging this provider's
    /// metadata into the extracted streams.
    async fn embed_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        templates: Vec<SourceTemplate>,
        target: EmbedTarget,
        title: &str,
    ) -> Result<Vec<Stream>, SourceError> {
        let EmbedTarget {
            anilist_id,
            mal_id,
            want_movie,
            episode,
        } = target;
        let mut streams = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let anidoor_root = parse_url(&format!("{BASE_URL}/"));
        for template in templates {
            // Movies use type "movie", series type "anime".
            if template.content_type != if want_movie { "movie" } else { "anime" } {
                continue;
            }
            // Templates keyed on ids we do not have cannot be built.
            if template.path.contains("{mal}") && mal_id.is_none() {
                continue;
            }
            if template.path.contains("{al}") && anilist_id.is_none() {
                continue;
            }
            // Only megaplay.buzz is resolvable server-side; the other
            // hosts are skipped upstream for the same reason.
            if !template.base.contains("megaplay.buzz") {
                continue;
            }

            let path = template
                .path
                .replacen(
                    "{al}",
                    &anilist_id.map(|id| id.to_string()).unwrap_or_default(),
                    1,
                )
                .replacen(
                    "{mal}",
                    &mal_id.map(|id| id.to_string()).unwrap_or_default(),
                    1,
                )
                .replacen("{s}", "1", 1)
                .replacen("{e}", &episode.to_string(), 1);
            let url = match Url::parse(&format!("{}{}", template.base, path)) {
                Ok(url) => url,
                Err(error) => {
                    return Err(SourceError::scrape(
                        PROVIDER_ID,
                        format!("unparsable embed URL {path:?}: {error}"),
                    ));
                }
            };
            if !seen_urls.insert(url.as_str().to_string()) {
                continue;
            }

            let audio = if template.dub { "Dub" } else { "Sub" };
            let languages = if template.dub {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            };
            let source_name = template
                .name
                .or(template.id)
                .unwrap_or_else(|| host_fragment(&template.base));
            let label = format!("{title} ({audio} · {source_name})");

            // Upstream: the resolver extracts every embed and merges the
            // source meta into the extractor's streams; extraction
            // errors are swallowed per URL.
            let embed_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: ctx.media.clone(),
                source_id: Some(self.info.id.as_str()),
                referer: Some(&anidoor_root),
            };
            let Ok(extracted) = self.registry.extract(&embed_ctx, &url).await else {
                continue;
            };
            for mut stream in extracted {
                stream.meta.source_id = Some(self.info.id.clone());
                stream.meta.source_label = Some(self.info.label.clone());
                stream.meta.languages.clone_from(&languages);
                stream.meta.dubbed = Some(template.dub);
                stream.meta.subbed = Some(!(template.dub));
                stream.label = Some(label.clone());
                streams.push(stream);
            }
        }
        Ok(streams)
    }
}

/// Steps 1–2: the best media match's ids — `AniList` (→ Jikan → Kitsu)
/// entries scored by title, hard-filtered by the request kind's
/// formats (this is what stops a music video from matching a movie
/// request), with the threshold raised from 60 to 75 because one-sided
/// containment matches are too loose.
async fn media_ids(
    ctx: &ResolveCtx<'_>,
    name: &str,
    want_movie: bool,
) -> Option<(Option<u64>, Option<u64>)> {
    let allowed_formats: &[&str] = if want_movie {
        &MOVIE_FORMATS
    } else {
        &SERIES_FORMATS
    };
    let entries = resolve_media_list(ctx, name).await;
    if entries.is_empty() {
        return None;
    }
    let name_norm = normalize(name);
    let mut best: Option<&MediaEntry> = None;
    let mut best_score = 0.0_f64;
    for entry in &entries {
        if !entry
            .format
            .as_deref()
            .is_some_and(|format| allowed_formats.contains(&format))
        {
            continue;
        }
        for candidate in &entry.titles {
            let candidate_norm = normalize(candidate);
            if candidate_norm.is_empty() {
                continue;
            }
            let score = if candidate_norm == name_norm {
                100.0
            } else if candidate_norm.contains(&name_norm) || name_norm.contains(&candidate_norm) {
                inclusion_score(&candidate_norm, &name_norm)
            } else {
                0.0
            };
            if score > best_score {
                best_score = score;
                best = Some(entry);
            }
        }
    }
    if best_score < 75.0 {
        return None;
    }
    let best = best?;
    let (anilist_id, mal_id) = (best.anilist_id, best.mal_id);
    // Without either id no URL can be built (rare Jikan/Kitsu edge).
    (anilist_id.is_some() || mal_id.is_some()).then_some((anilist_id, mal_id))
}

/// The `sources.json` template entry.
#[derive(Deserialize)]
struct SourceTemplate {
    /// Display name (e.g. `S2`).
    #[serde(default)]
    name: Option<String>,
    /// Template id (e.g. `megaplay-sub`).
    #[serde(default)]
    id: Option<String>,
    /// The embed host root (e.g. `https://megaplay.buzz`).
    #[serde(default)]
    base: String,
    /// The path template with `{al}`/`{mal}`/`{s}`/`{e}` placeholders.
    #[serde(default)]
    path: String,
    /// `movie` or `anime` — must match the request kind.
    #[serde(rename = "type", default)]
    content_type: String,
    /// Whether this template is the dub variant.
    #[serde(default)]
    dub: bool,
}

/// The host fragment of an embed base, for entries with neither name
/// nor id.
fn host_fragment(base: &str) -> String {
    base.split("//")
        .nth(1)
        .map(|rest| rest.split('/').next().unwrap_or_default().to_string())
        .unwrap_or_default()
}

/// `AniList` → Jikan → Kitsu, each stage best-effort (upstream wraps
/// every one in try/catch and falls to the next).
async fn resolve_media_list(ctx: &ResolveCtx<'_>, name: &str) -> Vec<MediaEntry> {
    if let Some(entries) = anilist_lookup(ctx, name).await
        && !entries.is_empty()
    {
        return entries;
    }
    if let Some(entries) = jikan_lookup(ctx, name).await
        && !entries.is_empty()
    {
        return entries;
    }
    kitsu_lookup(ctx, name).await.unwrap_or_default()
}

/// The `AniList` GraphQL stage.
async fn anilist_lookup(ctx: &ResolveCtx<'_>, name: &str) -> Option<Vec<MediaEntry>> {
    let body =
        serde_json::json!({ "query": ANILIST_QUERY, "variables": { "search": name } }).to_string();
    let request = FetchRequest::post(parse_url(ANILIST_GQL), body)
        .with_timeout(ANILIST_TIMEOUT)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
    let data: Value = fetch_json(ctx, request).await?;
    let media = data.pointer("/data/Page/media")?.as_array()?.clone();
    Some(
        media
            .iter()
            .map(|entry| MediaEntry {
                anilist_id: entry.get("id").and_then(Value::as_u64),
                mal_id: entry.get("idMal").and_then(Value::as_u64),
                titles: title_variants(entry),
                format: entry
                    .get("format")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
            .collect(),
    )
}

/// The Jikan stage — MAL ids only, mapped to the `AniList` entry shape.
async fn jikan_lookup(ctx: &ResolveCtx<'_>, name: &str) -> Option<Vec<MediaEntry>> {
    let url = parse_url(&format!(
        "{JIKAN_API}?q={}&limit=5&sfw=true",
        encode_component(name)
    ));
    let data: Value =
        fetch_json(ctx, FetchRequest::get(url).with_timeout(FALLBACK_TIMEOUT)).await?;
    let results = data.get("data")?.as_array()?.clone();
    Some(
        results
            .iter()
            .map(|entry| {
                let title = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Jikan reports "TV"/"Movie" capitalized; upstream maps
                // everything but its exact "MOVIE" tag to the series
                // bucket — kept verbatim.
                let kind = entry.get("type").and_then(Value::as_str);
                let format = match kind {
                    Some("MOVIE") => "MOVIE",
                    _ => "TV",
                };
                MediaEntry {
                    anilist_id: None,
                    mal_id: entry.get("mal_id").and_then(Value::as_u64),
                    titles: vec![
                        entry
                            .get("title_english")
                            .or_else(|| entry.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry
                            .get("title_japanese")
                            .or_else(|| entry.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        title.to_string(),
                    ],
                    format: Some(format.to_string()),
                }
            })
            .collect(),
    )
}

/// The Kitsu stage — no `AniList` or MAL ids, but title matching still
/// works (and the `{al}`/`{mal}` guards then skip every template).
async fn kitsu_lookup(ctx: &ResolveCtx<'_>, name: &str) -> Option<Vec<MediaEntry>> {
    let url = parse_url(&format!(
        "{KITSU_API}?filter[text]={}&page[limit]=5",
        encode_component(name)
    ));
    let request = FetchRequest::get(url)
        .with_timeout(FALLBACK_TIMEOUT)
        .with_header("Accept", "application/json");
    let data: Value = fetch_json(ctx, request).await?;
    let results = data.get("data")?.as_array()?.clone();
    Some(
        results
            .iter()
            .map(|entry| {
                let titles = entry.pointer("/attributes/titles");
                let canonical = entry
                    .pointer("/attributes/canonicalTitle")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let subtype = entry
                    .pointer("/attributes/subtype")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                MediaEntry {
                    anilist_id: None,
                    mal_id: None,
                    titles: vec![
                        titles
                            .and_then(|titles| titles.get("en"))
                            .or_else(|| entry.pointer("/attributes/canonicalTitle"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        titles
                            .and_then(|titles| titles.get("en_jp"))
                            .or_else(|| entry.pointer("/attributes/canonicalTitle"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        canonical.to_string(),
                    ],
                    format: Some((if subtype == "movie" { "MOVIE" } else { "TV" }).to_string()),
                }
            })
            .collect(),
    )
}

/// The scoring titles of an `AniList` media entry: english, romaji,
/// userPreferred (empty ones dropped, like the upstream filter).
fn title_variants(entry: &Value) -> Vec<String> {
    ["english", "romaji", "userPreferred"]
        .iter()
        .filter_map(|field| {
            entry
                .pointer(&format!("/title/{field}"))
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// `fetchSourcesJson`: the embed-template config (upstream caches it
/// for 24h; the parent's result cache bounds the refetch rate here).
async fn fetch_sources_json(ctx: &ResolveCtx<'_>) -> Option<Vec<SourceTemplate>> {
    let request = FetchRequest::get(parse_url(SOURCES_JSON_URL))
        .with_timeout(FALLBACK_TIMEOUT)
        .with_header("Accept", "application/json");
    let data: Value = fetch_json(ctx, request).await?;
    let templates: Vec<SourceTemplate> = serde_json::from_value(data).ok()?;
    // Entries without a base or path cannot build a URL.
    let templates = templates
        .into_iter()
        .filter(|template| !template.base.is_empty() && !template.path.is_empty())
        .collect::<Vec<_>>();
    (!templates.is_empty()).then_some(templates)
}

/// A best-effort JSON fetch — every failure is a miss (upstream wraps
/// each stage in try/catch, so no error escapes the lookup chain).
async fn fetch_json(ctx: &ResolveCtx<'_>, request: FetchRequest) -> Option<Value> {
    let response = ctx.fetcher.request(request).await.ok()?;
    if response.status != 200 {
        return None;
    }
    response.json::<Value>().ok()
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

/// The episode number for the `{e}` placeholder — the reference's
/// episode for series, 1 for movies.
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

/// Parse a URL the provider built — a structural surprise, not a miss.
fn parse_url(raw: &str) -> Url {
    Url::parse(raw).unwrap_or_else(|e| panic!("the AniDoor URL {raw:?} must parse: {e}"))
}

impl AniDoor {
    async fn resolve_inner(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        mapped: Option<vsources_core::mappings::SeasonIds>,
    ) -> Result<Vec<Stream>, SourceError> {
        // Upstream resolves TMDB (getTmdbId + getTmdbNameAndYear) and
        // searches by name; without pre-resolved media there is no title.
        let Some(resolved) = ctx.media.as_ref() else {
            return Err(SourceError::NotFound);
        };
        let title = display_title(&resolved.name, resolved.year, media);
        let episode = target_episode(media);

        // The request kind selects which AniList formats may match.
        let want_movie = media.season.is_none();

        // Steps 1–2: the best media match with its ids.
        let ids = match mapped {
            Some(ids) => Some((Some(ids.anilist_id), ids.mal_id)),
            None => media_ids(ctx, &resolved.name, want_movie).await,
        };
        let Some((anilist_id, mal_id)) = ids else {
            return Err(SourceError::NotFound);
        };

        // Step 3: the embed-template config.
        let Some(templates) = fetch_sources_json(ctx).await else {
            return Err(SourceError::NotFound);
        };

        // Step 4: build the embed URLs and resolve them.
        let target = EmbedTarget {
            anilist_id,
            mal_id,
            want_movie,
            episode,
        };
        self.embed_streams(ctx, templates, target, &title).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use vsources_core::error::ExtractorError;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{Extractor, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;
    use vsources_core::types::MediaId;

    /// The searched title.
    const NAME: &str = "Frieren: Beyond Journey's End";

    /// A fetcher serving canned bodies keyed by `host + path?query`
    /// (the extractors' `ScriptedFetcher` pattern — `pub(crate)` there,
    /// so a per-module copy lives here).
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, String>>,
    }

    impl ScriptedFetcher {
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
            }
        }

        /// Serve `url` (host + path + query) with `body`.
        fn page(self, url: impl Into<String>, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(url.into(), body.into());
            self
        }
    }

    /// The mock's page key: host + path + query.
    fn key_of(url: &Url) -> String {
        let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        format!(
            "{}{}{}",
            url.host_str().unwrap_or_default(),
            url.path(),
            query
        )
    }

    /// The page key for a URL string, canonicalized exactly like the
    /// provider's wire requests (the `url` crate percent-encodes some
    /// query characters, e.g. `'`).
    fn url_key(raw: &str) -> String {
        let url = Url::parse(raw).unwrap_or_else(|e| panic!("valid URL {raw:?}: {e}"));
        key_of(&url)
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            let key = key_of(&request.url);
            let body = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned();
            match body {
                Some(body) => Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: std::collections::BTreeMap::from([(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// The stub's stream TTL — thirty minutes.
    const STUB_TTL: Duration = Duration::from_mins(30);

    /// A stand-in for the Megaplay extractor: claims megaplay.buzz
    /// embeds and answers one direct HLS stream per token.
    struct StubMegaplay;

    #[async_trait]
    impl Extractor for StubMegaplay {
        fn id(&self) -> &'static str {
            "megaplay-stub"
        }

        fn label(&self) -> &'static str {
            "MegaplayStub"
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            url.host_str() == Some("megaplay.buzz")
        }

        async fn extract(
            &self,
            _ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            let token = url.path().trim_matches('/').replace('/', "-");
            let direct = Url::parse(&format!("https://cdn.example.com/{token}.m3u8"))
                .unwrap_or_else(|e| panic!("valid stub URL: {e}"));
            Ok(vec![Stream::new(direct, Format::Hls).with_ttl(STUB_TTL)])
        }
    }

    /// A registry over the stub extractor.
    fn registry() -> Arc<ExtractorRegistry> {
        Arc::new(ExtractorRegistry::new(vec![Arc::new(StubMegaplay)]))
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
    fn media_ref(kind: MediaType, season: Option<u32>, episode: Option<u32>) -> MediaRef {
        MediaRef {
            id: MediaId::Tmdb(154_587),
            kind,
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

    /// The `AniList` GraphQL fixture.
    const ANILIST_PAGE: &str = r#"{"data":{"Page":{"media":[
        {"id":154587,"idMal":52991,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End","userPreferred":"Sousou no Frieren"},"format":"TV","episodes":28,"duration":24},
        {"id":170068,"idMal":null,"title":{"romaji":"Sousou no Frieren: Mahou","english":null,"userPreferred":"Sousou no Frieren: Mahou"},"format":"ONA","episodes":2,"duration":3}
    ]}}}"#;

    /// The `AniList` GraphQL fixture with the movie-format entry first.
    const MOVIE_ANILIST_PAGE: &str = r#"{"data":{"Page":{"media":[
        {"id":154587,"idMal":52991,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End","userPreferred":"Sousou no Frieren"},"format":"MOVIE","episodes":1,"duration":118}
    ]}}}"#;

    /// The sources.json fixture, trimmed to the megaplay and vidnest
    /// families in the real shape.
    const SOURCES_JSON: &str = r#"[
        {"id":"vidnest-ap-sub","name":"S1","base":"https://vidnest.fun","path":"/animepahe/{al}/{e}/sub","type":"anime","dub":false},
        {"id":"megaplay-sub","name":"S2","base":"https://megaplay.buzz","path":"/stream/ani/{al}/{e}/sub","type":"anime","dub":false},
        {"id":"megaplay-dub","name":"D2","base":"https://megaplay.buzz","path":"/stream/ani/{al}/{e}/dub","type":"anime","dub":true},
        {"id":"megaplay-sub-alt","name":"S2(alt)","base":"https://megaplay.buzz","path":"/stream/mal/{mal}/{e}/sub","type":"anime","dub":false},
        {"id":"megaplay-movie-sub","name":"S2","base":"https://megaplay.buzz","path":"/stream/ani/{al}/1/sub","type":"movie","dub":false},
        {"id":"megaplay-movie-dub","name":"D2","base":"https://megaplay.buzz","path":"/stream/ani/{al}/1/dub","type":"movie","dub":true}
    ]"#;

    #[tokio::test]
    async fn resolves_series_embeds_through_the_registry() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page("graphql.anilist.co/", ANILIST_PAGE)
            .page("anidoor.me/assets/sources.json", SOURCES_JSON);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        let streams = AniDoor::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series path must resolve: {e}"));
        // The vidnest entry is filtered (not megaplay); the movie
        // entries are filtered (type mismatch); the ONA candidate loses
        // the format filter. Sub, dub, and the MAL-keyed variant remain.
        assert_eq!(streams.len(), 3);

        let sub = &streams[0];
        assert_eq!(
            sub.url.as_str(),
            "https://cdn.example.com/stream-ani-154587-2-sub.m3u8"
        );
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(
            sub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · S2)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.source_id.as_deref(), Some("anidoor"));
        assert_eq!(sub.meta.source_label.as_deref(), Some("AniDoor"));
        assert_eq!(sub.meta.extractor_label.as_deref(), Some("MegaplayStub"));

        let dub = &streams[1];
        assert_eq!(dub.meta.dubbed, Some(true));
        assert_eq!(dub.meta.subbed, Some(false));
        assert_eq!(
            dub.url.as_str(),
            "https://cdn.example.com/stream-ani-154587-2-dub.m3u8"
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(
            dub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Dub · D2)")
        );

        let alt = &streams[2];
        assert_eq!(
            alt.url.as_str(),
            "https://cdn.example.com/stream-mal-52991-2-sub.m3u8"
        );
        assert_eq!(
            alt.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · S2(alt))")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolves_movie_embeds_with_the_movie_templates() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher::new()
            .page("graphql.anilist.co/", MOVIE_ANILIST_PAGE)
            .page("anidoor.me/assets/sources.json", SOURCES_JSON);
        let ctx = ctx_for(&fetcher, Some(resolved_media(None, None)));
        let media = media_ref(MediaType::Movie, None, None);

        let streams = AniDoor::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the movie path must resolve: {e}"));
        // The series entries are format-filtered out; movie sub + dub
        // remain, with the literal `1` episode of the movie templates.
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.com/stream-ani-154587-1-sub.m3u8"
        );
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End (2023) (Sub · S2)")
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://cdn.example.com/stream-ani-154587-1-dub.m3u8"
        );
        Ok(())
    }

    #[tokio::test]
    async fn anilist_failure_falls_back_to_jikan() -> Result<(), SourceError> {
        // AniList unregistered (misses); Jikan carries the MAL id; the
        // {al}-keyed templates are skipped, the {mal} one resolves.
        let fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "{JIKAN_API}?q={}&limit=5&sfw=true",
                    encode_component(NAME)
                )),
                r#"{"data":[{"mal_id":52991,"title":"Frieren: Beyond Journey's End","title_japanese":"Sousou no Frieren","title_english":"Frieren: Beyond Journey's End","type":"TV","episodes":28}]}"#,
            )
            .page("anidoor.me/assets/sources.json", SOURCES_JSON);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        let streams = AniDoor::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the Jikan fallback must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.com/stream-mal-52991-2-sub.m3u8"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_media_match_below_the_threshold_is_not_found() {
        // The only candidate is a loose containment match (~50).
        let fetcher = ScriptedFetcher::new()
            .page(
                "graphql.anilist.co/",
                r#"{"data":{"Page":{"media":[{"id":1,"idMal":1,"title":{"romaji":"Frieren","english":null,"userPreferred":"Frieren"},"format":"TV"}]}}}"#,
            )
            .page("anidoor.me/assets/sources.json", SOURCES_JSON);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        match AniDoor::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a loose match must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_series_request_rejects_movie_formats() {
        // The only candidate is a MOVIE while an episode was requested.
        let fetcher = ScriptedFetcher::new()
            .page(
                "graphql.anilist.co/",
                r#"{"data":{"Page":{"media":[{"id":21,"idMal":21,"title":{"romaji":"Frieren","english":"Frieren: Beyond Journey's End","userPreferred":"Frieren"},"format":"MOVIE"}]}}}"#,
            )
            .page("anidoor.me/assets/sources.json", SOURCES_JSON);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        match AniDoor::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a format mismatch must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_config_is_not_found() {
        let fetcher = ScriptedFetcher::new().page("graphql.anilist.co/", ANILIST_PAGE);
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        match AniDoor::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing sources.json must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let ctx = ctx_for(&fetcher, None);
        let media = media_ref(MediaType::Series, Some(1), Some(2));

        match AniDoor::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("no resolved media must be NotFound, got {other:?}"),
        }
    }

    #[test]
    fn normalizes_titles_for_matching() {
        for (input, expected) in [
            (
                "  Frieren: Beyond  Journey's End ",
                "frieren beyond journeys end",
            ),
            ("Sōusou no Frieren", "susou no frieren"),
        ] {
            assert_eq!(normalize(input), expected, "input {input:?}");
        }
    }
}
