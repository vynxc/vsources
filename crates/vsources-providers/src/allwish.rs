//! `AllWish`: anime embeds from the Laravel AJAX backend of `all-wish.me`.
//!
//! Ports `src/source/AllWish.js` — an AniSuge/HiAnime-family clone whose
//! flow is plain HTML + JSON (no VRF/token validation):
//!
//! 1. search `GET /filter?keyword={title}` → cards linking
//!    `/watch/{slug}-{5charId}/ep-N`;
//! 2. detail `GET /watch/{slug}-{5charId}` → the numeric anime id
//!    (`.favourite[data-id]`, falling back to the first `[data-id]`);
//! 3. episodes `GET /ajax/episode/list/{animeId}`
//!    (`X-Requested-With: XMLHttpRequest`) →
//!    `{status: 200, result: "<html>"}` with
//!    `<a data-ids=… data-slug=… data-sub=… data-dub=…>`;
//! 4. servers `GET /ajax/server/list?servers={data-ids}` — the ids are
//!    sent **raw** (not URL-encoded), exactly like the site's own jQuery
//!    — → `.server-type[data-type=sub|dub] .server[data-link-id]`;
//! 5. embed `GET /ajax/server?get={data-link-id}` →
//!    `{result: {url: "https://megaplay.buzz/stream/s-1/{token}"}}`.
//!
//! Both SUB and DUB servers are returned when the episode flags them.
//! The embed URLs are megaplay.buzz links: upstream's resolver fed them
//! to the extractor registry after the source returned; here the
//! provider folds that step in by resolving each embed through the
//! shared [`ExtractorRegistry`] and merging its own metadata into the
//! extracted streams.
//!
//! Mappings and cuts (vs. upstream):
//! - `meta.title` (`{title} (Sub · {server})`) → [`Stream::label`]; the
//!   producing extractor stays attributed in `meta.extractor_label`.
//! - `meta.countryCodes` → `meta.languages`.
//! - No result cache here — the parent's `CachedSource` wrapper owns
//!   result/negative caching (upstream `Source.handle` with the default
//!   12h `this.ttl`); streams coming from the registry keep the
//!   extractor's ttl.
//! - `getTmdbId`/`getTmdbNameAndYear` → `ctx.media`: the provider needs
//!   a title to search; without pre-resolved media it answers
//!   [`SourceError::NotFound`] (upstream always resolved TMDB first, so
//!   the case never arose there).
//! - The explicit Chrome `User-Agent`/`Accept-Language`/HTML `Accept`
//!   are dropped — the fetcher layer already sends browser-like headers
//!   and solves Cloudflare challenges.
//! - No server `/proxy` routing: `requestHeaders` travel on
//!   `meta.request_headers` (this source sets none upstream).
//! - Extractor failures are skipped per embed — upstream
//!   `extractorRegistry.handle(...).catch(() => [])`.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; `normalize` still strips combining marks (U+0300–036F)
//!   and precomposed accents simply drop out — fine for romanized anime
//!   titles.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The site root, upstream `BASE_URL`.
const BASE_URL: &str = "https://all-wish.me";
/// This provider's id, for scrape diagnostics.
const PROVIDER_ID: &str = "allwish";
/// Upstream `timeout: { request: 15000 }`.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Search result cards (`div.item`).
static SEARCH_CARD: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div.item").unwrap_or_else(|e| panic!("valid card selector: {e}"))
});
/// The card poster link (`a.poster`).
static POSTER_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.poster").unwrap_or_else(|e| panic!("valid poster selector: {e}"))
});
/// The card title link (`.name a`).
static NAME_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".name a").unwrap_or_else(|e| panic!("valid name selector: {e}"))
});
/// The detail page's id anchor (`.favourite[data-id]`).
static FAVOURITE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".favourite[data-id]")
        .unwrap_or_else(|e| panic!("valid favourite selector: {e}"))
});
/// Any `[data-id]` element on the detail page.
static ANY_DATA_ID: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("[data-id]").unwrap_or_else(|e| panic!("valid data-id selector: {e}"))
});
/// Episode anchors in the AJAX episode-list HTML.
static EPISODE_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a[data-ids]").unwrap_or_else(|e| panic!("valid episode selector: {e}"))
});
/// Server groups (`.server-type, .type` — the site has used both).
static SERVER_GROUPS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".server-type, .type")
        .unwrap_or_else(|e| panic!("valid server-group selector: {e}"))
});
/// Server entries within a group (`.server, li`).
static SERVER_ITEMS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".server, li").unwrap_or_else(|e| panic!("valid server selector: {e}"))
});

/// The `all-wish.me` provider (HiAnime-family AJAX scraping).
pub struct AllWish {
    /// Static descriptor.
    info: SourceInfo,
    /// The site root (upstream `BASE_URL`).
    base: Url,
    /// The embed resolver — upstream's post-source extraction stage.
    registry: Arc<ExtractorRegistry>,
}

impl AllWish {
    /// A provider resolving megaplay embeds through `registry`.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: PROVIDER_ID.to_string(),
                label: "AllWish".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(parse_url(BASE_URL)),
                priority: 0,
                // Upstream leaves `this.domainKey` unset.
                domain_key: None,
            },
            base: parse_url(BASE_URL),
            registry,
        }
    }

    /// Step 1: search by name and return the best-matching watch page.
    ///
    /// Tries the upstream query variants in order; the first page whose
    /// best card scores ≥ 60 wins.
    async fn find_watch_page(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
    ) -> Result<Option<Url>, SourceError> {
        for query in query_variants(name) {
            let search = format!("{BASE_URL}/filter?keyword={}", encode_component(&query));
            let Some(html) = fetch_html(ctx, &site_url(&search)?, self.base.as_str()).await? else {
                continue;
            };
            if let Some(href) = best_card_href(&html, name) {
                // Relative card hrefs resolve against the site root.
                let watch = self.base.join(&href).map_err(|e| {
                    SourceError::scrape(PROVIDER_ID, format!("unparsable watch URL {href:?}: {e}"))
                })?;
                return Ok(Some(watch));
            }
        }
        Ok(None)
    }

    /// Step 7: resolve a server's link id to its embed URL via
    /// `/ajax/server?get={linkId}`.
    async fn resolve_embed(
        &self,
        ctx: &ResolveCtx<'_>,
        link_id: &str,
        referer: &str,
    ) -> Result<Option<Url>, SourceError> {
        let url = format!("{BASE_URL}/ajax/server?get={}", encode_component(link_id));
        let Some(data) = fetch_json(ctx, &site_url(&url)?, referer).await? else {
            return Ok(None);
        };
        // JS: `data.status === 200 && data.result?.url`.
        let Some(link) = data.pointer("/result/url").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(site_url(link)?))
    }
}

#[async_trait]
impl Source for AllWish {
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

        // Step 1: search for the anime.
        let Some(watch) = self.find_watch_page(ctx, &resolved.name).await? else {
            return Err(SourceError::NotFound);
        };

        // Step 2: detail page → numeric anime id.
        let Some(detail_html) = fetch_html(ctx, &watch, self.base.as_str()).await? else {
            return Err(SourceError::NotFound);
        };
        let Some(anime_id) = anime_id_from_detail(&detail_html) else {
            return Err(SourceError::NotFound);
        };

        // Step 3: episode list via AJAX (no vrf — all-wish doesn't
        // validate it).
        let episode_list = format!("{BASE_URL}/ajax/episode/list/{anime_id}");
        let Some(payload) = fetch_json(ctx, &site_url(&episode_list)?, watch.as_str()).await?
        else {
            return Err(SourceError::NotFound);
        };
        if payload.get("status").and_then(Value::as_i64) != Some(200) {
            return Err(SourceError::NotFound);
        }
        let Some(result_html) = payload.get("result").and_then(Value::as_str) else {
            return Err(SourceError::NotFound);
        };

        // Step 4: the target episode's data-ids + sub/dub flags.
        let Some(episode) = find_episode(result_html, target) else {
            return Err(SourceError::NotFound);
        };

        // Step 5: server list — data-ids sent RAW (not URL-encoded); the
        // site's own jQuery does the same.
        let server_list = format!("{BASE_URL}/ajax/server/list?servers={}", episode.ids);
        let ep_referer = format!("{watch}/ep-{target}");
        let Some(payload) = fetch_json(ctx, &site_url(&server_list)?, &ep_referer).await? else {
            return Err(SourceError::NotFound);
        };
        if payload.get("status").and_then(Value::as_i64) != Some(200) {
            return Err(SourceError::NotFound);
        }
        let Some(result_html) = payload.get("result").and_then(Value::as_str) else {
            return Err(SourceError::NotFound);
        };

        // Step 6: parse and dedupe the server groups.
        let servers = parse_servers(result_html);
        if servers.is_empty() {
            return Err(SourceError::NotFound);
        }
        let deduped = deduped_servers(&servers, &episode);
        let ep_page = site_url(&ep_referer)?;

        // Step 7: resolve embed URLs, prefer sub then dub, dedupe by
        // embed URL and label; extract each through the registry.
        let mut streams = Vec::new();
        let mut seen_urls = HashSet::new();
        let mut seen_labels = HashSet::new();
        for server in deduped {
            let Some(embed) = self
                .resolve_embed(ctx, &server.link_id, &ep_referer)
                .await?
            else {
                continue;
            };
            if !seen_urls.insert(embed.as_str().to_string()) {
                continue;
            }

            let audio = if server.server_type == "dub" {
                "Dub"
            } else {
                "Sub"
            };
            let languages = if server.server_type == "dub" {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            };
            let label = format!("{title} ({audio} · {})", server.name);
            let label_key = format!("{audio}_{}", server.name);
            if !seen_labels.insert(label_key) {
                continue;
            }

            // Upstream: the resolver extracts every embed and merges the
            // source meta into the extractor's streams; extraction
            // errors are swallowed per URL.
            let embed_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: ctx.media.clone(),
                source_id: Some(self.info.id.as_str()),
                referer: Some(&ep_page),
            };
            let Ok(extracted) = self.registry.extract(&embed_ctx, &embed).await else {
                continue;
            };
            for mut stream in extracted {
                stream.meta.source_id = Some(self.info.id.clone());
                stream.meta.source_label = Some(self.info.label.clone());
                stream.meta.languages.clone_from(&languages);
                stream.label = Some(label.clone());
                streams.push(stream);
            }
        }
        Ok(streams)
    }
}

/// One server entry: language group plus its AJAX link id.
struct ServerEntry {
    /// The group's `data-type` — `sub` or `dub`.
    server_type: String,
    /// The server's display name (the link text).
    name: String,
    /// `data-link-id` for `/ajax/server?get=`.
    link_id: String,
}

/// The target episode parsed from the AJAX episode-list HTML.
struct EpisodeLink {
    /// `data-ids` — the raw server-id list.
    ids: String,
    /// `data-sub` flag.
    has_sub: bool,
    /// `data-dub` flag.
    has_dub: bool,
}

/// The best-matching card href (score ≥ 60), with the `/ep-N` suffix
/// stripped — ports the `$('div.item')` scan: exact or Japanese-title
/// match scores 100, one-sided containment scores 90·(length ratio).
fn best_card_href(html: &str, name: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let name_norm = normalize(name);
    let mut best_score = 0.0_f64;
    let mut best: Option<String> = None;
    for card in doc.select(&SEARCH_CARD) {
        if best_score >= 100.0 {
            break;
        }
        let href = card
            .select(&POSTER_LINK)
            .next()
            .and_then(|el| el.attr("href"))
            .or_else(|| {
                card.select(&NAME_LINK)
                    .next()
                    .and_then(|el| el.attr("href"))
            });
        let Some(href) = href else { continue };
        let name_el = card.select(&NAME_LINK).next();
        let title_norm = name_el
            .map(|el| normalize(&text_of(el)))
            .unwrap_or_default();
        let jp_norm = name_el
            .and_then(|el| el.attr("data-jp"))
            .map(normalize)
            .unwrap_or_default();
        if title_norm.is_empty() && jp_norm.is_empty() {
            continue;
        }
        let score = if title_norm == name_norm || jp_norm == name_norm {
            100.0
        } else if title_norm.contains(&name_norm) || name_norm.contains(&title_norm) {
            inclusion_score(&title_norm, &name_norm)
        } else if !jp_norm.is_empty()
            && (jp_norm.contains(&name_norm) || name_norm.contains(&jp_norm))
        {
            inclusion_score(&jp_norm, &name_norm)
        } else {
            0.0
        };
        if score > best_score {
            best_score = score;
            best = Some(strip_episode_suffix(href).to_string());
        }
    }
    if best_score >= 60.0 { best } else { None }
}

/// The numeric anime id from the detail page —
/// `.favourite[data-id]`, else the first `[data-id]`.
fn anime_id_from_detail(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    doc.select(&FAVOURITE)
        .next()
        .or_else(|| doc.select(&ANY_DATA_ID).next())
        .and_then(|el| el.attr("data-id"))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Find the target episode's data-ids + sub/dub flags in the AJAX HTML —
/// `data-slug` or link text may carry the number; movies fall back to
/// the first episode.
fn find_episode(html: &str, target: i64) -> Option<EpisodeLink> {
    let doc = Html::parse_document(html);
    let mut first: Option<ElementRef> = None;
    let mut matched: Option<ElementRef> = None;
    for el in doc.select(&EPISODE_LINK) {
        if first.is_none() {
            first = Some(el);
        }
        if matched.is_some() {
            continue;
        }
        let slug = leading_int(el.attr("data-slug").unwrap_or("0")).unwrap_or_default();
        let text = leading_int(&text_of(el)).unwrap_or_default();
        if slug == target || text == target {
            matched = Some(el);
        }
    }
    let el = matched.or(first)?;
    // A missing data-ids would query `servers=undefined` upstream and
    // miss; surfaced as a miss directly.
    let ids = el.attr("data-ids")?.to_string();
    Some(EpisodeLink {
        ids,
        has_sub: el.attr("data-sub") == Some("1"),
        has_dub: el.attr("data-dub") == Some("1"),
    })
}

/// Parse the server groups from the AJAX HTML — `.server-type, .type`
/// elements with `data-type`, each holding `.server, li` items with
/// `data-link-id`.
fn parse_servers(html: &str) -> Vec<ServerEntry> {
    let doc = Html::parse_document(html);
    let mut servers = Vec::new();
    for group in doc.select(&SERVER_GROUPS) {
        let server_type = group.attr("data-type").unwrap_or("sub").to_string();
        for item in group.select(&SERVER_ITEMS) {
            let Some(link_id) = item.attr("data-link-id").filter(|id| !id.is_empty()) else {
                continue;
            };
            let name = text_of(item).trim().to_string();
            if name.is_empty() {
                continue;
            }
            servers.push(ServerEntry {
                server_type: server_type.clone(),
                name,
                link_id: link_id.to_string(),
            });
        }
    }
    servers
}

/// Sub first, then dub, at most two servers per language, deduped by
/// server name. A language only runs when the episode flags it *or* the
/// server list advertises a group of that type (ports the upstream
/// `hasLang` guard exactly).
fn deduped_servers(servers: &[ServerEntry], episode: &EpisodeLink) -> Vec<ServerEntry> {
    let mut out: Vec<ServerEntry> = Vec::new();
    for (lang, has_lang) in [("sub", episode.has_sub), ("dub", episode.has_dub)] {
        let exists = servers.iter().any(|server| server.server_type == lang);
        if !has_lang && !exists {
            continue;
        }
        let mut seen_names = HashSet::new();
        for server in servers.iter().filter(|server| server.server_type == lang) {
            if !seen_names.insert(server.name.clone()) {
                continue;
            }
            out.push(ServerEntry {
                server_type: server.server_type.clone(),
                name: server.name.clone(),
                link_id: server.link_id.clone(),
            });
            if out
                .iter()
                .filter(|server| server.server_type == lang)
                .count()
                >= 2
            {
                break;
            }
        }
    }
    out
}

/// The display title — `name S01E02` for episodes, `name (year)` for
/// movies (ports the `getTmdbNameAndYear` + `formatSeasonAndEpisode`
/// formatting; the id/season/episode come from the reference, the
/// name/year from the resolved media).
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
fn target_episode(media: &MediaRef) -> i64 {
    i64::from(media.season.map_or(1, |_| media.episode.unwrap_or(1)))
}

/// `parseInt`-style leading-integer parse — the data attributes and
/// link texts the site emits.
fn leading_int(s: &str) -> Option<i64> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
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

/// Normalize for fuzzy title matching — lowercase, diacritics stripped,
/// non-alphanumerics dropped, whitespace collapsed.
fn normalize(s: &str) -> String {
    let kept: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_ascii_whitespace())
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

/// Strip a trailing `/ep-N` (optionally slash-terminated) from a card
/// href — `href.replace(/\/ep-\d+\/?$/, '')`.
fn strip_episode_suffix(href: &str) -> &str {
    let no_slash = href.strip_suffix('/').unwrap_or(href);
    if let Some(pos) = no_slash.rfind("/ep-") {
        let tail = &no_slash[pos + 4..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return &href[..pos];
        }
    }
    href
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

/// The concatenated text of an element (JS `$(el).text()`).
fn text_of(el: ElementRef) -> String {
    el.text().collect::<String>()
}

/// Parse a runtime-built URL — a structural surprise, not a miss.
fn site_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw).map_err(|error| {
        SourceError::scrape(PROVIDER_ID, format!("unparsable URL {raw:?}: {error}"))
    })
}

/// A constant URL — must parse.
fn parse_url(raw: &str) -> Url {
    Url::parse(raw).unwrap_or_else(|e| panic!("the AllWish URL {raw:?} must parse: {e}"))
}

/// `fetchPage`: HTML, or `None` on a miss. Upstream returns `null` for
/// any non-200; transport-level failures propagate (upstream
/// `gotScraping` throws on those).
async fn fetch_html(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &str,
) -> Result<Option<String>, SourceError> {
    let request = FetchRequest::get(url.clone())
        .with_timeout(TIMEOUT)
        .with_header("Referer", referer);
    match ctx.fetcher.request(request).await {
        Ok(response) if response.status == 200 => Ok(Some(response.body)),
        Ok(_) => Ok(None),
        Err(error) if is_miss(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `fetchJson`: the AJAX endpoints with `X-Requested-With` — `None` on
/// a miss or malformed JSON (upstream wraps `JSON.parse` in try/catch).
async fn fetch_json(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &str,
) -> Result<Option<Value>, SourceError> {
    let request = FetchRequest::get(url.clone())
        .with_timeout(TIMEOUT)
        .with_header("Accept", "application/json,text/plain,*/*")
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_header("Referer", referer);
    let response = match ctx.fetcher.request(request).await {
        Ok(response) if response.status == 200 => response,
        Ok(_) => return Ok(None),
        Err(error) if is_miss(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(response.json::<Value>().ok())
}

/// Whether a fetch failure is a miss — upstream's `throwHttpErrors:
/// false` turns non-200 answers (including blocks and rate limits)
/// into `null`, while transport errors throw and propagate.
fn is_miss(error: &FetchError) -> bool {
    matches!(
        error,
        FetchError::NotFound { .. }
            | FetchError::Http { .. }
            | FetchError::RateLimited { .. }
            | FetchError::Blocked { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use vsources_core::error::ExtractorError;
    use vsources_core::traits::{Extractor, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;
    use vsources_core::types::MediaId;

    /// The searched title.
    const NAME: &str = "Frieren: Beyond Journey's End";

    /// A fetcher serving canned bodies keyed by `host + path?query` and
    /// recording every request (the extractors' `ScriptedFetcher` pattern
    /// — `pub(crate)` there, so a per-module copy lives here).
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, String>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
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

        /// The value of a header sent with the request for `url`.
        fn sent_header(&self, url: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| key_of(&request.url) == url)
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }

        /// Every request seen so far, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
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
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
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
                        "text/html".to_string(),
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
    /// embeds and answers one direct HLS stream per embed token.
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
            let token = url.path().rsplit('/').next().unwrap_or_default();
            let direct = Url::parse(&format!("https://cdn.example.com/{token}/index.m3u8"))
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

    /// The search page: the first card matches exactly.
    fn search_page() -> String {
        r#"<html><body><div class="list">
            <div class="item">
                <a class="poster" href="/watch/sousou-no-frieren-abc12/ep-1"></a>
                <div class="name"><a href="/watch/sousou-no-frieren-abc12" data-jp="Sousou no Frieren">Frieren: Beyond Journey's End</a></div>
            </div>
            <div class="item">
                <a class="poster" href="/watch/frieren-spinoff-xyz99/ep-1"></a>
                <div class="name"><a href="/watch/frieren-spinoff-xyz99" data-jp="Spinoff">Frieren Spinoff</a></div>
            </div>
        </div></body></html>"#
            .to_string()
    }

    /// The detail page: `.favourite[data-id]`.
    const DETAIL_PAGE: &str =
        r#"<html><body><button class="favourite" data-id="999">♥</button></body></html>"#;

    /// The AJAX episode list: episodes 1 and 2, both sub and dub.
    fn episode_list() -> String {
        r#"{"status":200,"result":"<div class='screen episodes'><ol><li><a data-ids='165071,165072' data-slug='1' data-sub='1' data-dub='1'>1</a></li><li><a data-ids='165073,165074' data-slug='2' data-sub='1' data-dub='1'>2</a></li></ol></div>"}"#
            .to_string()
    }

    /// The AJAX server list: sub (`MegaCloud` + Vidplay) and dub (`MegaCloud`).
    fn server_list() -> String {
        r#"{"status":200,"result":"<div class='server-types'><div class='server-type' data-type='sub'><div class='server' data-link-id='77'>MegaCloud</div><div class='server' data-link-id='78'>Vidplay</div></div><div class='server-type' data-type='dub'><div class='server' data-link-id='79'>MegaCloud</div></div></div>"}"#
            .to_string()
    }

    /// The full fixture set for a series episode resolve.
    fn scripted_pages() -> ScriptedFetcher {
        let mut fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://all-wish.me/filter?keyword={}",
                    encode_component(NAME)
                )),
                search_page(),
            )
            .page("all-wish.me/watch/sousou-no-frieren-abc12", DETAIL_PAGE)
            .page("all-wish.me/ajax/episode/list/999", episode_list())
            .page(
                "all-wish.me/ajax/server/list?servers=165073,165074",
                server_list(),
            )
            .page(
                "all-wish.me/ajax/server/list?servers=165071,165072",
                server_list(),
            );
        for (link_id, token) in [("77", "tokA"), ("78", "tokB"), ("79", "tokC")] {
            fetcher = fetcher.page(
                format!("all-wish.me/ajax/server?get={link_id}"),
                format!(r#"{{"status":200,"result":{{"url":"https://megaplay.buzz/stream/s-1/{token}"}}}}"#),
            );
        }
        fetcher
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_embeds_through_the_registry() -> Result<(), SourceError> {
        let fetcher = scripted_pages();
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AllWish::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the happy path must resolve: {e}"));
        assert_eq!(streams.len(), 3, "two sub servers + one dub server");

        let sub = &streams[0];
        assert_eq!(sub.url.as_str(), "https://cdn.example.com/tokA/index.m3u8");
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(sub.ttl.as_secs(), 1800);
        assert_eq!(
            sub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · MegaCloud)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.source_id.as_deref(), Some("allwish"));
        assert_eq!(sub.meta.source_label.as_deref(), Some("AllWish"));
        assert_eq!(sub.meta.extractor_label.as_deref(), Some("MegaplayStub"));

        assert_eq!(
            streams[1].label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · Vidplay)")
        );
        let dub = &streams[2];
        assert_eq!(dub.url.as_str(), "https://cdn.example.com/tokC/index.m3u8");
        assert_eq!(
            dub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Dub · MegaCloud)")
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );

        // The AJAX endpoints carried X-Requested-With and the watch-page
        // Referer chain, exactly like the JS.
        assert_eq!(
            fetcher
                .sent_header("all-wish.me/ajax/episode/list/999", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            fetcher
                .sent_header(
                    "all-wish.me/ajax/server/list?servers=165073,165074",
                    "Referer"
                )
                .as_deref(),
            Some("https://all-wish.me/watch/sousou-no-frieren-abc12/ep-2")
        );
        assert_eq!(
            fetcher
                .sent_header("all-wish.me/ajax/server?get=77", "Referer")
                .as_deref(),
            Some("https://all-wish.me/watch/sousou-no-frieren-abc12/ep-2")
        );
        // data-ids went over the wire raw (not URL-encoded).
        assert!(fetcher.requests().iter().any(|request| request.url.as_str()
            == "https://all-wish.me/ajax/server/list?servers=165073,165074"));
        Ok(())
    }

    #[tokio::test]
    async fn movies_fall_back_to_the_first_episode() -> Result<(), SourceError> {
        let fetcher = scripted_pages();
        let ctx = ctx_for(&fetcher, Some(resolved_media(None, None)));
        let media = media_ref(None, None);

        let streams = AllWish::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the movie path must resolve: {e}"));
        // Episode 1's ids were requested — `servers=165071,165072`.
        assert!(fetcher.requests().iter().any(|request| request.url.as_str()
            == "https://all-wish.me/ajax/server/list?servers=165071,165072"));
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End (2023) (Sub · MegaCloud)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_language_without_a_flag_or_group_is_skipped() -> Result<(), SourceError> {
        // data-dub absent → dub needs an explicit server group; the
        // fixture's dub group still runs (ports the upstream hasLang
        // guard exactly).
        let fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://all-wish.me/filter?keyword={}",
                    encode_component(NAME)
                )),
                search_page(),
            )
            .page("all-wish.me/watch/sousou-no-frieren-abc12", DETAIL_PAGE)
            .page(
                "all-wish.me/ajax/episode/list/999",
                r#"{"status":200,"result":"<div class='screen'><a data-ids='165073,165074' data-slug='2' data-sub='1'>2</a></div>"}"#,
            )
            .page(
                "all-wish.me/ajax/server/list?servers=165073,165074",
                server_list(),
            )
            .page(
                "all-wish.me/ajax/server?get=77",
                r#"{"status":200,"result":{"url":"https://megaplay.buzz/stream/s-1/tokA"}}"#,
            );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = AllWish::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the sub-only path must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · MegaCloud)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_search_without_a_match_is_not_found() {
        // Non-matching cards, then unregistered variant pages miss.
        let fetcher = ScriptedFetcher::new().page(
            url_key(&format!(
                "https://all-wish.me/filter?keyword={}",
                encode_component(NAME)
            )),
            r#"<html><body><div class="item"><a class="poster" href="/watch/other-thing-aa111"></a>
                <div class="name"><a href="/watch/other-thing-aa111" data-jp="">Something Else Entirely</a></div>
            </div></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match AllWish::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a search miss must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_detail_page_without_an_anime_id_is_not_found() {
        let fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://all-wish.me/filter?keyword={}",
                    encode_component(NAME)
                )),
                search_page(),
            )
            .page(
                "all-wish.me/watch/sousou-no-frieren-abc12",
                "<html><body><p>no ids here</p></body></html>",
            );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match AllWish::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing data-id must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let ctx = ctx_for(&fetcher, None);
        let media = media_ref(Some(1), Some(2));

        match AllWish::new(registry()).resolve(&ctx, &media).await {
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
            ("", ""),
        ] {
            assert_eq!(normalize(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn strips_episode_suffixes() {
        for (href, expected) in [
            ("/watch/slug-abc12/ep-1", "/watch/slug-abc12"),
            ("/watch/slug-abc12/ep-12/", "/watch/slug-abc12"),
            ("/watch/slug-abc12", "/watch/slug-abc12"),
            ("/watch/episode-1", "/watch/episode-1"),
        ] {
            assert_eq!(strip_episode_suffix(href), expected, "href {href:?}");
        }
    }

    #[test]
    fn encodes_like_the_js_function() {
        assert_eq!(encode_component("Frieren: Beyond"), "Frieren%3A%20Beyond");
        assert_eq!(encode_component("123,456"), "123%2C456");
        assert_eq!(encode_component("plain-id_1."), "plain-id_1.");
    }
}
