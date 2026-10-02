//! `AnimeKai`: sub+dub anime HLS via `animekai.at` / `zokoanime.video`.
//!
//! Ports `src/source/AnimeKai.js` (flow verified live upstream, pure
//! `Node.js`, no browser). Two chains produce the same streams:
//!
//! **Direct path (primary, "Task 64" upstream):**
//! `animekai.at` hard-403s datacenter egress ("Just a moment"), while
//! `zokoanime.video` — the actual stream host `animekai` embeds —
//! serves its `/stream/mal/…` payloads to datacenter IPs. The MAL id
//! comes from an `AniList` GraphQL search (reliable, season-aware,
//! exposes `idMal`):
//!
//! 1. `POST https://graphql.anilist.co` — the title, and for seasons
//!    past season 1 the `"Nth season"` variant too; entries are matched
//!    with a fuzzy score (exact 100, substring ratio × 90) plus season
//!    alignment: a title's `Nth season`/`Season N` suffix must match
//!    the requested season (+25), else a multi-season request
//!    penalizes the base entry (−20). Threshold 60.
//! 2. `GET https://zokoanime.video/stream/mal/{malId}/{ep}/{sub|dub}`
//!    (Referer `animekai.at/`) → `window.__P="…"`.
//! 3. Deobfuscate: base64 → XOR `"otaku-embed-v1"` → JSON `{src:
//!    m3u8, subtitles: […]}`.
//!
//! **Site-search path (fallback, "Task 41b"):** works when
//! `animekai.at`'s Cloudflare relents (residential/device egress):
//!
//! 1. `GET /?s={candidate}` — the candidate ladder is the full name,
//!    the name before a `:`/`–`/`—`, the first two words, then the
//!    first word; the first candidate with results wins.
//! 2. Watch links `/watch/{slug}` are scored (apostrophes are dropped
//!    BEFORE tokenizing so "Journey's" == "journeys"); threshold 60.
//! 3. `GET /watch/{slug}/` → `MAL_ID` (or the
//!    `myanimelist.net/anime/{id}/` link every watch page carries).
//! 4. The zoko stream, as above.
//!
//! zoko's `/dub` endpoint silently mirrors the sub file when a dub is
//! missing (live-verified: Frieren E1 sub/dub tokens resolve to
//! byte-identical segments while One Piece E1 differs). After both
//! categories resolve, a 1-byte `Content-Range` probe compares the
//! first segment of each; equal totals mean the DUB rows would replay
//! the sub media, and they are dropped. An inconclusive probe (the CDN
//! is unreachable or Cloudflare-gated) keeps both — never lose a real
//! dub.
//!
//! The m3u8 only plays with `Referer: https://zokoanime.video/`;
//! upstream routed it through a server-side `/proxy`, this library has
//! no server, so the playlist ships through the `animekai` extractor
//! with the Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's
//!   TMDB resolution: name/year come from `ctx.media`, season/episode
//!   from the [`MediaRef`]. Without `ctx.media` there is no title to
//!   search → [`SourceError::NotFound`].
//! - The JS's three-transport ladder for the zoko page (got h2 → got
//!   h1 → plain fetch: "zoko serves a compact no-player variant to
//!   some transports") and the `curl`/got-scraping split for
//!   `animekai.at` both collapse onto the shared fetcher, which owns
//!   browser TLS impersonation, Cloudflare solving, and per-host
//!   queueing.
//! - `meta.title` becomes the stream label, `meta.countryCodes` become
//!   `meta.languages`, `meta.height` becomes `meta.resolution`
//!   (1080), and zoko's inline `{lang, label, src}` VTT subtitles
//!   become subtitle tracks. The JS's synthetic subtitle ids
//!   (`lang` + index, 8 chars) are cut — [`SubtitleTrack`] has no id
//!   field. `this.ttl` (10min) is the stream TTL.
//! - Cut: the JS's `console.log` diagnostics (no logging facade in
//!   this crate) and the per-source result cache (the parent's
//!   `CachedSource` owns it).
//! - Patterns are ported as `scraper` selectors and byte scanners
//!   (this crate has no regex engine).
//!
//! [`SubtitleTrack`]: vsources_core::types::SubtitleTrack

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{
    CountryCode, Format, MediaRef, MediaType, SourceInfo, Stream, SubtitleTrack,
};
use vsources_extractors::ExtractorRegistry;

/// The provider id (upstream `this.id`).
const ID: &str = "animekai";
/// Upstream result lifetime: 10min.
const TTL: Duration = Duration::from_mins(10);
/// Fuzzy match threshold for both the `AniList` and the site-search
/// scorers.
const MIN_MATCH_SCORE: f64 = 60.0;
/// The XOR key for the `window.__P` payload — upstream's central
/// `site-secrets.cjs` registry (`OTAKU_XOR_KEY`), reverse-engineered
/// out of the public site, with the same default value.
const OBF_KEY: &str = "otaku-embed-v1";

/// Ordinal season labels, 1–10 (upstream `ORDINALS`).
const ORDINALS: [&str; 11] = [
    "", "1st", "2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th", "10th",
];

/// The `AniList` GraphQL query (upstream `ANILIST_QUERY`).
const ANILIST_QUERY: &str = "query ($search: String) { Page(perPage: 8) { media(search: $search, type: ANIME) { idMal startDate { year } title { romaji english } } } }";

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://animekai.at").unwrap_or_else(|_| panic!("the AnimeKai base URL must parse"))
});
static ANILIST_GQL: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://graphql.anilist.co")
        .unwrap_or_else(|_| panic!("the AniList GraphQL endpoint must parse"))
});

static ALL_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a").unwrap_or_else(|_| panic!("valid a selector")));

/// An `AniList` search entry with a MAL id.
struct MalEntry {
    /// The MAL id (`idMal`).
    mal_id: u64,
    /// The romaji title.
    romaji: String,
    /// The English title.
    english: String,
}

/// The `AniList` GraphQL envelope.
#[derive(Deserialize)]
struct AniListResponse {
    /// The response data.
    #[serde(default)]
    data: Option<AniListData>,
}

/// The GraphQL `data` object.
#[derive(Deserialize)]
struct AniListData {
    /// The `Page` object.
    #[serde(default)]
    #[serde(rename = "Page")]
    page: Option<AniListPage>,
}

/// The GraphQL `Page` object.
#[derive(Deserialize)]
struct AniListPage {
    /// The matched media.
    #[serde(default)]
    media: Vec<AniListMedia>,
}

/// One matched `AniList` media entry.
#[derive(Deserialize)]
struct AniListMedia {
    /// The MAL id — entries without one are dropped (upstream filter).
    #[serde(default)]
    #[serde(rename = "idMal")]
    id_mal: Option<u64>,
    /// The media titles.
    #[serde(default)]
    title: Option<AniListTitle>,
}

/// The `AniList` title block.
#[derive(Deserialize)]
struct AniListTitle {
    /// The romaji title.
    #[serde(default)]
    romaji: Option<String>,
    /// The English title.
    #[serde(default)]
    english: Option<String>,
}

/// The deobfuscated `window.__P` payload.
#[derive(Deserialize)]
struct ZokoPayload {
    /// The m3u8 URL.
    #[serde(default)]
    src: Option<String>,
    /// The inline VTT subtitle tracks (`{lang, label, src}`) — kept as
    /// raw values so a malformed entry is skipped, not fatal (the JS
    /// filters per entry).
    #[serde(default)]
    subtitles: Option<serde_json::Value>,
}

/// A `/watch/{slug}` search result.
struct WatchLink {
    /// The result title (the most informative occurrence's text).
    title: String,
    /// The watch-page slug.
    slug: String,
}

/// The `AnimeKai` provider.
pub struct AnimeKai {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that claims the resolved m3u8 URLs.
    registry: Arc<ExtractorRegistry>,
}

impl AnimeKai {
    /// Share cached anime identity mappings with the other providers.
    #[must_use]
    pub fn with_mappings(mut self, mappings: vsources_core::mappings::MappingService) -> Self {
        self.mappings = Some(mappings);
        self
    }

    /// Build the provider over an extractor registry.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            mappings: None,
            info: SourceInfo {
                id: ID.to_string(),
                label: "AnimeKai".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Build the sub+dub streams for one MAL id, ports `buildResults`
    /// (the shared tail of both chains).
    ///
    /// `source_tag` is always empty in the upstream calls; it stays in
    /// the signature to mirror them.
    async fn build_results(
        &self,
        ctx: &ResolveCtx<'_>,
        title_base: &str,
        mal_id: u64,
        ep_num: u32,
        source_tag: &str,
    ) -> Result<Vec<Stream>, SourceError> {
        // Per-category streams and source URLs: the fake-dub check
        // below needs to know which rows — and which `src` — came from
        // which audio category.
        let mut sub_streams = Vec::new();
        let mut dub_streams = Vec::new();
        let mut srcs = [None, None];
        let mut seen: HashSet<String> = HashSet::new();
        for (index, category) in ["sub", "dub"].into_iter().enumerate() {
            let Some(data) = zoko_stream(ctx, mal_id, ep_num, category).await else {
                // Upstream skips a failed category.
                continue;
            };
            let Some(src) = data.src.filter(|src| !src.is_empty()) else {
                continue;
            };
            if !seen.insert(src.clone()) {
                continue;
            }
            let Ok(url) = Url::parse(&src) else {
                continue;
            };
            srcs[index] = Some(src);
            let subtitles = subtitle_tracks(data.subtitles.as_ref());

            // The animekai extractor claims URLs from this source and
            // attaches the zokoanime Referer.
            let extract_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: None,
                source_id: Some(ID),
                referer: Some(&url),
            };
            let Ok(extracted) = self.registry.extract(&extract_ctx, &url).await else {
                continue;
            };
            let is_dub = category == "dub";
            let target = if is_dub {
                &mut dub_streams
            } else {
                &mut sub_streams
            };
            for mut stream in extracted {
                stream.format = Format::Hls;
                stream.label = Some(format!(
                    "{title_base} (AnimeKai {}{source_tag})",
                    if is_dub { "DUB" } else { "SUB" }
                ));
                stream.ttl = TTL;
                stream.meta.languages = if is_dub {
                    vec![CountryCode::Multi, CountryCode::En]
                } else {
                    vec![CountryCode::Multi, CountryCode::Ja]
                };
                stream.meta.dubbed = Some(is_dub);
                stream.meta.subbed = Some(!is_dub);
                stream.meta.source_id = Some(ID.to_string());
                stream.meta.source_label = Some("AnimeKai".to_string());
                stream.meta.resolution = Some(1080);
                if !subtitles.is_empty() {
                    stream.meta.subtitles.clone_from(&subtitles);
                }
                target.push(stream);
            }
        }

        // zoko's `/dub` endpoint silently mirrors the sub file when a
        // dub is missing — verified live: Frieren E1 sub and dub tokens
        // resolve to byte-identical segments while One Piece E1
        // differs. A 1-byte `Content-Range` probe on the first segment
        // of each category detects the mirror: equal totals mean the
        // DUB rows would replay the sub media, so they are dropped. An
        // inconclusive probe keeps both — never lose a real dub.
        if !sub_streams.is_empty()
            && !dub_streams.is_empty()
            && let (Some(sub_src), Some(dub_src)) = (srcs[0].as_deref(), srcs[1].as_deref())
            && sub_src != dub_src
            && matches!(
                (first_segment_total(ctx, sub_src).await, first_segment_total(ctx, dub_src).await),
                (Some(sub_total), Some(dub_total)) if sub_total == dub_total
            )
        {
            dub_streams.clear();
        }
        sub_streams.extend(dub_streams);
        Ok(sub_streams)
    }

    /// The site-search fallback chain: candidate search → best watch
    /// link → watch page → MAL id. Returns `None` on every miss (the
    /// upstream answers `[]`).
    async fn site_search_mal_id(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
    ) -> Result<Option<u64>, SourceError> {
        // Progressive site search: full name, name before the colon,
        // first two words, first word.
        let candidates = search_candidates(name);
        let mut results: Vec<WatchLink> = Vec::new();
        for candidate in &candidates {
            let search_url = Url::parse_with_params(BASE.as_str(), &[("s", candidate.as_str())])
                .map_err(|error| {
                    SourceError::scrape(ID, format!("the search URL is invalid: {error}"))
                })?;
            // Upstream curls without a Referer, then retries through
            // got-scraping — one fetcher request covers both.
            let Ok(html) = fetch_text(ctx, &search_url, None).await else {
                continue;
            };
            results = parse_watch_links(&html);
            if !results.is_empty() {
                break;
            }
        }
        if results.is_empty() {
            return Ok(None);
        }

        // Pick the best match — a score below 60 is refused.
        let Some(best) = pick_best_watch_link(&results, name) else {
            return Ok(None);
        };

        // The watch page: curl first, then the got-scraping chain; if
        // the shell lacks the MAL_ID var (challenge or variant page),
        // fall back to the myanimelist.net/anime/{id}/ link every
        // watch page carries.
        let watch_url = BASE
            .join(&format!("watch/{}/", best.slug))
            .map_err(|error| {
                SourceError::scrape(ID, format!("the watch URL is invalid: {error}"))
            })?;
        let referer = referer_url("/");
        let Ok(html) = fetch_text(ctx, &watch_url, Some(&referer)).await else {
            return Ok(None);
        };
        Ok(extract_mal_id(&html))
    }
}

#[async_trait]
impl Source for AnimeKai {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        // Upstream resolves the TMDB id, name, and year here; in this
        // SDK the engine resolves media metadata before the fan-out.
        let Some(meta) = ctx.media.as_ref() else {
            return Err(SourceError::NotFound);
        };
        let name = meta.name.as_str();
        let season = if media.kind == MediaType::Series {
            media.season
        } else {
            None
        };
        let title_base = match season {
            Some(_) => format!("{name} {}", media.format_season_and_episode()),
            None => match meta.year {
                Some(year) => format!("{name} ({year})"),
                None => name.to_string(),
            },
        };
        let ep_num = season.map_or(1, |_| media.episode.unwrap_or(1));
        let season_num = season.unwrap_or(1);

        if let Some(ids) = crate::anime_mapping::ids(self.mappings.as_ref(), ctx, media).await
            && let Some(mal_id) = ids.mal_id
        {
            let direct = self
                .build_results(ctx, &title_base, mal_id, ep_num, "")
                .await?;
            if !direct.is_empty() {
                return Ok(direct);
            }
        }

        // Direct path: AniList idMal → zoko stream. Season-aware via
        // the "Nth Season" title alignment.
        let queries = if season_num > 1 {
            let ordinal = ORDINALS
                .get(usize::try_from(season_num).unwrap_or_default())
                .copied()
                .unwrap_or_default();
            let ordinal = if ordinal.is_empty() {
                format!("{season_num}th")
            } else {
                ordinal.to_string()
            };
            vec![format!("{name} {ordinal} season"), name.to_string()]
        } else {
            vec![name.to_string()]
        };
        for query in &queries {
            let entries = anilist_search_mal_ids(ctx, query).await;
            let Some(picked) = pick_mal_entry(&entries, name, season_num) else {
                continue;
            };
            let direct = self
                .build_results(ctx, &title_base, picked.mal_id, ep_num, "")
                .await?;
            if !direct.is_empty() {
                return Ok(direct);
            }
        }

        // Fallback: the animekai.at search chain.
        let Some(mal_id) = self.site_search_mal_id(ctx, name).await? else {
            return Ok(Vec::new());
        };
        self.build_results(ctx, &title_base, mal_id, ep_num, "")
            .await
    }
}

/// `BASE` joined with `path`, as the JS `Referer` header value.
fn referer_url(path: &str) -> Url {
    BASE.join(path)
        .unwrap_or_else(|_| panic!("the {path} referer must join with the base URL"))
}

/// Fetch a page as text, optionally with a Referer — the upstream
/// `curlGet`/`gotPage` pair collapsed onto the shared fetcher.
async fn fetch_text(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: Option<&Url>,
) -> Result<String, SourceError> {
    let mut request = FetchRequest::get(url.clone())
        .with_header("Accept", "text/html,*/*")
        .with_timeout(Duration::from_secs(12));
    if let Some(referer) = referer {
        request = request.with_header("Referer", referer.as_str());
    }
    let response = ctx.fetcher.request(request).await?;
    Ok(response.body)
}

/// Search `AniList` for MAL ids, ports `anilistSearchMalIds` (a failed
/// request answers `[]` — the upstream caught-exception default).
async fn anilist_search_mal_ids(ctx: &ResolveCtx<'_>, search: &str) -> Vec<MalEntry> {
    let body =
        serde_json::json!({"query": ANILIST_QUERY, "variables": {"search": search}}).to_string();
    let request = FetchRequest::post(ANILIST_GQL.clone(), body)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
    let Ok(response) = ctx.fetcher.request(request).await else {
        return Vec::new();
    };
    let Ok(data) = response.json::<AniListResponse>() else {
        return Vec::new();
    };
    data.data
        .and_then(|data| data.page)
        .map(|page| page.media)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|media| {
            let mal_id = media.id_mal?;
            let title = media.title.unwrap_or(AniListTitle {
                romaji: None,
                english: None,
            });
            Some(MalEntry {
                mal_id,
                romaji: title.romaji.unwrap_or_default(),
                english: title.english.unwrap_or_default(),
            })
        })
        .collect()
}

/// Pick the `AniList` entry whose title matches the TMDB name, ports
/// `pickMalEntry`: exact/substring scoring plus season alignment (a
/// title's `Nth season`/`Season N` suffix, defaulting to 1, must match
/// the requested season; multi-season requests penalize base entries).
fn pick_mal_entry<'a>(entries: &'a [MalEntry], name: &str, season: u32) -> Option<&'a MalEntry> {
    let name_norm = norm(name);
    let mut best: Option<(&MalEntry, f64)> = None;
    for entry in entries {
        for title in [&entry.romaji, &entry.english] {
            let title_norm = norm(title);
            if title_norm.is_empty() {
                continue;
            }
            let mut score = 0.0;
            if title_norm == name_norm {
                score = 100.0;
            } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
                let short = title_norm.len().min(name_norm.len());
                let long = title_norm.len().max(name_norm.len()).max(1);
                score = f64::from(u32::try_from(short).unwrap_or_default())
                    / f64::from(u32::try_from(long).unwrap_or(1))
                    * 90.0;
            }
            if score == 0.0 {
                continue;
            }
            let entry_season = entry_season(&title_norm);
            if entry_season == season {
                score += 25.0;
            } else if season > 1 {
                // Prefer suffix-matching entries.
                score -= 20.0;
            }
            if best.is_none_or(|(_, best_score)| score > best_score) {
                best = Some((entry, score));
            }
        }
    }
    let (entry, score) = best?;
    (score >= MIN_MATCH_SCORE).then_some(entry)
}

/// The season a normalized `AniList` title refers to:
/// `(\d+)(?:nd|rd|th|st) season` or `season (\d+)`, else 1 (the base
/// entry).
fn entry_season(title_norm: &str) -> u32 {
    let mut from = 0;
    while let Some(found) = title_norm[from..].find(" season") {
        let pos = from + found;
        let before = &title_norm[..pos];
        for suffix in ["nd", "rd", "th", "st"] {
            if let Some(stripped) = before.strip_suffix(suffix) {
                let digits = trailing_digits(stripped);
                if !digits.is_empty() {
                    return digits.parse().unwrap_or(1);
                }
            }
        }
        from = pos + " season".len();
    }
    let mut from = 0;
    while let Some(found) = title_norm[from..].find("season ") {
        let pos = from + found + "season ".len();
        let digits = leading_digits(&title_norm[pos..]);
        if !digits.is_empty() {
            return digits.parse().unwrap_or(1);
        }
        from = pos;
    }
    1
}

/// Normalize for fuzzy matching, ports the upstream `norm`/`normalize`:
/// apostrophes are dropped BEFORE tokenizing so "Journey's" ==
/// "journeys" (TMDB title text vs site slug artifact otherwise never
/// converge).
fn norm(s: &str) -> String {
    let lower = s.to_lowercase();
    let no_apostrophes: String = lower
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .collect();
    no_apostrophes
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The substring-ratio score.
fn ratio(short: usize, long: usize) -> f64 {
    f64::from(u32::try_from(short).unwrap_or_default())
        / f64::from(u32::try_from(long).unwrap_or_default())
}

/// Pick the best watch link (≥ 60), the site-search scorer.
fn pick_best_watch_link<'a>(results: &'a [WatchLink], name: &str) -> Option<&'a WatchLink> {
    let name_norm = norm(name);
    let mut best: Option<(&WatchLink, f64)> = None;
    for result in results {
        let title_norm = norm(&result.title);
        if title_norm.is_empty() {
            continue;
        }
        let mut score = 0.0;
        if title_norm == name_norm {
            score = 100.0;
        } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
            score = ratio(
                title_norm.len().min(name_norm.len()),
                title_norm.len().max(name_norm.len()).max(1),
            ) * 90.0;
        }
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((result, score));
        }
    }
    let (result, score) = best?;
    (score >= MIN_MATCH_SCORE).then_some(result)
}

/// The search candidate ladder: the name, the name before a
/// `:`/`–`/`—`, the first two words, then the first word — each only
/// once it is at least 3 chars.
fn search_candidates(name: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim();
        if value.chars().count() >= 3 && !candidates.iter().any(|seen| seen == value) {
            candidates.push(value.to_string());
        }
    };
    push(name);
    let colon = name.char_indices().find(|(i, c)| {
        matches!(*c, ':' | '–' | '—')
            && name[i + c.len_utf8()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
    });
    if let Some((index, _)) = colon.filter(|(index, _)| *index > 0) {
        push(&name[..index]);
    }
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() > 2 {
        push(&words[..2].join(" "));
    }
    if words.len() > 1 {
        push(words[0]);
    }
    candidates
}

/// Parse `/watch/{slug}` links off a search page, ports
/// `parseWatchLinks`: poster-card anchors have empty text, so a later
/// anchor carrying the real title wins (the most informative
/// occurrence per slug, in first-seen order).
fn parse_watch_links(html: &str) -> Vec<WatchLink> {
    let doc = Html::parse_document(html);
    let mut results: Vec<WatchLink> = Vec::new();
    for link in doc.select(&ALL_LINKS) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(slug) = watch_slug(href) else {
            continue;
        };
        let text = link.text().collect::<String>().trim().to_string();
        if let Some(result) = results.iter_mut().find(|result| result.slug == slug) {
            if text.len() > result.title.len() {
                result.title = text;
            }
        } else {
            let title = if text.is_empty() {
                slug.replace('-', " ")
            } else {
                text
            };
            results.push(WatchLink {
                title,
                slug: slug.to_string(),
            });
        }
    }
    results
}

/// The `{slug}` of a href ending in `/watch/{slug}` (optional trailing
/// slash), ports `href.match(/\/watch\/([^/]+)\/?$/)` — anchored to
/// the end, so deeper paths never match.
fn watch_slug(href: &str) -> Option<&str> {
    let pos = href.rfind("/watch/")? + "/watch/".len();
    let rest = href[pos..].strip_suffix('/').unwrap_or(&href[pos..]);
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest)
}

/// Extract the MAL id from a watch page: the `MAL_ID = "…"` shell
/// variable, falling back to the `myanimelist.net/anime/{id}/` link
/// every watch page carries.
fn extract_mal_id(html: &str) -> Option<u64> {
    // `MAL_ID\s*=\s*["']?(\d+)["']?`
    if let Some(pos) = html.find("MAL_ID") {
        let after = html[pos + "MAL_ID".len()..].trim_start();
        if let Some(after) = after.strip_prefix('=') {
            let after = after.trim_start();
            let after = after
                .strip_prefix('"')
                .or_else(|| after.strip_prefix('\''))
                .unwrap_or(after);
            let digits = leading_digits(after);
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    // `myanimelist\.net\/anime\/(\d+)\//`
    let pos = html.find("myanimelist.net/anime/")? + "myanimelist.net/anime/".len();
    let digits = leading_digits(&html[pos..]);
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Fetch a zoko stream payload for one audio category, ports
/// `zokoStream` (the transport ladder collapses onto the shared
/// fetcher). Returns `None` when the page or the payload is missing —
/// the upstream null.
async fn zoko_stream(
    ctx: &ResolveCtx<'_>,
    mal_id: u64,
    episode: u32,
    category: &str,
) -> Option<ZokoPayload> {
    let url = Url::parse(&format!(
        "https://zokoanime.video/stream/mal/{mal_id}/{episode}/{category}"
    ))
    .ok()?;
    let referer = referer_url("/");
    let request = FetchRequest::get(url)
        .with_header("Accept", "text/html,*/*")
        .with_header("Referer", referer.as_str())
        .with_timeout(Duration::from_secs(12));
    let response = ctx.fetcher.request(request).await.ok()?;
    let payload = window_p_payload(&response.body)?;
    deobfuscate(payload)
}

/// The total size of the first media segment behind one zoko HLS
/// source: master playlist → top variant → child playlist → first
/// segment → a 1-byte `Content-Range` request. `None` on any miss.
///
/// zoko's `/dub` endpoint silently mirrors the sub file when a dub is
/// missing, and equal first-segment totals expose the mirror for the
/// cost of one byte per category (the tokens differ, so the URLs
/// cannot be compared directly).
async fn first_segment_total(ctx: &ResolveCtx<'_>, src: &str) -> Option<u64> {
    let master_url = Url::parse(src).ok()?;
    let master = zoko_text(ctx, master_url.clone()).await?;
    let variant = master
        .lines()
        .rfind(|line| !line.is_empty() && !line.starts_with('#'))?;
    let child_url = master_url.join(variant).ok()?;
    let child = zoko_text(ctx, child_url.clone()).await?;
    let segment = child
        .lines()
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    let segment_url = child_url.join(segment).ok()?;
    let request = FetchRequest::get(segment_url)
        .with_header("Accept", "*/*")
        .with_header("Referer", referer_url("/").as_str())
        .with_header("Range", "bytes=0-0")
        .with_timeout(Duration::from_secs(8));
    let response = ctx.fetcher.request(request).await.ok()?;
    let total = response.header("content-range")?.rsplit('/').next()?;
    total.parse::<u64>().ok()
}

/// One small text GET with the zokoanime Referer — the playlist walk of
/// [`first_segment_total`].
async fn zoko_text(ctx: &ResolveCtx<'_>, url: Url) -> Option<String> {
    let request = FetchRequest::get(url)
        .with_header("Accept", "*/*")
        .with_header("Referer", referer_url("/").as_str())
        .with_timeout(Duration::from_secs(8));
    let response = ctx.fetcher.request(request).await.ok()?;
    response.is_success().then_some(response.body)
}

/// The `window.__P="…"` payload — upstream
/// `html.match(/window\.__P="([^"]+)"/)`.
fn window_p_payload(html: &str) -> Option<&str> {
    const MARKER: &str = "window.__P=\"";
    let start = html.find(MARKER)? + MARKER.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Deobfuscate the `window.__P` payload: base64 → XOR `OBF_KEY` →
/// JSON. The JS pads the base64 first; the lenient decoder does not
/// need the padding. A parse failure is the upstream caught exception.
fn deobfuscate(payload: &str) -> Option<ZokoPayload> {
    let raw = decode_base64_lenient(payload);
    let key = OBF_KEY.as_bytes();
    let out: Vec<u8> = raw
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ key[i % key.len()])
        .collect();
    serde_json::from_str(&String::from_utf8_lossy(&out)).ok()
}

/// The inline subtitle tracks of a zoko payload, ports the upstream
/// mapping: `lang` (falling back to `label`, then `en`) capped at 8
/// chars, entries with a non-string or invalid `src` dropped, the
/// synthetic JS ids cut.
fn subtitle_tracks(subtitles: Option<&serde_json::Value>) -> Vec<SubtitleTrack> {
    let Some(array) = subtitles.and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|subtitle| {
            let src = subtitle.get("src").and_then(serde_json::Value::as_str)?;
            let url = Url::parse(src).ok()?;
            let lang = subtitle
                .get("lang")
                .and_then(serde_json::Value::as_str)
                .filter(|lang| !lang.is_empty())
                .or_else(|| subtitle.get("label").and_then(serde_json::Value::as_str))
                .unwrap_or("en");
            let lang: String = lang.chars().take(8).collect();
            Some(SubtitleTrack {
                label: None,
                language: Some(lang),
                url,
            })
        })
        .collect()
}

/// The leading ASCII digit run of `s`.
fn leading_digits(s: &str) -> &str {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    &s[..end]
}

/// The trailing ASCII digit run of `s`.
fn trailing_digits(s: &str) -> &str {
    let start = s
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |pos| pos + 1);
    &s[start..]
}

/// Lenient standard base64 decode, mirroring `Buffer.from(x, 'base64')`:
/// URL-safe and standard alphabets both decode, padding and whitespace
/// are ignored, and invalid characters are discarded rather than fatal.
fn decode_base64_lenient(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in input.as_bytes() {
        if b"= \n\r\t".contains(&byte) {
            // Padding and incidental whitespace are ignored.
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            // Invalid characters are discarded, like Buffer.from.
            _ => continue,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).unwrap_or_default());
        }
    }
    out
}

#[cfg(test)]
mod tests {

    use crate::testing::ScriptedFetcher;
    use vsources_core::traits::ResolvedMedia;
    use vsources_core::types::MediaId;
    use vsources_extractors::hosts::animekai::AnimeKai as AnimeKaiExtractor;

    use super::*;

    /// Match a host and path (the query is ignored).
    fn at(host: &'static str, path: &'static str) -> impl Fn(&Url) -> bool {
        move |url| url.host_str() == Some(host) && url.path() == path
    }

    /// A resolve context over the scripted fetcher.
    fn ctx(fetcher: &ScriptedFetcher, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media,
            source_id: Some(ID),
            referer: None,
        }
    }

    /// A series episode reference with resolved TMDB metadata.
    fn one_piece(season: u32, episode: u32) -> (MediaRef, ResolvedMedia) {
        (
            MediaRef {
                id: MediaId::Tmdb(37854),
                kind: MediaType::Series,
                season: Some(season),
                episode: Some(episode),
            },
            ResolvedMedia {
                tmdb_id: Some(37854),
                imdb_id: None,
                name: "One Piece".to_string(),
                year: Some(1999),
                season: Some(season),
                episode: Some(episode),
            },
        )
    }

    /// Standard base64 encode (fixture builder).
    fn b64(data: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let byte = |i: usize| u32::from(*chunk.get(i).unwrap_or(&0));
            let triple = (byte(0) << 16) | (byte(1) << 8) | byte(2);
            out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(triple & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    /// Build a `window.__P` page around the XOR+base64 obfuscated
    /// `payload` JSON.
    fn stream_page(payload: &str) -> String {
        let key = OBF_KEY.as_bytes();
        let xored: Vec<u8> = payload
            .bytes()
            .enumerate()
            .map(|(i, byte)| byte ^ key[i % key.len()])
            .collect();
        format!(
            r#"<html><script>window.__P="{}";</script></html>"#,
            b64(&xored)
        )
    }

    /// A provider wired to a registry with the animekai passthrough
    /// extractor (it attaches the zokoanime Referer).
    fn provider() -> AnimeKai {
        AnimeKai::new(Arc::new(ExtractorRegistry::new(vec![Arc::new(
            AnimeKaiExtractor::new(),
        )])))
    }

    /// `AniList` results: a season-suffixed entry and a weaker match.
    const ANILIST_JSON: &str = r#"{"data":{"Page":{"media":[
        {"idMal":22,"startDate":{"year":2017},"title":{"romaji":"One Piece 2nd Season","english":"One Piece 2nd Season"}},
        {"idMal":100,"startDate":{"year":2002},"title":{"romaji":"One Piece Film: Z","english":"One Piece Film Z"}}
    ]}}}"#;

    const SITE_SEARCH_PAGE: &str = r#"<html><body>
        <a href="https://animekai.at/watch/one-piece/"><img alt=""/></a>
        <a href="/watch/one-piece/">One Piece</a>
        <a href="/watch/unrelated-show/">Unrelated Show</a>
    </body></html>"#;

    const WATCH_PAGE: &str = r#"<html><script>var MAL_ID = "21";</script></html>"#;

    #[test]
    fn parses_season_suffixes() {
        assert_eq!(entry_season("one piece 2nd season"), 2);
        assert_eq!(entry_season("one piece season 3"), 3);
        assert_eq!(entry_season("one piece 4th season anything"), 4);
        // The base entry maps to season 1.
        assert_eq!(entry_season("one piece"), 1);
    }

    #[test]
    fn norm_drops_apostrophes_before_tokenizing() {
        assert_eq!(norm("Journey’s End"), "journeys end");
        assert_eq!(norm("Journey's End"), "journeys end");
        assert_eq!(norm("One-Piece!"), "one piece");
    }

    #[test]
    fn extracts_mal_ids_from_both_shapes() {
        assert_eq!(
            extract_mal_id(r#"<script>MAL_ID = "21";</script>"#),
            Some(21)
        );
        assert_eq!(extract_mal_id("<script>MAL_ID = 21;</script>"), Some(21));
        assert_eq!(
            extract_mal_id(r#"<a href="https://myanimelist.net/anime/21/one_piece">MAL</a>"#),
            Some(21)
        );
        assert_eq!(extract_mal_id("<html>no ids</html>"), None);
    }

    #[test]
    fn watch_slugs_only_match_at_the_end() {
        assert_eq!(watch_slug("/watch/one-piece"), Some("one-piece"));
        assert_eq!(
            watch_slug("https://animekai.at/watch/one-piece/"),
            Some("one-piece")
        );
        assert_eq!(watch_slug("/watch/one-piece/extra"), None);
        assert_eq!(watch_slug("/browse/one-piece"), None);
    }

    #[test]
    fn picks_the_season_aligned_anilist_entry() {
        let entries = vec![
            MalEntry {
                mal_id: 22,
                romaji: "One Piece 2nd Season".to_string(),
                english: String::new(),
            },
            MalEntry {
                mal_id: 21,
                romaji: "One Piece".to_string(),
                english: String::new(),
            },
        ];
        // Season 2, verbatim arithmetic: the exact base match
        // (100−20=80) still beats the aligned suffix entry
        // (40.5+25=65.5) — the base entry wins, like upstream.
        let picked = pick_mal_entry(&entries, "One Piece", 2)
            .unwrap_or_else(|| panic!("a season entry must match"));
        assert_eq!(picked.mal_id, 21);
        // Season 1: the base entry aligns (100+25).
        let picked = pick_mal_entry(&entries, "One Piece", 1)
            .unwrap_or_else(|| panic!("the base entry must match"));
        assert_eq!(picked.mal_id, 21);

        // The suffix preference only wins when the base entry is not
        // an exact title match: 40.5+25 (aligned) vs 50.6−20.
        let entries = vec![
            MalEntry {
                mal_id: 22,
                romaji: "One Piece 2nd Season".to_string(),
                english: String::new(),
            },
            MalEntry {
                mal_id: 99,
                romaji: "One Piece Film Z".to_string(),
                english: String::new(),
            },
        ];
        let picked = pick_mal_entry(&entries, "One Piece", 2)
            .unwrap_or_else(|| panic!("the aligned entry must match"));
        assert_eq!(picked.mal_id, 22);
    }
    #[tokio::test]
    async fn direct_path_uses_the_anilist_mal_id_for_season_two() -> Result<(), SourceError> {
        let sub_payload = r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/master.m3u8","subtitles":[{"lang":"en","label":"English","src":"https://zokoanime.video/subs/en.vtt"}]}"#;
        let dub_payload =
            r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/dub/master.m3u8","subtitles":[]}"#;

        let fetcher = ScriptedFetcher::new()
            .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
            .page(
                at("zokoanime.video", "/stream/mal/22/5/sub"),
                stream_page(sub_payload),
            )
            .page(
                at("zokoanime.video", "/stream/mal/22/5/dub"),
                stream_page(dub_payload),
            );

        let (media, meta) = one_piece(2, 5);
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        // The season-aware AniList query was searched.
        let anilist_body = fetcher
            .requests()
            .iter()
            .find(|request| request.url.host_str() == Some("graphql.anilist.co"))
            .and_then(|request| request.body.as_ref())
            .cloned()
            .unwrap_or_default();
        assert!(
            anilist_body.contains("One Piece 2nd season"),
            "the season-aware query must be searched, got {anilist_body}"
        );

        assert_eq!(streams.len(), 2, "sub + dub");
        let sub = &streams[0];
        assert_eq!(
            sub.label.as_deref(),
            Some("One Piece S02E05 (AnimeKai SUB)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            sub.url.as_str(),
            "https://hls2.aniwatchtv.uk/v/abc/master.m3u8"
        );
        // The zoko payload's inline VTT subtitle track rides along.
        assert_eq!(sub.meta.subtitles.len(), 1);
        assert_eq!(sub.meta.subtitles[0].language.as_deref(), Some("en"));
        assert_eq!(
            sub.meta.subtitles[0].url.as_str(),
            "https://zokoanime.video/subs/en.vtt"
        );
        let dub = &streams[1];
        assert_eq!(dub.meta.dubbed, Some(true));
        assert_eq!(dub.meta.subbed, Some(false));
        assert_eq!(
            dub.label.as_deref(),
            Some("One Piece S02E05 (AnimeKai DUB)")
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        for stream in &streams {
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.ttl, TTL);
            assert_eq!(stream.meta.resolution, Some(1080));
            assert_eq!(stream.meta.source_id.as_deref(), Some(ID));
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://zokoanime.video/")
            );
        }
        // The zoko page was fetched with the animekai referer.
        assert_eq!(
            fetcher
                .header_sent_to("/stream/mal/22/5/sub", "Referer")
                .as_deref(),
            Some("https://animekai.at/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn arm_season_two_id_beats_the_base_title_without_graphql() -> Result<(), SourceError> {
        let fetcher = Arc::new(
            ScriptedFetcher::new()
                .page(
                    at("arm.haglund.dev", "/api/v2/themoviedb"),
                    r#"[
                {"anilist":11,"myanimelist":21,"themoviedb-season":1},
                {"anilist":12,"myanimelist":22,"themoviedb-season":2}
            ]"#,
                )
                .page(
                    at("zokoanime.video", "/stream/mal/22/5/sub"),
                    stream_page(r#"{"src":"https://cdn.example/season2.m3u8","subtitles":[]}"#),
                ),
        );
        let (media, meta) = one_piece(2, 5);
        let mappings = vsources_core::mappings::MappingService::new(fetcher.clone());
        let streams = provider()
            .with_mappings(mappings)
            .resolve(&ctx(&fetcher, Some(meta)), &media)
            .await?;
        assert_eq!(streams.len(), 1);
        assert!(
            fetcher
                .requests()
                .iter()
                .any(|r| r.url.path() == "/stream/mal/22/5/sub")
        );
        assert!(
            fetcher
                .requests()
                .iter()
                .all(|r| r.url.host_str() != Some("graphql.anilist.co"))
        );
        assert!(
            fetcher
                .requests()
                .iter()
                .all(|r| !r.url.path().contains("/mal/21/"))
        );
        Ok(())
    }

    /// The playlist walk and Range endpoints for one zoko source, with
    /// a first-segment `Content-Range` total of `total`. The three
    /// paths are the source's master, child, and first segment URLs.
    fn zoko_mirror_walk(
        fetcher: ScriptedFetcher,
        master_path: &'static str,
        child_path: &'static str,
        segment_path: &'static str,
        total: u64,
    ) -> ScriptedFetcher {
        let host = "hls2.aniwatchtv.uk";
        fetcher
            .page(
                at(host, master_path),
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5300000,RESOLUTION=1920x1080\n1080/index.m3u8\n",
            )
            .page(
                at(host, child_path),
                "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:14.56,\nseg_00000.ts\n",
            )
            .ranged(at(host, segment_path), total)
    }

    #[tokio::test]
    async fn mirrored_dub_rows_are_dropped() -> Result<(), SourceError> {
        // zoko's /dub endpoint mirrors the sub file when a dub is
        // missing — equal first-segment totals (Frieren E1, verified
        // live) mean the DUB rows would replay the sub media.
        let sub_payload =
            r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/sub/master.m3u8","subtitles":[]}"#;
        let mirror_payload =
            r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/dub/master.m3u8","subtitles":[]}"#;

        let fetcher = ScriptedFetcher::new()
            .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
            .page(
                at("zokoanime.video", "/stream/mal/22/5/sub"),
                stream_page(sub_payload),
            )
            .page(
                at("zokoanime.video", "/stream/mal/22/5/dub"),
                stream_page(mirror_payload),
            );
        // Both categories walk to the same first-segment total.
        let fetcher = zoko_mirror_walk(
            fetcher,
            "/v/abc/sub/master.m3u8",
            "/v/abc/sub/1080/index.m3u8",
            "/v/abc/sub/1080/seg_00000.ts",
            3_807_752,
        );
        let fetcher = zoko_mirror_walk(
            fetcher,
            "/v/abc/dub/master.m3u8",
            "/v/abc/dub/1080/index.m3u8",
            "/v/abc/dub/1080/seg_00000.ts",
            3_807_752,
        );

        let (media, meta) = one_piece(2, 5);
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert_eq!(streams.len(), 1, "the mirrored DUB row must be dropped");
        assert_eq!(
            streams[0].label.as_deref(),
            Some("One Piece S02E05 (AnimeKai SUB)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn real_dub_rows_are_kept_and_unknown_probes_keep_both() -> Result<(), SourceError> {
        // Different first-segment totals (One Piece E1, verified live)
        // are a genuine dub — both rows ship. An unreachable probe (no
        // playlist pages) is inconclusive and also keeps both.
        let sub_payload =
            r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/sub/master.m3u8","subtitles":[]}"#;
        let dub_payload =
            r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/dub/master.m3u8","subtitles":[]}"#;

        let build = || {
            ScriptedFetcher::new()
                .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
                .page(
                    at("zokoanime.video", "/stream/mal/22/5/sub"),
                    stream_page(sub_payload),
                )
                .page(
                    at("zokoanime.video", "/stream/mal/22/5/dub"),
                    stream_page(dub_payload),
                )
        };

        // Real dub: the first-segment totals differ.
        let fetcher = zoko_mirror_walk(
            zoko_mirror_walk(
                build(),
                "/v/abc/sub/master.m3u8",
                "/v/abc/sub/1080/index.m3u8",
                "/v/abc/sub/1080/seg_00000.ts",
                1_161_464,
            ),
            "/v/abc/dub/master.m3u8",
            "/v/abc/dub/1080/index.m3u8",
            "/v/abc/dub/1080/seg_00000.ts",
            1_155_448,
        );
        let (media, meta) = one_piece(2, 5);
        let first = ctx(&fetcher, Some(meta.clone()));
        let streams = provider().resolve(&first, &media).await?;
        assert_eq!(streams.len(), 2, "sub + a genuine dub");

        // Inconclusive: the playlist walk 404s for both categories.
        let fetcher = build();
        let second = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&second, &media).await?;
        assert_eq!(streams.len(), 2, "an unknown probe keeps both rows");
        Ok(())
    }

    #[tokio::test]
    async fn site_search_path_falls_back_when_anilist_is_down() -> Result<(), SourceError> {
        // No AniList fixture: the direct path finds nothing and the
        // animekai.at search chain takes over.
        let sub_payload = r#"{"src":"https://hls2.aniwatchtv.uk/v/op/master.m3u8"}"#;
        let dub_payload = r#"{"src":"https://hls2.aniwatchtv.uk/v/op/dub/master.m3u8"}"#;

        let fetcher = ScriptedFetcher::new()
            .page(at("animekai.at", "/"), SITE_SEARCH_PAGE)
            .page(at("animekai.at", "/watch/one-piece/"), WATCH_PAGE)
            .page(
                at("zokoanime.video", "/stream/mal/21/5/sub"),
                stream_page(sub_payload),
            )
            .page(
                at("zokoanime.video", "/stream/mal/21/5/dub"),
                stream_page(dub_payload),
            );

        let (media, meta) = one_piece(1, 5);
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        assert_eq!(streams.len(), 2, "sub + dub");
        assert_eq!(
            streams[0].label.as_deref(),
            Some("One Piece S01E05 (AnimeKai SUB)")
        );
        assert_eq!(
            streams[1].label.as_deref(),
            Some("One Piece S01E05 (AnimeKai DUB)")
        );
        assert!(streams.iter().all(|stream| stream.format == Format::Hls));
        // The watch-page referer traveled on the zoko fetch.
        assert_eq!(
            fetcher
                .header_sent_to("/stream/mal/21/5/sub", "Referer")
                .as_deref(),
            Some("https://animekai.at/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_mal_id_anywhere_is_an_empty_answer() -> Result<(), SourceError> {
        // AniList is down and the search page has no watch links.
        let fetcher = ScriptedFetcher::new()
            .page(at("animekai.at", "/"), "<html><body>nothing</body></html>");

        let (media, meta) = one_piece(1, 5);
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(streams.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_media_metadata_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let (media, _) = one_piece(1, 5);
        let ctx = ctx(&fetcher, None);
        match provider().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("without a title there is nothing to search: {other:?}"),
        }
    }
}
