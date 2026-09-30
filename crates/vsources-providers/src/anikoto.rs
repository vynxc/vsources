//! `Anikoto`: anime embeds from the HiAnime-family AJAX backend of
//! `anikoto.cz`.
//!
//! Ports `src/source/Anikoto.js` — same family as `AllWish`, with one
//! wrinkle: the episode-list endpoint validates a `vrf` parameter:
//!
//! 1. search `GET /filter?keyword={title}` → cards linking
//!    `/watch/{slug}/ep-{N}`;
//! 2. detail `GET /watch/{slug}` → the numeric anime id from the
//!    `layout-page-watch` element's `data-id`;
//! 3. episodes `GET /ajax/episode/list/{animeId}?style=grid&vrf={vrf}`
//!    where `vrf = base64(RC4("simple-hash", String(animeId)))`
//!    (`X-Requested-With: XMLHttpRequest`) →
//!    `{status: 200, result: "<html>"}` with
//!    `<a data-ids=… data-num=… data-slug=… data-mal=… data-timestamp=…>`;
//! 4. servers `GET /ajax/server/list?servers={urlencoded data-ids}` →
//!    `.type[data-type=sub|dub|hsub] li[data-link-id]`;
//! 5. embed `GET /ajax/server?get={linkId}` →
//!    `{result: {url: "https://megaplay.buzz/stream/…"}}`.
//!
//! All embeds point at megaplay.buzz (or its vidtube.site mirror) and
//! are resolved through the shared [`ExtractorRegistry`] — upstream's
//! resolver post-source extraction stage, folded into the provider
//! here. Anime on anikoto is episode-based (flat numbering), so the
//! reference's episode number maps to the absolute episode. Upstream
//! notes the site serves HTTP 200 to a desktop UA with no JS challenge
//! — the fetcher layer provides that UA.
//!
//! Mappings and cuts (vs. upstream):
//! - `meta.title` (`{title} (Sub · {server})`) → [`Stream::label`]; the
//!   producing extractor stays attributed in `meta.extractor_label`.
//! - `meta.countryCodes` → `meta.languages` (`hsub` keeps the Japanese
//!   pair like the upstream default, only the label says "Hard Sub").
//! - The upstream 5min `this.ttl` override is the source result-cache
//!   TTL — the parent's `CachedSource` owns that; streams keep the
//!   extractor's ttl.
//! - `getTmdbId`/`getTmdbNameAndYear` → `ctx.media`; missing media (no
//!   title to search) answers [`SourceError::NotFound`].
//! - Extractor failures are skipped per embed — upstream
//!   `extractorRegistry.handle(...).catch(() => [])`.
//! - Explicit `User-Agent`/`Accept-Language`/HTML `Accept` dropped — the
//!   fetcher layer already sends browser-like headers.
//! - No `/proxy` routing: `requestHeaders` travel on
//!   `meta.request_headers` (this source sets none upstream).
//! - RC4 and base64 are hand-rolled (`rc4`, `base64_encode`) — the
//!   provider crate carries no crypto/base64 dependencies.
//! - Unicode `NFD` decomposition is unavailable without an extra
//!   dependency; `normalize` still strips combining marks (U+0300–036F),
//!   and precomposed accents drop out.

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
const BASE_URL: &str = "https://anikoto.cz";
/// This provider's id, for scrape diagnostics.
const PROVIDER_ID: &str = "anikoto";
/// Upstream `timeout: { request: 15000 }`.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Search result cards (`div.item`).
static SEARCH_CARD: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".item").unwrap_or_else(|e| panic!("valid card selector: {e}"))
});
/// The card title link (`.name.d-title`).
static TITLE_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".name.d-title").unwrap_or_else(|e| panic!("valid title selector: {e}"))
});
/// Episode anchors in the AJAX episode-list HTML.
static EPISODE_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a[data-ids]").unwrap_or_else(|e| panic!("valid episode selector: {e}"))
});
/// Server groups (`.type`).
static SERVER_GROUPS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".type").unwrap_or_else(|e| panic!("valid server-group selector: {e}"))
});
/// Server entries within a group (`li[data-link-id]`).
static SERVER_ITEMS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("li[data-link-id]").unwrap_or_else(|e| panic!("valid server selector: {e}"))
});
/// Any `[data-id]` element on the detail page.
static ANY_DATA_ID: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("[data-id]").unwrap_or_else(|e| panic!("valid data-id selector: {e}"))
});

/// The `anikoto.cz` provider: HiAnime-family AJAX scraping with the
/// RC4-protected `vrf` parameter.
pub struct Anikoto {
    /// Static descriptor.
    info: SourceInfo,
    /// The site root (upstream `BASE_URL`).
    base: Url,
    /// The embed resolver — upstream's post-source extraction stage.
    registry: Arc<ExtractorRegistry>,
}

impl Anikoto {
    /// A provider resolving megaplay embeds through `registry`.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: PROVIDER_ID.to_string(),
                label: "Anikoto".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(
                    Url::parse(BASE_URL)
                        .unwrap_or_else(|e| panic!("the Anikoto base URL must parse: {e}")),
                ),
                priority: 0,
                // Upstream leaves `this.domainKey` unset.
                domain_key: None,
            },
            base: Url::parse(BASE_URL)
                .unwrap_or_else(|e| panic!("the Anikoto base URL must parse: {e}")),
            registry,
        }
    }

    /// Step 1: search by name and return the best-matching watch page.
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
        if data.get("status").and_then(Value::as_i64) != Some(200) {
            return Ok(None);
        }
        let Some(link) = data.pointer("/result/url").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(site_url(link)?))
    }
}

#[async_trait]
impl Source for Anikoto {
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

        // Step 1: search anikoto by name.
        let Some(watch) = self.find_watch_page(ctx, &resolved.name).await? else {
            return Err(SourceError::NotFound);
        };

        // Step 2: detail page → the watch layout's numeric anime id.
        let Some(detail_html) = fetch_html(ctx, &watch, self.base.as_str()).await? else {
            return Err(SourceError::NotFound);
        };
        let Some(anime_id) = anime_id_from_detail(&detail_html) else {
            return Err(SourceError::NotFound);
        };

        // Step 3: episode list via AJAX, with the RC4-protected vrf.
        let episode_list = format!(
            "{BASE_URL}/ajax/episode/list/{anime_id}?style=grid&vrf={}",
            encode_component(&vrf(&anime_id))
        );
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

        // Step 4: the target episode's data-ids (for movies the slug is
        // "1"; for series the absolute episode number).
        let Some(episode) = find_episode(result_html, target) else {
            return Err(SourceError::NotFound);
        };

        // Step 5: server list — ids URL-encoded here (unlike AllWish,
        // which sends them raw).
        let server_list = format!(
            "{BASE_URL}/ajax/server/list?servers={}",
            encode_component(&episode.ids)
        );
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

        // Step 6: parse the server groups.
        let servers = parse_servers(result_html);
        if servers.is_empty() {
            return Err(SourceError::NotFound);
        }
        let deduped = deduped_servers(&servers);
        let ep_page = site_url(&ep_referer)?;

        // Step 7: resolve embed URLs — sub, dub, hsub, at most three
        // servers per language; skip VidPlay servers (their vidtube.site
        // URLs resolve to the wrong content through megaplay's
        // getSourcesNew API).
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

            let audio = match server.server_type.as_str() {
                "dub" => "Dub",
                "hsub" => "Hard Sub",
                _ => "Sub",
            };
            let languages = if server.server_type == "dub" {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            };
            let label = format!("{title} ({audio} · {})", server.name);
            if !seen_labels.insert(format!("{audio}_{}", server.name)) {
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
    /// The group's `data-type` — `sub`, `dub`, or `hsub`.
    server_type: String,
    /// The server's display name (the link text).
    name: String,
    /// `data-link-id` for `/ajax/server?get=`.
    link_id: String,
}

/// The target episode parsed from the AJAX episode-list HTML.
struct EpisodeLink {
    /// `data-ids` — the id list (comma-separated).
    ids: String,
}

/// The best-matching card href (score ≥ 60), with the `/ep-N` suffix
/// stripped — ports the `$('.item')` scan: exact or Japanese-title match
/// scores 100, one-sided containment scores 90·(length ratio).
fn best_card_href(html: &str, name: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let name_norm = normalize(name);
    let mut best_score = 0.0_f64;
    let mut best: Option<String> = None;
    for card in doc.select(&SEARCH_CARD) {
        if best_score >= 100.0 {
            break;
        }
        let Some(title_el) = card.select(&TITLE_LINK).next() else {
            continue;
        };
        let Some(href) = title_el.attr("href") else {
            continue;
        };
        let title_norm = normalize(&text_of(title_el));
        let jp_norm = title_el.attr("data-jp").map(normalize).unwrap_or_default();
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

/// The numeric anime id from the detail page — the `data-id` of the
/// element whose class starts with `layout-page-watch` (ports
/// `/class="layout-page-watch[^"]*"[^>]*data-id="(\d+)"/`).
fn anime_id_from_detail(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    doc.select(&ANY_DATA_ID)
        .find(|el| {
            el.attr("class")
                .is_some_and(|class| class.starts_with("layout-page-watch"))
        })
        .and_then(|el| el.attr("data-id"))
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Find the target episode's data-ids in the AJAX HTML — matching by
/// `data-num` or a `data-slug` equal to the target; movies fall back to
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
        let num = leading_int(el.attr("data-num").unwrap_or("0")).unwrap_or_default();
        let slug = el.attr("data-slug").unwrap_or_default();
        if num == target || slug == target.to_string() {
            matched = Some(el);
        }
    }
    let el = matched.or(first)?;
    // A missing data-ids would query `servers=undefined` upstream and
    // miss; surfaced as a miss directly.
    let ids = el.attr("data-ids")?.to_string();
    Some(EpisodeLink { ids })
}

/// Parse the server groups from the AJAX HTML — `.type` elements with
/// `data-type`, each holding `li[data-link-id]` items.
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

/// Sub, dub, then hsub, at most three servers per language, deduped by
/// server name, with `VidPlay` servers skipped (their embeds resolve to
/// the wrong anime).
fn deduped_servers(servers: &[ServerEntry]) -> Vec<ServerEntry> {
    let mut out: Vec<ServerEntry> = Vec::new();
    for lang in ["sub", "dub", "hsub"] {
        let mut seen_names = HashSet::new();
        for server in servers
            .iter()
            .filter(|server| server.server_type == lang)
            .filter(|server| !server.name.to_lowercase().contains("vidplay"))
        {
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
                >= 3
            {
                break;
            }
        }
    }
    out
}

/// RC4 stream cipher — ports the upstream `rc4` helper, used to derive
/// the episode-list `vrf` parameter.
fn rc4(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut state: [u8; 256] = std::array::from_fn(|i| u8::try_from(i).unwrap_or_default());
    let mut a = 0_usize;
    for n in 0..256 {
        a = (a + state[n] as usize + key[n % key.len()] as usize) % 256;
        state.swap(n, a);
    }
    let mut a = 0_usize;
    let mut n = 0_usize;
    let mut out = Vec::with_capacity(plaintext.len());
    for &byte in plaintext {
        n = (n + 1) % 256;
        let e = state[n];
        a = (a + state[n] as usize) % 256;
        state[n] = state[a];
        state[a] = e;
        let keystream = state[(state[n] as usize + state[a] as usize) % 256];
        out.push(byte ^ keystream);
    }
    out
}

/// Standard base64 with padding — ports
/// `Buffer.from(…, 'binary').toString('base64')`.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = chunk.get(1).copied().map_or(0, u32::from);
        let b3 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b1 << 16) | (b2 << 8) | b3;
        out.push(ALPHABET[triple as usize >> 18 & 0x3F] as char);
        out.push(ALPHABET[triple as usize >> 12 & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[triple as usize >> 6 & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

/// The episode-list `vrf`: base64(RC4("simple-hash", animeId)).
fn vrf(anime_id: &str) -> String {
    base64_encode(&rc4(b"simple-hash", anime_id.as_bytes()))
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

/// The absolute episode number to look for — the reference's episode
/// for series, 1 for movies (`tmdbId.season ? (tmdbId.episode || 1) : 1`).
fn target_episode(media: &MediaRef) -> i64 {
    i64::from(media.season.map_or(1, |_| media.episode.unwrap_or(1)))
}

/// `parseInt`-style leading-integer parse — the data attributes the
/// site emits.
fn leading_int(s: &str) -> Option<i64> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
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

/// The concatenated text of an element (JS `$(el).text()`).
fn text_of(el: ElementRef) -> String {
    el.text().collect::<String>()
}

/// Parse a URL the provider built — a structural surprise, not a miss.
fn site_url(raw: &str) -> Result<Url, SourceError> {
    Url::parse(raw).map_err(|error| {
        SourceError::scrape(PROVIDER_ID, format!("unparsable URL {raw:?}: {error}"))
    })
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

    /// The golden `vrf` for anime id 12345, verified against the
    /// upstream JS (`Buffer.from(rc4('simple-hash', '12345'),
    /// 'binary').toString('base64')`).
    const VRF_12345: &str = "Jtar0UM=";

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
                <a class="name d-title" href="/watch/sousou-no-frieren/ep-1" data-jp="Sousou no Frieren">Frieren: Beyond Journey's End</a>
            </div>
            <div class="item">
                <a class="name d-title" href="/watch/frieren-spinoff/ep-1" data-jp="Spinoff">Frieren Spinoff</a>
            </div>
        </div></body></html>"#
            .to_string()
    }

    /// The detail page: the watch layout element carries data-id.
    const DETAIL_PAGE: &str =
        r#"<html><body><div class="layout-page-watch" data-id="12345"></div></body></html>"#;

    /// The AJAX episode list: episodes 1 and 2.
    fn episode_list() -> String {
        r#"{"status":200,"result":"<div class='screen episodes'><ol><li><a data-ids='165071,165072' data-num='1' data-slug='1' data-mal='52991' data-timestamp='1700000000'>1</a></li><li><a data-ids='165073,165074' data-num='2' data-slug='2' data-mal='52991' data-timestamp='1700000001'>2</a></li></ol></div>"}"#
            .to_string()
    }

    /// The AJAX server list: sub (`MegaCloud` + Vidplay + Filemoon), dub
    /// (`MegaCloud`), hsub (`MegaCloud`).
    fn server_list() -> String {
        r#"{"status":200,"result":"<div class='server-types'><div class='type' data-type='sub'><ul><li data-link-id='77'>MegaCloud</li><li data-link-id='78'>Vidplay</li><li data-link-id='80'>Filemoon</li></ul></div><div class='type' data-type='dub'><ul><li data-link-id='79'>MegaCloud</li></ul></div><div class='type' data-type='hsub'><ul><li data-link-id='81'>MegaCloud</li></ul></div></div>"}"#
            .to_string()
    }

    /// The full fixture set for a series episode resolve.
    fn scripted_pages() -> ScriptedFetcher {
        let mut fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://anikoto.cz/filter?keyword={}",
                    encode_component(NAME)
                )),
                search_page(),
            )
            .page("anikoto.cz/watch/sousou-no-frieren", DETAIL_PAGE)
            .page(
                format!(
                    "anikoto.cz/ajax/episode/list/12345?style=grid&vrf={}",
                    encode_component(VRF_12345)
                ),
                episode_list(),
            )
            .page(
                "anikoto.cz/ajax/server/list?servers=165073%2C165074",
                server_list(),
            )
            .page(
                "anikoto.cz/ajax/server/list?servers=165071%2C165072",
                server_list(),
            );
        for (link_id, token) in [("77", "tokA"), ("79", "tokC"), ("81", "tokD")] {
            fetcher = fetcher.page(
                format!("anikoto.cz/ajax/server?get={link_id}"),
                format!(r#"{{"status":200,"result":{{"url":"https://megaplay.buzz/stream/s-2/{token}"}}}}"#),
            );
        }
        fetcher
    }

    #[tokio::test]
    async fn resolves_sub_dub_and_hsub_embeds_through_the_registry() -> Result<(), SourceError> {
        let fetcher = scripted_pages();
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        let streams = Anikoto::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the happy path must resolve: {e}"));
        // Vidplay was skipped; sub/dub/hsub each kept their MegaCloud.
        assert_eq!(streams.len(), 3);

        let sub = &streams[0];
        assert_eq!(sub.url.as_str(), "https://cdn.example.com/tokA/index.m3u8");
        assert_eq!(sub.format, Format::Hls);
        assert_eq!(
            sub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Sub · MegaCloud)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.source_id.as_deref(), Some("anikoto"));
        assert_eq!(sub.meta.source_label.as_deref(), Some("Anikoto"));
        assert_eq!(sub.meta.extractor_label.as_deref(), Some("MegaplayStub"));

        let dub = &streams[1];
        assert_eq!(
            dub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Dub · MegaCloud)")
        );
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        let hsub = &streams[2];
        assert_eq!(
            hsub.label.as_deref(),
            Some("Frieren: Beyond Journey's End S01E02 (Hard Sub · MegaCloud)")
        );
        // hsub keeps the Japanese language pair like the upstream default.
        assert_eq!(
            hsub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );

        // The AJAX endpoints carried X-Requested-With; the data-ids were
        // URL-encoded (unlike AllWish's raw form).
        assert_eq!(
            fetcher
                .sent_header(
                    "anikoto.cz/ajax/episode/list/12345?style=grid&vrf=Jtar0UM%3D",
                    "X-Requested-With"
                )
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert!(fetcher.requests().iter().any(|request| request.url.as_str()
            == "https://anikoto.cz/ajax/server/list?servers=165073%2C165074"));
        Ok(())
    }

    #[tokio::test]
    async fn movies_fall_back_to_the_first_episode() -> Result<(), SourceError> {
        let fetcher = scripted_pages();
        let ctx = ctx_for(&fetcher, Some(resolved_media(None, None)));
        let media = media_ref(None, None);

        let streams = Anikoto::new(registry())
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the movie path must resolve: {e}"));
        assert_eq!(streams.len(), 3);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Frieren: Beyond Journey's End (2023) (Sub · MegaCloud)")
        );
        assert!(fetcher.requests().iter().any(|request| request.url.as_str()
            == "https://anikoto.cz/ajax/server/list?servers=165071%2C165072"));
        Ok(())
    }

    #[tokio::test]
    async fn a_search_without_a_match_is_not_found() {
        let fetcher = ScriptedFetcher::new().page(
            url_key(&format!(
                "https://anikoto.cz/filter?keyword={}",
                encode_component(NAME)
            )),
            r#"<html><body><div class="item"><a class="name d-title" href="/watch/other-thing" data-jp="">Something Else Entirely</a></div></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match Anikoto::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a search miss must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_detail_page_without_the_watch_layout_is_not_found() {
        let fetcher = ScriptedFetcher::new()
            .page(
                url_key(&format!(
                    "https://anikoto.cz/filter?keyword={}",
                    encode_component(NAME)
                )),
                search_page(),
            )
            .page(
                "anikoto.cz/watch/sousou-no-frieren",
                r#"<html><body><div class="other" data-id="1"></div></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, Some(resolved_media(Some(1), Some(2))));
        let media = media_ref(Some(1), Some(2));

        match Anikoto::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("a missing watch layout must be NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_is_not_found() {
        let fetcher = ScriptedFetcher::new();
        let ctx = ctx_for(&fetcher, None);
        let media = media_ref(Some(1), Some(2));

        match Anikoto::new(registry()).resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("no resolved media must be NotFound, got {other:?}"),
        }
    }

    /// The classic RC4 test vector.
    #[test]
    fn rc4_matches_the_reference_vector() {
        let cipher = rc4(b"Key", b"Plaintext");
        let hex = cipher.iter().fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02X}");
            out
        });
        assert_eq!(hex, "BBF316E8D940AF0AD3");
    }

    /// The golden upstream vrf values (computed with Node).
    #[test]
    fn vrf_matches_the_upstream_values() {
        for (anime_id, expected) in [("12345", "Jtar0UM="), ("99999", "Lt2h3E8="), ("1", "Jg==")] {
            assert_eq!(vrf(anime_id), expected, "anime id {anime_id}");
        }
    }

    #[test]
    fn base64_encodes_with_padding() {
        for (input, expected) in [
            (&b"hello"[..], "aGVsbG8="),
            (&b"abc"[..], "YWJj"),
            (&b"a"[..], "YQ=="),
            (&b"ab"[..], "YWI="),
        ] {
            assert_eq!(base64_encode(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn strips_episode_suffixes() {
        for (href, expected) in [
            ("/watch/sousou-no-frieren/ep-1", "/watch/sousou-no-frieren"),
            (
                "/watch/sousou-no-frieren/ep-12/",
                "/watch/sousou-no-frieren",
            ),
            ("/watch/sousou-no-frieren", "/watch/sousou-no-frieren"),
        ] {
            assert_eq!(strip_episode_suffix(href), expected, "href {href:?}");
        }
    }

    #[test]
    fn parses_the_anime_id_from_the_watch_layout() {
        for (html, expected) in [
            (
                r#"<div class="layout-page-watch" data-id="12345"></div>"#,
                Some("12345"),
            ),
            (
                r#"<div class="layout-page-watch other-class" data-id="99"></div>"#,
                Some("99"),
            ),
            (r#"<div class="something-else" data-id="1"></div>"#, None),
            (
                r#"<div class="layout-page-watch" data-id="abc"></div>"#,
                None,
            ),
        ] {
            assert_eq!(
                anime_id_from_detail(html).as_deref(),
                expected,
                "html {html:?}"
            );
        }
    }
}
