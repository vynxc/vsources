//! `AnimeFlix`: `WordPress` anime embeds from `9animes.me.uk`.
//!
//! Ports `src/source/AnimeFlix.js` — `9animes.me.uk` (formerly
//! `animeflix.team`; the site migrated and the old domain's search now
//! only links here), the same `WordPress` structure as `9anime.cl`:
//!
//! 1. The `WordPress` REST search (`/wp-json/wp/v2/search`) is the
//!    primary path: the site's `?s=` HTML search now ignores the query
//!    and returns the same latest-posts list for everything, so the
//!    scored matcher refused every title and the source went silent.
//!    The legacy `?s=` scrape stays as a fallback in case the REST
//!    endpoint ever goes away.
//! 2. Candidates are scored against the title: exact match (100),
//!    ASCII-folded exact (95), substring ratio (up to 90), then a
//!    word-overlap fallback (up to 75) for per-arc/per-season pages
//!    whose titles never match a TMDB title exactly. Typographic
//!    quotes are normalized first — site headings use `’` (U+2019)
//!    while TMDB uses `'` (U+0027). A matching year in the href adds 5.
//!    Threshold is 40 (lowered from 60 for the same per-arc reason).
//! 3. The best page plus a distinct `-dub` page are each scraped
//!    (sub and dub are separate `/Anime/…` entries; one candidate
//!    failing must not kill the other).
//! 4. The episode link is matched boundary-aware
//!    (`episode-5/`, `episode-5-`, `episode-5#`, or end — but NOT
//!    `episode-50`) because the DOM lists episodes descending; the
//!    movie case takes the last link (episode 1).
//! 5. The episode page's first iframe, or its base64 `data-hash`
//!    server iframes (`src="…"` inside the decoded HTML, `&amp;`
//!    unescaped), are resolved through the
//!    `ExtractorRegistry`.
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's TMDB
//!   resolution: name/year come from
//!   [`ctx.media`](vsources_core::traits::ResolveCtx::media), and
//!   season/episode from the [`MediaRef`]. Without `ctx.media` there is
//!   no title to search, which maps to [`SourceError::NotFound`].
//! - Upstream never sets `meta.vidking` for this source (the JS
//!   comment: "Don't pass vidking for anime — speedracelight returns
//!   wrong content"), so the extraction context carries no media and
//!   the `VidKing` media fallback never joins the extractor chain.
//! - `meta.title` becomes the stream label and `meta.countryCodes`
//!   become `meta.languages`. The JS's 12h result TTL was capped at
//!   15min by the resolver's cache, so streams carry 15min.
//! - Cut: `fetchAnimePageUrl` (a dead upstream helper), the JS's
//!   `console.log` diagnostics (this crate has no logging facade), and
//!   the per-source result cache (the parent's `CachedSource` owns it).
//! - Patterns are ported as `scraper` selectors and byte scanners (this
//!   crate has no regex engine); NFD folding is approximated with a
//!   Latin accent table, which covers the TMDB titles these searches
//!   see.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{ResolveCtx, Source, fetch_json, fetch_text};
use vsources_core::types::{CountryCode, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id (upstream `this.id`).
const ID: &str = "animeflix";
/// Upstream effective result lifetime: `this.ttl` is the 12h default,
/// which the resolver's cache capped at 15min.
const TTL: Duration = Duration::from_mins(15);
/// Candidate acceptance threshold (upstream lowered 60 → 40).
const MIN_CANDIDATE_SCORE: f64 = 40.0;

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://9animes.me.uk")
        .unwrap_or_else(|_| panic!("the AnimeFlix base URL must parse"))
});

static ANIME_LINKS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(r#"a[href*="/Anime/"]"#)
        .unwrap_or_else(|_| panic!(r#"valid a[href*="/Anime/"] selector"#))
});
static HEADING: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("h1, h2, h3, h4, h5, h6").unwrap_or_else(|_| panic!("valid heading selector"))
});
static EPISODE_LINKS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(r#"a[href*="episode"]"#)
        .unwrap_or_else(|_| panic!(r#"valid a[href*="episode"] selector"#))
});
static SERVER_HASH_LINKS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".server-item a[data-hash]")
        .unwrap_or_else(|_| panic!("valid .server-item a[data-hash] selector"))
});
static IFRAME: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("iframe").unwrap_or_else(|_| panic!("valid iframe selector")));

/// A scored anime page candidate (upstream `{href, score}`).
struct Candidate {
    /// The page href (absolute from wp-json, possibly relative from `?s=`).
    href: String,
    /// The `scoreCandidate` score.
    score: f64,
}

/// One scraped episode page: the page URL plus the embed URLs found on it.
struct PageEmbeds {
    /// The episode page the embeds came from (the extraction referer).
    page: Url,
    /// Iframe URLs: the direct first iframe, or every decoded
    /// `data-hash` server iframe.
    embeds: Vec<Url>,
}

/// An entry of the `WordPress` REST search endpoint.
#[derive(serde::Deserialize)]
struct WpSearchEntry {
    /// The result URL — must contain `/Anime/` to be a candidate.
    #[serde(default)]
    url: Option<String>,
    /// The result title.
    #[serde(default)]
    title: Option<String>,
}

/// The `AnimeFlix` provider.
pub struct AnimeFlix {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that resolves the scraped iframes.
    registry: Arc<ExtractorRegistry>,
}

impl AnimeFlix {
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
                label: "AnimeFlix".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Collect the embed URLs of one anime page (sub or dub variant).
    ///
    /// Ports `collectFromPage`: anime page → episode link → episode
    /// page → direct iframe / `data-hash` iframes. A missing episode
    /// link is the upstream `return []`; a failed fetch propagates so
    /// the caller can skip just this candidate.
    async fn collect_from_page(
        &self,
        ctx: &ResolveCtx<'_>,
        href: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<PageEmbeds, SourceError> {
        let page_url = BASE.join(href).map_err(|error| {
            SourceError::scrape(
                ID,
                format!("the anime page href {href:?} is invalid: {error}"),
            )
        })?;
        let html = fetch_text(ctx.fetcher, page_url.clone()).await?;

        // The episode link: series match boundary-aware ("episode-5/",
        // "episode-5-" or "episode-5#" but NOT "episode-50" — the DOM
        // lists episodes DESCENDING, so a naive fallback could grab
        // episode 50); movies take the LAST link (episode 1).
        //
        // `scraper::Html` is not `Send` (tendril's `NonAtomic`
        // refcount), so the document is scoped away before the
        // episode-page fetch crosses an await.
        let found: Option<String> = {
            let doc = Html::parse_document(&html);
            let hrefs: Vec<String> = doc
                .select(&EPISODE_LINKS)
                .filter_map(|link| link.value().attr("href"))
                .map(str::to_string)
                .collect();
            match season {
                Some(_) => {
                    let ep_num = episode.unwrap_or(1);
                    hrefs
                        .iter()
                        .find(|href| episode_link_matches(href, ep_num))
                        .or_else(|| {
                            hrefs
                                .iter()
                                .find(|href| href.contains(&format!("episode-{ep_num}-")))
                                .or_else(|| {
                                    hrefs
                                        .iter()
                                        .find(|href| href.contains(&format!("episode-{ep_num}")))
                                })
                        })
                        .cloned()
                }
                None => hrefs.last().cloned(),
            }
        };
        let Some(episode_link) = found else {
            return Ok(PageEmbeds {
                page: page_url,
                embeds: Vec::new(),
            });
        };
        let episode_url = BASE.join(&episode_link).map_err(|error| {
            SourceError::scrape(
                ID,
                format!("the episode link {episode_link:?} is invalid: {error}"),
            )
        })?;

        let episode_html = fetch_text(ctx.fetcher, episode_url.clone()).await?;
        let episode_doc = Html::parse_document(&episode_html);

        // Direct iframe first: a single stream.
        if let Some(iframe) = episode_doc.select(&IFRAME).next()
            && let Some(src) = iframe.value().attr("src")
        {
            let embed = BASE.join(src).map_err(|error| {
                SourceError::scrape(ID, format!("the iframe src {src:?} is invalid: {error}"))
            })?;
            return Ok(PageEmbeds {
                page: episode_url,
                embeds: vec![embed],
            });
        }

        // `data-hash` server iframes: base64 iframe HTML, `src="…"`
        // capture, `&amp;` unescaped, deduped.
        let mut embeds = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for link in episode_doc.select(&SERVER_HASH_LINKS) {
            let Some(hash) = link.value().attr("data-hash") else {
                continue;
            };
            let decoded = decode_base64_lenient(hash);
            let decoded = String::from_utf8_lossy(&decoded);
            let Some(src) = capture_src_attr(&decoded) else {
                continue;
            };
            let src = src.replace("&amp;", "&");
            if !seen.insert(src.clone()) {
                continue;
            }
            if let Ok(embed) = BASE.join(&src) {
                embeds.push(embed);
            }
        }
        Ok(PageEmbeds {
            page: episode_url,
            embeds,
        })
    }

    /// Resolve one embed URL through the registry (upstream: the
    /// resolver routed every source result through the extractor
    /// registry; an extractor error became `[]`).
    async fn extract_embed(&self, ctx: &ResolveCtx<'_>, page: &Url, embed: &Url) -> Vec<Stream> {
        let extract_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: None,
            source_id: Some(ID),
            referer: Some(page),
        };
        self.registry
            .extract(&extract_ctx, embed)
            .await
            .unwrap_or_default()
    }

    /// Every anime page matching the title above the threshold,
    /// best-first: the wp-json REST search first, then the legacy `?s=`
    /// scrape. Ports `fetchAnimePageCandidates`.
    async fn fetch_candidates(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        year: Option<u16>,
    ) -> Result<Vec<Candidate>, SourceError> {
        // Multiple search queries — normalize special characters.
        let queries = candidate_queries(name);

        let wp_matches = self.wp_json_candidates(ctx, &queries, name, year).await?;
        if !wp_matches.is_empty() {
            return Ok(wp_matches);
        }
        self.legacy_search_candidates(ctx, &queries, name, year)
            .await
    }

    /// The `WordPress` REST search: `/wp-json/wp/v2/search` honors the
    /// query (unlike the broken `?s=` page) and ranks exact and partial
    /// title matches properly. All queries merge (the same-title rule
    /// as the legacy loop); a 404 or invalid JSON is the upstream
    /// `catch { continue }`.
    async fn wp_json_candidates(
        &self,
        ctx: &ResolveCtx<'_>,
        queries: &[String],
        name: &str,
        year: Option<u16>,
    ) -> Result<Vec<Candidate>, SourceError> {
        let mut scored: Vec<Candidate> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for query in queries {
            let api_url = Url::parse_with_params(
                "https://9animes.me.uk/wp-json/wp/v2/search",
                &[("search", query.as_str()), ("per_page", "20")],
            )
            .map_err(|error| {
                SourceError::scrape(ID, format!("the wp-json search URL is invalid: {error}"))
            })?;
            let Ok(entries) = fetch_json::<Vec<WpSearchEntry>>(ctx.fetcher, api_url).await else {
                continue;
            };
            for entry in entries {
                let (Some(url), Some(title)) = (entry.url, entry.title) else {
                    continue;
                };
                if !url.contains("/Anime/") || seen.contains(&url) {
                    // Only anime pages; wp-json also returns pages/posts.
                    continue;
                }
                seen.insert(url.clone());
                let score = score_candidate(&title, &url, name, year);
                if score > 0.0 {
                    scored.push(Candidate { href: url, score });
                }
            }
        }
        scored.retain(|candidate| candidate.score >= MIN_CANDIDATE_SCORE);
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(scored)
    }

    /// The legacy `?s=` scrape: the query is currently ignored by the
    /// site, but the scoring below still refuses wrong-title posts.
    /// First query with matches wins (upstream breaks out of the loop).
    async fn legacy_search_candidates(
        &self,
        ctx: &ResolveCtx<'_>,
        queries: &[String],
        name: &str,
        year: Option<u16>,
    ) -> Result<Vec<Candidate>, SourceError> {
        for query in queries {
            let search_url = Url::parse_with_params(BASE.as_str(), &[("s", query.as_str())])
                .map_err(|error| {
                    SourceError::scrape(ID, format!("the search URL is invalid: {error}"))
                })?;
            let Ok(html) = fetch_text(ctx.fetcher, search_url).await else {
                continue;
            };

            let doc = Html::parse_document(&html);
            let mut scored: Vec<Candidate> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for link in doc.select(&ANIME_LINKS) {
                let Some(href) = link.value().attr("href") else {
                    continue;
                };
                if href.contains("/Anime/?")
                    || href.contains("/az-list")
                    || href.contains("/genres/")
                {
                    continue;
                }
                // The <a> tag's text is polluted with status/type
                // labels: walk up to the nearest <article> and use its
                // heading instead.
                let article = link
                    .ancestors()
                    .filter_map(ElementRef::wrap)
                    .find(|element| element.value().name() == "article");
                let text = article
                    .and_then(|article| article.select(&HEADING).next())
                    .map(|heading| heading.text().collect::<String>().trim().to_string())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| link.text().collect::<String>().trim().to_string());
                if text.is_empty() {
                    continue;
                }
                let score = score_candidate(&text, href, name, year);
                if score > 0.0 && seen.insert(href.to_string()) {
                    scored.push(Candidate {
                        href: href.to_string(),
                        score,
                    });
                }
            }

            scored.retain(|candidate| candidate.score >= MIN_CANDIDATE_SCORE);
            scored.sort_by(|a, b| b.score.total_cmp(&a.score));
            if !scored.is_empty() {
                return Ok(scored);
            }
        }
        Ok(Vec::new())
    }
}

#[async_trait]
impl Source for AnimeFlix {
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

/// The shared candidate scorer (used by both search paths).
///
/// Normalizes typographic quotes (`’` → `'`) first, scores exact /
/// ASCII-folded exact / substring-ratio, applies the word-overlap
/// fallback below 50, and adds the year bonus.
fn score_candidate(text: &str, href: &str, name: &str, year: Option<u16>) -> f64 {
    let name_lower = normalize_quotes(name);
    let name_ascii = ascii_fold(name);
    let text_lower = normalize_quotes(text);

    let mut score = 0.0;
    if text_lower == name_lower {
        score = 100.0;
    } else if text_lower == name_ascii {
        score = 95.0;
    } else if text_lower.contains(&name_lower) || name_lower.contains(&text_lower) {
        let short = text_lower.len().min(name_lower.len());
        let long = text_lower.len().max(name_lower.len()).max(1);
        score = ratio(short, long) * 90.0;
    }

    // Word-overlap fallback: TMDB titles that match no site entry
    // exactly (per-arc/per-season pages on anime sites).
    if score < 50.0 {
        let name_words: Vec<&str> = name_lower
            .split_whitespace()
            .filter(|word| word.chars().count() > 2)
            .collect();
        let text_words: Vec<&str> = text_lower
            .split_whitespace()
            .filter(|word| word.chars().count() > 2)
            .collect();
        let common = name_words
            .iter()
            .filter(|word| text_words.contains(word))
            .count();
        if !name_words.is_empty() && common >= name_words.len().min(2) {
            let overlap = ratio(common, name_words.len().max(text_words.len()).max(1));
            if overlap >= 0.5 {
                score = score.max(overlap * 75.0);
            }
        }
    }

    // Bonus for a matching year in the href.
    if score > 0.0
        && let Some(year) = year
        && href.contains(&year.to_string())
    {
        score += 5.0;
    }
    score
}

/// The substring-ratio score: `short / long`, ports the upstream
/// `Math.min(a, b) / Math.max(a, b)` length ratio. Lengths go through
/// `u32` (titles sit far below that bound) so the float conversion
/// stays lossless.
fn ratio(short: usize, long: usize) -> f64 {
    f64::from(u32::try_from(short).unwrap_or_default())
        / f64::from(u32::try_from(long).unwrap_or_default())
}

/// The three upstream query variants (raw, NFD-folded,
/// punctuation-stripped), deduped.
fn candidate_queries(name: &str) -> Vec<String> {
    let folded = ascii_fold(name);
    let alnum = name
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
        .join(" ");
    let mut queries: Vec<String> = Vec::new();
    for query in [name.to_string(), folded, alnum] {
        if !query.is_empty() && !queries.contains(&query) {
            queries.push(query);
        }
    }
    queries
}

/// Lowercase, typographic quotes to ASCII, trim — upstream `norm`.
fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{02bc}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            other => other,
        })
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_string()
}

/// `normalize('NFD')` + strip combining marks + lowercase + trim,
/// approximated with a Latin accent table (the practical TMDB title
/// space for these searches).
fn ascii_fold(s: &str) -> String {
    let folded: String = s
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'ñ' => 'n',
            'ç' => 'c',
            'đ' => 'd',
            'ł' => 'l',
            other => other,
        })
        .collect();
    folded.trim().to_string()
}

/// Boundary-aware episode link test, ports
/// `new RegExp(`episode-${n}(?:/|-|#|$)`).test(href)`: the first
/// `episode-{n}` whose next character is `/`, `-`, `#`, or the end of
/// the href (NOT `episode-50` when episode 5 is wanted).
fn episode_link_matches(href: &str, episode: u32) -> bool {
    let needle = format!("episode-{episode}");
    let mut from = 0;
    while let Some(found) = href[from..].find(&needle) {
        let after = from + found + needle.len();
        match href[after..].chars().next() {
            None | Some('/' | '-' | '#') => return true,
            _ => from = after,
        }
    }
    false
}

/// The first `src="…"` attribute value in `html` — upstream
/// `decoded.match(/src="([^"]+)"/)`.
fn capture_src_attr(html: &str) -> Option<&str> {
    const MARKER: &str = "src=\"";
    let start = html.find(MARKER)? + MARKER.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
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

impl AnimeFlix {
    async fn resolve_by_title(
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
        let year = meta.year;
        let season = if media.kind == MediaType::Series {
            media.season
        } else {
            None
        };

        let candidates = self.fetch_candidates(ctx, name, year).await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let title = match season {
            Some(_) => format!("{name} {}", media.format_season_and_episode()),
            None => match year {
                Some(year) => format!("{name} ({year})"),
                None => name.to_string(),
            },
        };

        // Primary = best-scoring page. If a DISTINCT "-dub" page also
        // scored above threshold, emit it too so sub+dub both ship
        // (the site lists them as separate /Anime/…-dub/ entries).
        let primary = &candidates[0];
        let dub = candidates.iter().find(|candidate| {
            candidate.href != primary.href && candidate.href.to_ascii_lowercase().contains("dub")
        });
        let mut chosen: Vec<&Candidate> = vec![primary];
        if let Some(dub) = dub {
            chosen.push(dub);
        }

        let mut streams = Vec::new();
        for candidate in chosen {
            let is_dub = candidate.href.to_ascii_lowercase().contains("dub");
            let card_title = if is_dub {
                format!("{title} (Dub)")
            } else {
                title.clone()
            };
            // One candidate failing must not kill the other.
            let Ok(embeds) = self
                .collect_from_page(ctx, &candidate.href, season, media.episode)
                .await
            else {
                continue;
            };
            for embed in &embeds.embeds {
                for mut stream in self.extract_embed(ctx, &embeds.page, embed).await {
                    stream.label = Some(card_title.clone());
                    stream.ttl = TTL;
                    stream.meta.languages = if is_dub {
                        vec![CountryCode::Multi, CountryCode::En]
                    } else {
                        vec![CountryCode::Multi, CountryCode::Ja]
                    };
                    stream.meta.dubbed = Some(is_dub);
                    stream.meta.subbed = Some(!is_dub);
                    stream.meta.source_id = Some(ID.to_string());
                    stream.meta.source_label = Some("AnimeFlix".to_string());
                    streams.push(stream);
                }
            }
        }
        Ok(streams)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::{Format, MediaId};
    use vsources_extractors::hosts::megaplay::Megaplay;

    use super::*;

    /// A canned-body matcher: `(host, path)`.
    type PageRule = Box<dyn Fn(&Url) -> bool + Send + Sync>;

    /// A fetcher that serves canned bodies keyed by a URL matcher and
    /// records every request it sees.
    struct ScriptedFetcher {
        pages: Mutex<Vec<(PageRule, String)>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `body` to every request whose URL matches `matches`.
        fn page<F>(self, matches: F, body: impl Into<String>) -> Self
        where
            F: Fn(&Url) -> bool + Send + Sync + 'static,
        {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((Box::new(matches), body.into()));
            self
        }

        /// Every request seen so far, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The value of a header on the first request to `path`.
        fn header_sent_to(&self, path: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .find(|request| request.url.path() == path)
                .and_then(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }
    }

    /// Match a host and path (the query is ignored).
    fn at(host: &'static str, path: &'static str) -> impl Fn(&Url) -> bool {
        move |url| url.host_str() == Some(host) && url.path() == path
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let url = request.url.clone();
            let body = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|(matches, _)| matches(&url))
                .map(|(_, body)| body.clone());
            match body {
                Some(body) => Ok(FetchResponse {
                    url,
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "text/html".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url }),
            }
        }
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

    /// Resolved TMDB metadata for the fixtures.
    fn media_meta(name: &str, year: Option<u16>, season: Option<u32>) -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(1429),
            imdb_id: None,
            name: name.to_string(),
            year,
            season,
            episode: Some(5),
        }
    }

    /// A series episode reference.
    fn series_ref() -> MediaRef {
        MediaRef {
            id: MediaId::Tmdb(1429),
            kind: MediaType::Series,
            season: Some(1),
            episode: Some(5),
        }
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

    /// A provider wired to a registry with the megaplay extractor —
    /// the fixtures' data-hash iframes are megaplay embeds.
    fn provider() -> AnimeFlix {
        AnimeFlix::new(Arc::new(ExtractorRegistry::new(vec![Arc::new(
            Megaplay::new(),
        )])))
    }

    const WP_SEARCH_JSON: &str = r#"[
        {"id": 1, "title": "Attack on Titan", "url": "https://9animes.me.uk/Anime/attack-on-titan/", "type": "post"},
        {"id": 2, "title": "Attack on Titan (Dub)", "url": "https://9animes.me.uk/Anime/attack-on-titan-dub/", "type": "post"},
        {"id": 3, "title": "Sidebar widget", "url": "https://9animes.me.uk/about/", "type": "post"}
    ]"#;

    /// Episodes descending, with an `episode-50` trap before episode 5.
    const ANIME_PAGE: &str = r#"<html><body><article>
        <h2>Attack on Titan</h2>
        <a href="/Anime/attack-on-titan/watch/episode-50/">Episode 50</a>
        <a href="/Anime/attack-on-titan/watch/episode-8/">Episode 8</a>
        <a href="/Anime/attack-on-titan/watch/episode-6/">Episode 6</a>
        <a href="/Anime/attack-on-titan/watch/episode-5/">Episode 5</a>
        <a href="/Anime/attack-on-titan/watch/episode-4/">Episode 4</a>
    </article></body></html>"#;

    /// The dub variant page — same shape, links under the `-dub` path.
    const DUB_ANIME_PAGE: &str = r#"<html><body><article>
        <h2>Attack on Titan</h2>
        <a href="/Anime/attack-on-titan-dub/watch/episode-5/">Episode 5</a>
    </article></body></html>"#;

    /// The data-hash variant of the episode page (two server items: a
    /// duplicate and an `&amp;`-escaped mirror).
    const EPISODE_PAGE_HASHES: &str = r#"<html><body><div class="server-list">
        <div class="server-item"><a data-hash="HASH1"></a></div>
        <div class="server-item"><a data-hash="HASH1"></a></div>
        <div class="server-item"><a data-hash="HASH2"></a></div>
    </div></body></html>"#;

    /// The direct-iframe variant of the episode page.
    const EPISODE_PAGE_IFRAME: &str = r#"<html><body>
        <iframe src="https://megaplay.buzz/stream/ani/16498/5/dub"></iframe>
    </body></html>"#;

    /// The megaplay embed page and API payloads for the extraction chain.
    const MEGAPLAY_EMBED: &str =
        r#"<html><body><div id="player" data-id="987654"></div></body></html>"#;
    const MEGAPLAY_SOURCES: &str = r#"{"sources":{"file":"https://fetch.example/hls/master.m3u8"},"tracks":[{"file":"https://fetch.example/sub.vtt","label":"english","kind":"captions"}]}"#;
    const MEGAPLAY_PLAYLIST: &str =
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1920x1080\nchunklist.m3u8";

    fn assert_audio_category(stream: &Stream, is_dub: bool) {
        assert_eq!(
            (stream.meta.dubbed, stream.meta.subbed),
            (Some(is_dub), Some(!is_dub))
        );
        assert_eq!(
            stream.meta.languages,
            if is_dub {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            }
        );
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_candidates_through_the_registry() -> Result<(), SourceError> {
        let hash1 = b64(br#"<iframe src="https://megaplay.buzz/stream/ani/16498/5/sub"></iframe>"#);
        let hash2 = b64(
            br#"<iframe src="https://megaplay.buzz/stream/ani/16498/5/sub&amp;mirror=2"></iframe>"#,
        );
        let episode_hashes = EPISODE_PAGE_HASHES
            .replace("HASH1", &hash1)
            .replace("HASH2", &hash2);

        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("9animes.me.uk", "/wp-json/wp/v2/search"), WP_SEARCH_JSON)
        .page(at("9animes.me.uk", "/Anime/attack-on-titan/"), ANIME_PAGE)
        .page(
            at("9animes.me.uk", "/Anime/attack-on-titan/watch/episode-5/"),
            episode_hashes,
        )
        .page(
            at("9animes.me.uk", "/Anime/attack-on-titan-dub/"),
            DUB_ANIME_PAGE,
        )
        .page(
            at(
                "9animes.me.uk",
                "/Anime/attack-on-titan-dub/watch/episode-5/",
            ),
            EPISODE_PAGE_IFRAME,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/5/sub"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/5/sub&mirror=2"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/5/dub"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/getSourcesNew"),
            MEGAPLAY_SOURCES,
        )
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        let ctx = ctx(
            &fetcher,
            Some(media_meta("Attack on Titan", Some(2013), Some(1))),
        );
        let streams = provider().resolve(&ctx, &series_ref()).await?;

        // Sub page: two data-hash embeds (the duplicate is deduped);
        // dub page: one direct iframe. One stream per embed.
        assert_eq!(streams.len(), 3, "two sub embeds + one dub iframe");
        let labels: Vec<&str> = streams
            .iter()
            .map(|s| s.label.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Attack on Titan S01E05",
                "Attack on Titan S01E05",
                "Attack on Titan S01E05 (Dub)",
            ]
        );
        for (index, stream) in streams.iter().enumerate() {
            let is_dub = index == 2;
            assert_audio_category(stream, is_dub);
            assert_eq!(stream.meta.source_id.as_deref(), Some(ID));
            assert_eq!(stream.meta.source_label.as_deref(), Some("AnimeFlix"));
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.ttl, TTL);
            assert!(
                stream
                    .url
                    .as_str()
                    .starts_with("https://fetch.example/hls/master.m3u8")
            );
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://megaplay.buzz/")
            );
            assert_eq!(stream.meta.resolution, Some(1080));
        }
        // The megaplay extractor carries the embed URL as the API
        // referer (it infers its own upstream site, ignoring the
        // provider's episode-page referer).
        assert_eq!(
            fetcher
                .header_sent_to("/stream/getSourcesNew", "Referer")
                .as_deref(),
            Some("https://megaplay.buzz/stream/ani/16498/5/sub")
        );
        Ok(())
    }

    #[tokio::test]
    async fn movies_take_the_last_episode_link() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("9animes.me.uk", "/wp-json/wp/v2/search"), WP_SEARCH_JSON)
        .page(at("9animes.me.uk", "/Anime/attack-on-titan/"), ANIME_PAGE)
        .page(
            at("9animes.me.uk", "/Anime/attack-on-titan/watch/episode-4/"),
            EPISODE_PAGE_IFRAME,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/5/dub"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/getSourcesNew"),
            MEGAPLAY_SOURCES,
        )
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        let media = MediaRef::tmdb(1429, MediaType::Movie);
        let ctx = ctx(
            &fetcher,
            Some(ResolvedMedia {
                tmdb_id: Some(1429),
                imdb_id: None,
                name: "Attack on Titan".to_string(),
                year: Some(2013),
                season: None,
                episode: None,
            }),
        );
        let streams = provider().resolve(&ctx, &media).await?;

        // The last episode link is episode-4 (descending DOM order).
        assert!(
            fetcher
                .requests()
                .iter()
                .any(|r| r.url.path() == "/Anime/attack-on-titan/watch/episode-4/")
        );
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("Attack on Titan (2013)"));
        Ok(())
    }

    #[tokio::test]
    async fn legacy_search_falls_back_with_typographic_quotes() -> Result<(), SourceError> {
        // wp-json is down (404): the legacy ?s= scrape must take over,
        // and the ’ in the article heading must match the ' in the name.
        let legacy_page = r#"<html><body>
            <nav><a href="/Anime/?order=popular">Popular</a> <a href="/az-list">A-Z</a> <a href="/genres/action">Action</a></nav>
            <article><h2>Journey’s End</h2><a href="/Anime/journeys-end/">Watch</a></article>
            <article><h2>Unrelated Show</h2><a href="/Anime/unrelated/">Watch</a></article>
        </body></html>"#;

        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("9animes.me.uk", "/"), legacy_page)
        .page(at("9animes.me.uk", "/Anime/journeys-end/"), ANIME_PAGE)
        .page(
            at("9animes.me.uk", "/Anime/attack-on-titan/watch/episode-4/"),
            EPISODE_PAGE_IFRAME,
        )
        .page(
            at("megaplay.buzz", "/stream/ani/16498/5/dub"),
            MEGAPLAY_EMBED,
        )
        .page(
            at("megaplay.buzz", "/stream/getSourcesNew"),
            MEGAPLAY_SOURCES,
        )
        .page(at("fetch.example", "/hls/master.m3u8"), MEGAPLAY_PLAYLIST);

        // A movie reference: the last episode link is episode-4.
        let media = MediaRef::tmdb(9759, MediaType::Movie);
        let ctx = ctx(
            &fetcher,
            Some(ResolvedMedia {
                tmdb_id: Some(9759),
                imdb_id: None,
                name: "Journey's End".to_string(),
                year: Some(2018),
                season: None,
                episode: None,
            }),
        );
        let streams = provider().resolve(&ctx, &media).await?;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label.as_deref(), Some("Journey's End (2018)"));
        assert!(
            fetcher
                .requests()
                .iter()
                .any(|r| r.url.path() == "/Anime/journeys-end/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_candidates_is_an_empty_answer() {
        // Every wp-json query 404s and the legacy search has no
        // /Anime/ links: the upstream returns [].
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(
            at("9animes.me.uk", "/"),
            "<html><body>nothing here</body></html>",
        );

        let ctx = ctx(
            &fetcher,
            Some(media_meta("Ghost Show", Some(2020), Some(1))),
        );
        match provider().resolve(&ctx, &series_ref()).await {
            Ok(streams) => assert!(streams.is_empty(), "no candidates must answer empty"),
            Err(error) => panic!("a candidate-less search is not an error: {error:?}"),
        }
    }

    #[tokio::test]
    async fn missing_media_metadata_is_not_found() {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };
        let ctx = ctx(&fetcher, None);
        match provider().resolve(&ctx, &series_ref()).await {
            Err(SourceError::NotFound) => {}
            other => panic!("without a title there is nothing to search: {other:?}"),
        }
    }

    #[test]
    fn episode_links_match_on_boundaries() {
        assert!(episode_link_matches("/Anime/x/watch/episode-5/", 5));
        assert!(episode_link_matches("/Anime/x/watch/episode-5#top", 5));
        assert!(episode_link_matches("/Anime/x/watch/episode-5-special", 5));
        assert!(!episode_link_matches("/Anime/x/watch/episode-50", 5));
        assert!(episode_link_matches("/Anime/x/watch/episode-50", 50));
    }

    #[test]
    fn base64_decoding_is_lenient_like_buffer_from() {
        assert_eq!(decode_base64_lenient("aGVsbG8="), b"hello");
        assert_eq!(decode_base64_lenient("aGVsbG8"), b"hello");
        // Invalid characters are discarded, not fatal.
        assert_eq!(decode_base64_lenient("a G V s b G 8 !"), b"hello");
    }
}
