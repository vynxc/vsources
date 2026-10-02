//! `AnimeGG`: sub+dub direct MP4/HLS from `animegg.org`.
//!
//! Ports `src/source/AnimeGG.js` (flow verified live upstream, pure
//! `Node.js`, no browser):
//!
//! 1. Search `animegg.org` for the series slug: `/search/?q={name}`
//!    scanned for `/series/{slug}` references. When that comes back
//!    empty, an `AniList` GraphQL search (with Jikan and Kitsu fallbacks)
//!    supplies an alternate title — English or romaji — to retry with.
//! 2. The series page's episode list: root-relative
//!    `/…-episode-{n}` hrefs (the digits must end the href), deduped by
//!    number, ascending.
//! 3. The episode page's iframe embeds `/embed/{id}`: which is sub and
//!    which is dub comes from a ±500-char context window around each
//!    iframe tag (`dubb`/`subb` — double b), with the
//!    first/second/third iframe as the fallback assignment.
//! 4. The embed page's `var videoSources = […]` array — an object
//!    literal that becomes JSON by quoting the keys and single-quoted
//!    values (upstream's two regex fixups, ported as scanners).
//!    Entries yield direct files: `file` (joined onto the site when
//!    relative) or the base64 `bk` backup (percent-encoded), quality
//!    from `label`, HLS when the file path contains `.m3u8`.
//! 5. Both categories are emitted: sub → `multi`/`ja`, dub →
//!    `multi`/`en`.
//!
//! The files only play with `Referer: https://www.animegg.org/`;
//! upstream routed them through a server-side `/proxy` (the `AnimeGG`
//! extractor), this library has no server, so the URLs ship through the
//! `animegg` extractor with the Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's
//!   TMDB resolution: name/year come from `ctx.media`, season/episode
//!   from the [`MediaRef`]. Without `ctx.media` there is no title to
//!   search → [`SourceError::NotFound`].
//! - The JS's own `got-scraping`/`HeaderGenerator` transport collapses
//!   onto the shared fetcher (browser TLS impersonation and the
//!   browser-like headers live there); the JS's Chrome `User-Agent`
//!   string is cut for the same reason.
//! - `meta.title` becomes the stream label, `meta.countryCodes` become
//!   `meta.languages`, `meta.height` becomes `meta.resolution` from the
//!   quality label (1080/720/480/360). `this.ttl` (10min) is the
//!   stream TTL.
//! - The `animegg` extractor hardcodes `Format::Mp4` (an upstream
//!   proxy-era artifact); the provider re-states the format it parsed
//!   from the file path (`.m3u8` → HLS), which is the one deviation
//!   from the upstream's final stream shape.
//! - Cut: the JS's `console.log` diagnostics (no logging facade in
//!   this crate) and the per-source result cache (the parent's
//!   `CachedSource` owns it).
//! - Patterns are ported as `scraper` selectors and byte scanners
//!   (this crate has no regex engine); NFD folding is approximated
//!   with a Latin accent table.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, Format, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id (upstream `this.id`).
const ID: &str = "animegg";
/// Upstream result lifetime: 10min.
const TTL: Duration = Duration::from_mins(10);

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://www.animegg.org")
        .unwrap_or_else(|_| panic!("the AnimeGG base URL must parse"))
});
static ANILIST_GQL: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://graphql.anilist.co")
        .unwrap_or_else(|_| panic!("the AniList GraphQL endpoint must parse"))
});

static ALL_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a").unwrap_or_else(|_| panic!("valid a selector")));

/// The `AniList` GraphQL query (upstream `query`).
const ANILIST_QUERY: &str = r"
    query($search: String) {
      Page(page: 1, perPage: 5) {
        media(type: ANIME, search: $search, sort: [SEARCH_MATCH, POPULARITY_DESC]) {
          id idMal title { romaji english } format seasonYear
        }
      }
    }";

/// A search hit: a series slug and its dashed title.
struct SeriesHit {
    /// The series slug.
    slug: String,
    /// The slug with dashes as spaces (the site has no titles).
    title: String,
}

/// An episode link from the series page.
struct EpisodeLink {
    /// The episode page slug (e.g. `one-piece-episode-5`).
    slug: String,
    /// The episode number.
    number: u32,
}

/// One entry of the embed page's `videoSources` array.
#[derive(Deserialize)]
struct VideoSource {
    /// The direct file path (absolute or site-relative).
    #[serde(default)]
    file: Option<String>,
    /// The quality label (e.g. `1080p`).
    #[serde(default)]
    label: Option<String>,
    /// The percent-encoded base64 backup URL.
    #[serde(default)]
    bk: Option<String>,
}

/// A parsed direct file, before extraction.
struct RawStream {
    /// The file URL.
    url: Url,
    /// The embed page the file was found on (the extraction referer).
    page: Url,
    /// The quality label (upstream `s.label || 'unknown'`).
    quality: String,
    /// Whether the file path contains `.m3u8` (upstream type).
    is_hls: bool,
}

/// A title-bearing media entry from AniList/Jikan/Kitsu (only the
/// titles are used — the alternate-title retry).
struct TitleMedia {
    /// The English title.
    english: Option<String>,
    /// The romaji title.
    romaji: Option<String>,
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

/// The Jikan API response.
#[derive(Deserialize)]
struct JikanResponse {
    /// The matched anime.
    #[serde(default)]
    data: Vec<JikanAnime>,
}

/// One Jikan anime entry.
#[derive(Deserialize)]
struct JikanAnime {
    /// The main title.
    #[serde(default)]
    title: Option<String>,
    /// The English title.
    #[serde(default)]
    title_english: Option<String>,
    /// The Japanese title (maps to romaji).
    #[serde(default)]
    title_japanese: Option<String>,
}

/// The Kitsu API response.
#[derive(Deserialize)]
struct KitsuResponse {
    /// The matched anime.
    #[serde(default)]
    data: Vec<KitsuAnime>,
}

/// One Kitsu anime entry.
#[derive(Deserialize)]
struct KitsuAnime {
    /// The entry attributes.
    #[serde(default)]
    attributes: Option<KitsuAttributes>,
}

/// The Kitsu attributes block.
#[derive(Deserialize)]
struct KitsuAttributes {
    /// The titles map.
    #[serde(default)]
    titles: Option<KitsuTitles>,
    /// The canonical title.
    #[serde(default)]
    canonical_title: Option<String>,
}

/// The Kitsu titles map.
#[derive(Deserialize)]
struct KitsuTitles {
    /// The romanized `en_jp` title.
    #[serde(default)]
    #[serde(rename = "en_jp")]
    en_jp: Option<String>,
    /// The English title.
    #[serde(default)]
    en: Option<String>,
}

/// The `AnimeGG` provider.
pub struct AnimeGG {
    /// Shared anime identity mappings, when configured.
    mappings: Option<vsources_core::mappings::MappingService>,
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that claims the direct file URLs.
    registry: Arc<ExtractorRegistry>,
}

impl AnimeGG {
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
                label: "AnimeGG".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Fetch the direct files of one audio category for an episode,
    /// ports `getEpisodeStreams`: the episode page is fetched per
    /// category (like the JS), and each file remembers the embed page
    /// it was parsed from.
    async fn episode_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        episode_slug: &str,
        category: &str,
    ) -> Result<Vec<RawStream>, SourceError> {
        let episode_url = BASE.join(episode_slug).map_err(|error| {
            SourceError::scrape(
                ID,
                format!("the episode URL {episode_slug:?} is invalid: {error}"),
            )
        })?;
        let site_referer = BASE.join("/").map_err(|error| {
            SourceError::scrape(ID, format!("the site referer is invalid: {error}"))
        })?;
        let Some(html) = fetch_html(ctx, &episode_url, &site_referer).await else {
            // Upstream skips the category when the page is missing.
            return Ok(Vec::new());
        };

        // Find all iframe embed ids and their sub/dub context.
        let iframes = scan_embed_iframes(&html);
        if iframes.is_empty() {
            return Ok(Vec::new());
        }
        let (sub_embed, dub_embed) = classify_embeds(&html, &iframes);
        // Fallback: first embed = sub, second (or third) = dub.
        let sub_embed = sub_embed.or_else(|| iframes.first().map(|(_, id)| id.clone()));
        let dub_embed = dub_embed
            .or_else(|| iframes.get(1).map(|(_, id)| id.clone()))
            .or_else(|| iframes.get(2).map(|(_, id)| id.clone()));

        // Target embeds: the dub probe also includes the third iframe.
        let targets: Vec<String> = if category == "dub" {
            let mut ids: Vec<String> = Vec::new();
            for id in dub_embed
                .into_iter()
                .chain(iframes.get(2).map(|(_, id)| id.clone()))
            {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            ids
        } else {
            sub_embed.into_iter().collect()
        };
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let embed_referer = BASE.join(episode_slug).map_err(|error| {
            SourceError::scrape(
                ID,
                format!("the embed referer {episode_slug:?} is invalid: {error}"),
            )
        })?;
        let mut streams = Vec::new();
        for embed_id in targets {
            let embed_url = BASE.join(&format!("embed/{embed_id}")).map_err(|error| {
                SourceError::scrape(ID, format!("the embed URL is invalid: {error}"))
            })?;
            let Some(embed_html) = fetch_html(ctx, &embed_url, &embed_referer).await else {
                continue;
            };
            let Some(sources) = video_sources_array(&embed_html)
                .map(json_ify)
                .and_then(|json| serde_json::from_str::<Vec<VideoSource>>(&json).ok())
            else {
                continue;
            };
            for source in sources {
                // `bk` is base64 of a percent-encoded URL.
                let backup = source.bk.as_deref().map(|bk| {
                    let decoded = decode_base64_lenient(bk);
                    percent_decode(&String::from_utf8_lossy(&decoded))
                });
                let file = source.file.unwrap_or_default();
                // `is_hls` is read off `file` before it is moved into
                // `url` (upstream: `source.file?.includes('.m3u8')`).
                let is_hls = file.contains(".m3u8");
                let url = if file.is_empty() {
                    backup.unwrap_or_default()
                } else if file.starts_with("http") {
                    file
                } else {
                    format!("{}{}", BASE.as_str().trim_end_matches('/'), file)
                };
                if url.is_empty() {
                    continue;
                }
                let quality = source.label.unwrap_or_else(|| "unknown".to_string());
                if let Ok(parsed) = Url::parse(&url) {
                    streams.push(RawStream {
                        url: parsed,
                        page: embed_url.clone(),
                        quality,
                        is_hls,
                    });
                }
            }
        }
        Ok(streams)
    }

    /// Resolve one direct file URL through the registry and tag it
    /// with the provider's metadata.
    async fn resolve_raw(
        &self,
        ctx: &ResolveCtx<'_>,
        raw: &RawStream,
        title_base: &str,
        is_dub: bool,
    ) -> Vec<Stream> {
        let extract_ctx = ResolveCtx {
            fetcher: ctx.fetcher,
            media: None,
            source_id: Some(ID),
            referer: Some(&raw.page),
        };
        let Ok(extracted) = self.registry.extract(&extract_ctx, &raw.url).await else {
            return Vec::new();
        };
        extracted
            .into_iter()
            .map(|mut stream| {
                // The animegg extractor hardcodes Mp4; the provider's
                // own parse of the file path is authoritative.
                stream.format = if raw.is_hls { Format::Hls } else { Format::Mp4 };
                stream.label = Some(format!(
                    "{title_base} (AnimeGG {} {})",
                    raw.quality,
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
                stream.meta.source_label = Some("AnimeGG".to_string());
                if let Some(height) = height_of(&raw.quality) {
                    stream.meta.resolution = Some(height);
                }
                stream
            })
            .collect()
    }
}

#[async_trait]
impl Source for AnimeGG {
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

/// Fetch a page as HTML — the upstream `fetchText` (a Referer when the
/// JS passes one; non-200 answers become `None`).
async fn fetch_html(ctx: &ResolveCtx<'_>, url: &Url, referer: &Url) -> Option<String> {
    let request = FetchRequest::get(url.clone())
        .with_header("Accept", "text/html")
        .with_header("Referer", referer.as_str())
        .with_timeout(Duration::from_secs(12));
    let response = ctx.fetcher.request(request).await.ok()?;
    let body = response.body;
    (!body.is_empty()).then_some(body)
}

/// The site base as the default referer.
fn base_referer() -> Url {
    BASE.join("/")
        .unwrap_or_else(|_| panic!("the base referer must join"))
}

/// Search the site for series slugs, ports `searchSeries`: the
/// `/search/?q=` page scanned for `/series/{slug}` (stopping at a
/// quote, slash, `?`, or `#`), deduped, title = slug dashes as spaces.
/// The JS search passes no Referer; the site referer keeps the request
/// shape the shared fetcher expects.
async fn search_series(ctx: &ResolveCtx<'_>, query: &str) -> Result<Vec<SeriesHit>, SourceError> {
    let search_url = Url::parse_with_params("https://www.animegg.org/search/", &[("q", query)])
        .map_err(|error| SourceError::scrape(ID, format!("the search URL is invalid: {error}")))?;
    let Some(html) = fetch_html(ctx, &search_url, &base_referer()).await else {
        return Ok(Vec::new());
    };
    Ok(scan_series_slugs(&html))
}

/// Scan `/series/{slug}` references out of raw HTML, ports
/// `html.match(/\/series\/([^"'/?#]+)/g)` — first-seen order, slugs
/// longer than 2 chars.
fn scan_series_slugs(html: &str) -> Vec<SeriesHit> {
    let mut slugs: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(found) = html[from..].find("/series/") {
        let start = from + found + "/series/".len();
        let rest = &html[start..];
        let end = rest.find(['"', '\'', '/', '?', '#']).unwrap_or(rest.len());
        let slug = &rest[..end];
        if slug.chars().count() > 2 && !slugs.iter().any(|seen| seen == slug) {
            slugs.push(slug.to_string());
        }
        from = start + end.max(1);
    }
    slugs
        .into_iter()
        .map(|slug| {
            let title = slug.replace('-', " ");
            SeriesHit { slug, title }
        })
        .collect()
}

/// The episode list of a series page, ports `getEpisodes`:
/// root-relative hrefs ending in `-episode-{n}` (the digits must end
/// the href; hrefs with `?` never match), deduped by number, ascending.
async fn series_episodes(
    ctx: &ResolveCtx<'_>,
    slug: &str,
) -> Result<Vec<EpisodeLink>, SourceError> {
    let series_url = BASE
        .join(&format!("series/{slug}"))
        .map_err(|error| SourceError::scrape(ID, format!("the series URL is invalid: {error}")))?;
    let Some(html) = fetch_html(ctx, &series_url, &base_referer()).await else {
        return Ok(Vec::new());
    };
    Ok(scan_episode_links(&html))
}

/// Scan episode links out of the series page, ports
/// `href=["']\/([^"'?]*-episode-(\d+))["']` (case-insensitive — the
/// ASCII-lowered copy locates the marker, the original is sliced at
/// the same byte offsets).
fn scan_episode_links(html: &str) -> Vec<EpisodeLink> {
    let doc = Html::parse_document(html);
    let mut episodes: Vec<EpisodeLink> = Vec::new();
    for link in doc.select(&ALL_LINKS) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.starts_with('/') || href.contains('?') {
            continue;
        }
        let lowered = href.to_ascii_lowercase();
        let Some(pos) = lowered.rfind("-episode-") else {
            continue;
        };
        let digits = &href[pos + "-episode-".len()..];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(number) = digits.parse() else {
            continue;
        };
        if episodes.iter().any(|episode| episode.number == number) {
            continue;
        }
        // Strip only the leading '/': the JS capture group
        // `([^"'?]*-episode-(\d+))` includes the `-episode-N`
        // suffix, and the episode URL is `/{slug}` verbatim.
        episodes.push(EpisodeLink {
            slug: href[1..].to_string(),
            number,
        });
    }
    episodes.sort_by_key(|episode| episode.number);
    episodes
}

/// The iframe embed ids of an episode page, ports
/// `<iframe[^>]+src=["']\/embed\/(\d+)["']` (case-insensitive):
/// `(tag start, embed id)` pairs in document order.
fn scan_embed_iframes(html: &str) -> Vec<(usize, String)> {
    let lowered = html.to_ascii_lowercase();
    let mut iframes = Vec::new();
    let mut from = 0;
    while let Some(found) = lowered[from..].find("<iframe") {
        let tag_start = from + found;
        let Some(tag_end) = lowered[tag_start..].find('>').map(|end| tag_start + end) else {
            break;
        };
        for quote in ['"', '\''] {
            let needle = format!("src={quote}");
            let mut scan = tag_start;
            while scan < tag_end {
                let Some(rel) = lowered[scan..tag_end].find(&needle) else {
                    break;
                };
                let value_start = scan + rel + needle.len();
                if html[value_start..].starts_with("/embed/") {
                    let digits_start = value_start + "/embed/".len();
                    let digits = leading_digits(&html[digits_start..]);
                    if !digits.is_empty() && html[digits_start + digits.len()..].starts_with(quote)
                    {
                        iframes.push((tag_start, digits.to_string()));
                    }
                }
                scan = value_start;
            }
        }
        from = tag_end;
    }
    iframes
}

/// Decide which embed is sub and which is dub from ±500-char context
/// windows around each iframe tag, ports the upstream loop: the
/// context (lowercased) must contain `dubb`/`subb` — double b — and
/// the else-if chain means an already-assigned dub falls through to
/// the sub check.
fn classify_embeds(html: &str, iframes: &[(usize, String)]) -> (Option<String>, Option<String>) {
    let mut sub_embed: Option<String> = None;
    let mut dub_embed: Option<String> = None;
    for (tag_start, embed_id) in iframes {
        let before = &html[tag_start.saturating_sub(500)..*tag_start];
        let after_end = (*tag_start + 500).min(html.len());
        let after = &html[*tag_start..after_end];
        let context = format!("{before}{after}").to_ascii_lowercase();
        if context.contains("dubb") && dub_embed.is_none() {
            dub_embed = Some(embed_id.clone());
        } else if context.contains("subb") && sub_embed.is_none() {
            sub_embed = Some(embed_id.clone());
        }
    }
    (sub_embed, dub_embed)
}

/// The `var videoSources = […]` array text, ports
/// `embedHtml.match(/var\s+videoSources\s*=\s*(\[[\s\S]*?\]);/)` — the
/// lazy capture up to the first `];`.
fn video_sources_array(html: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(found) = html[from..].find("var") {
        let after_var = from + found + "var".len();
        let skipped = html[after_var..].len() - html[after_var..].trim_start().len();
        let ident = after_var + skipped;
        if html[ident..].starts_with("videoSources") {
            let after_ident = ident + "videoSources".len();
            let skipped = html[after_ident..].len() - html[after_ident..].trim_start().len();
            let assign = after_ident + skipped;
            if html[assign..].starts_with('=') {
                let after_assign = assign + 1;
                let skipped = html[after_assign..].len() - html[after_assign..].trim_start().len();
                let array = after_assign + skipped;
                if html[array..].starts_with('[') {
                    // Lazy up to the first `];`.
                    let semi = html[array..].find("];")?;
                    // +1: the capture `\[[\s\S]*?\]` includes the
                    // closing bracket.
                    return Some(&html[array..=array + semi]);
                }
            }
        }
        from = after_var;
    }
    None
}

/// Make a JS object literal JSON-parseable, ports the two upstream
/// regex fixups: quote the unquoted keys
/// (`([{,]\s*)([a-zA-Z_]\w*)\s*:`) and single-quoted values
/// (`:\s*'([^']*)'`).
fn json_ify(text: &str) -> String {
    quote_single_values(&quote_js_keys(text))
}

/// Quote the object-literal keys: an identifier directly after `{` or
/// `,` (whitespace allowed) that is followed by `:` gets quotes.
fn quote_js_keys(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 16);
    let mut i = 0;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(quote) = in_string {
            out.push(byte);
            if byte == b'\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i]);
            } else if byte == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => {
                in_string = Some(byte);
                out.push(byte);
                i += 1;
            }
            b'{' | b',' => {
                out.push(byte);
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    out.push(bytes[i]);
                    i += 1;
                }
                if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                    let ident_start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    let ident = &bytes[ident_start..i];
                    let mut j = i;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b':' {
                        // `$1"$2":` — the ident gets quotes; the
                        // whitespace and colon are copied verbatim.
                        out.push(b'"');
                        out.extend_from_slice(ident);
                        out.push(b'"');
                    } else {
                        out.extend_from_slice(ident);
                    }
                }
            }
            _ => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Rewrite `: 'value'` to `: "value"`, ports
/// `text.replace(/:\s*'([^']*)'/g, ': "$1"')`.
fn quote_single_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len()
                && bytes[j] == b'\''
                && let Some(rel_end) = bytes[j + 1..].iter().position(|&b| b == b'\'')
            {
                let value_end = j + 1 + rel_end;
                out.push(b':');
                out.push(b' ');
                out.push(b'"');
                out.extend_from_slice(&bytes[j + 1..value_end]);
                out.push(b'"');
                i = value_end + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The vertical resolution a quality label advertises (upstream
/// `includes` checks for 1080/720/480/360).
fn height_of(quality: &str) -> Option<u16> {
    [1080, 720, 480, 360]
        .into_iter()
        .find(|&height| quality.contains(&height.to_string()))
}

/// Resolve alternate titles by searching `AniList` (with Jikan and Kitsu
/// fallbacks), ports `resolveAniList` — each hop answers `[]` when it
/// is down, and the JS falls through.
async fn resolve_title_media(ctx: &ResolveCtx<'_>, name: &str) -> Vec<TitleMedia> {
    // AniList GraphQL.
    let body =
        serde_json::json!({"query": ANILIST_QUERY, "variables": {"search": name}}).to_string();
    let request = FetchRequest::post(ANILIST_GQL.clone(), body)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json");
    if let Ok(response) = ctx.fetcher.request(request).await
        && let Ok(data) = response.json::<AniListResponse>()
        && let Some(media) = data.data.and_then(|data| data.page).map(|page| page.media)
        && !media.is_empty()
    {
        return media
            .into_iter()
            .map(|entry| {
                let title = entry.title.unwrap_or(AniListTitle {
                    romaji: None,
                    english: None,
                });
                TitleMedia {
                    english: title.english,
                    romaji: title.romaji,
                }
            })
            .collect();
    }

    // Jikan (MyAnimeList wrapper).
    let jikan_url = Url::parse_with_params(
        "https://api.jikan.moe/v4/anime",
        &[("q", name), ("limit", "5"), ("sfw", "true")],
    )
    .ok();
    if let Some(jikan_url) = jikan_url
        && let Ok(response) = ctx.fetcher.request(FetchRequest::get(jikan_url)).await
        && let Ok(data) = response.json::<JikanResponse>()
        && !data.data.is_empty()
    {
        return data
            .data
            .into_iter()
            .map(|anime| {
                let title = anime.title;
                TitleMedia {
                    english: anime.title_english.or(title.clone()),
                    romaji: anime.title_japanese.or(title),
                }
            })
            .collect();
    }

    // Kitsu.
    let kitsu_url = Url::parse_with_params(
        "https://kitsu.app/api/edge/anime",
        &[("filter[text]", name), ("page[limit]", "5")],
    )
    .ok();
    if let Some(kitsu_url) = kitsu_url
        && let Ok(response) = ctx.fetcher.request(FetchRequest::get(kitsu_url)).await
        && let Ok(data) = response.json::<KitsuResponse>()
        && !data.data.is_empty()
    {
        return data
            .data
            .into_iter()
            .map(|anime| {
                let attributes = anime.attributes.unwrap_or(KitsuAttributes {
                    titles: None,
                    canonical_title: None,
                });
                let titles = attributes.titles.unwrap_or(KitsuTitles {
                    en_jp: None,
                    en: None,
                });
                let canonical = attributes.canonical_title;
                TitleMedia {
                    english: titles.en.or(canonical.clone()),
                    romaji: titles.en_jp.or(canonical),
                }
            })
            .collect();
    }

    Vec::new()
}

/// Normalize for the best-match comparison, ports the upstream
/// `normalize`: lowercase, NFD-folded (approximated with a Latin
/// accent table), non-alphanumerics REMOVED (not spaced), collapsed.
fn normalize(s: &str) -> String {
    let folded = ascii_fold(s);
    folded
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `normalize('NFD')` + strip combining marks + lowercase, approximated
/// with a Latin accent table (the practical title space for these
/// searches).
fn ascii_fold(s: &str) -> String {
    s.chars()
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
        .collect()
}

/// Percent-decode, ports `decodeURIComponent` (invalid escapes pass
/// through — the JS would throw and the entry is skipped upstream, a
/// lenient decode keeps the good ones).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) =
                (hex_value(bytes.get(i + 1)), hex_value(bytes.get(i + 2)))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of one hex digit byte.
fn hex_value(byte: Option<&u8>) -> Option<u8> {
    match byte.copied().unwrap_or_default() {
        b'0'..=b'9' => Some(byte.copied().unwrap_or_default() - b'0'),
        b'a'..=b'f' => Some(byte.copied().unwrap_or_default() - b'a' + 10),
        b'A'..=b'F' => Some(byte.copied().unwrap_or_default() - b'A' + 10),
        _ => None,
    }
}

/// The leading ASCII digit run of `s`.
fn leading_digits(s: &str) -> &str {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    &s[..end]
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

impl AnimeGG {
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
        let name_norm = normalize(name);
        let ep_num = season.map_or(1, |_| media.episode.unwrap_or(1));

        // Step 1: search by title (no AniList needed).
        let mut hits = search_series(ctx, name).await?;
        if hits.is_empty() {
            // AniList alternate title retry: the English or romaji
            // title may match the site better.
            for alt in resolve_title_media(ctx, name).await {
                let Some(alt_title) = alt.english.or(alt.romaji) else {
                    continue;
                };
                if alt_title == name {
                    continue;
                }
                hits = search_series(ctx, &alt_title).await?;
                if !hits.is_empty() {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        // Pick the best match: the first hit, unless another normalizes
        // to exactly the title.
        let mut best_slug = hits[0].slug.clone();
        for hit in &hits {
            if normalize(&hit.title) == name_norm {
                best_slug = hit.slug.clone();
                break;
            }
        }

        // Step 2: get the episode list.
        let episodes = series_episodes(ctx, &best_slug).await?;
        let Some(episode) = episodes
            .iter()
            .find(|episode| episode.number == ep_num)
            .or_else(|| episodes.first())
        else {
            return Ok(Vec::new());
        };

        // Step 3: streams for both sub and dub.
        let mut streams = Vec::new();
        let mut seen: HashSet<Url> = HashSet::new();
        for category in ["sub", "dub"] {
            let Ok(raw) = self.episode_streams(ctx, &episode.slug, category).await else {
                continue;
            };
            for stream in raw {
                if !seen.insert(stream.url.clone()) {
                    continue;
                }
                let mut resolved = self
                    .resolve_raw(ctx, &stream, &title_base, category == "dub")
                    .await;
                streams.append(&mut resolved);
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
    use vsources_core::types::MediaId;
    use vsources_extractors::hosts::animegg::AnimeGG as AnimeGGExtractor;

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

    /// Match a host, path, and query substring.
    fn at_query(
        host: &'static str,
        path: &'static str,
        needle: &'static str,
    ) -> impl Fn(&Url) -> bool {
        move |url| {
            url.host_str() == Some(host)
                && url.path() == path
                && url.query().is_some_and(|query| query.contains(needle))
        }
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

    /// A series episode reference with resolved TMDB metadata.
    fn one_piece() -> (MediaRef, ResolvedMedia) {
        (
            MediaRef {
                id: MediaId::Tmdb(37854),
                kind: MediaType::Series,
                season: Some(1),
                episode: Some(5),
            },
            ResolvedMedia {
                tmdb_id: Some(37854),
                imdb_id: None,
                name: "One Piece".to_string(),
                year: Some(1999),
                season: Some(1),
                episode: Some(5),
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

    /// A provider wired to a registry with the animegg passthrough
    /// extractor (it attaches the site Referer).
    fn provider() -> AnimeGG {
        AnimeGG::new(Arc::new(ExtractorRegistry::new(vec![Arc::new(
            AnimeGGExtractor::new(),
        )])))
    }

    const SEARCH_PAGE: &str =
        r#"<html><body><a href="/series/one-piece">One Piece</a></body></html>"#;

    const SERIES_PAGE: &str = r#"<html><body>
        <a href="/one-piece-episode-5">Episode 5</a>
        <a href="/one-piece-episode-4">Episode 4</a>
    </body></html>"#;

    /// The sub embed (context "subb") and the dub embed (context
    /// "dubb") — separated by more than the ±500-char classification
    /// window, so each iframe's context holds only its own marker
    /// (in a small page both markers land in every window and the
    /// upstream else-if chain assigns the first iframe to dub).
    fn episode_page() -> String {
        format!(
            r#"<html><body>
        <div class="episode-links">
            <div class="subbed"><iframe src="/embed/1001"></iframe></div>{filler}<div class="dubbed"><iframe src='/embed/1002'></iframe></div>
        </div>
    </body></html>"#,
            filler = "<p>filler</p>".repeat(150)
        )
    }

    /// The sub embed page: a relative mp4, an m3u8, and a `bk` backup
    /// (no `file`).
    const SUB_EMBED_PAGE: &str = r#"<html><script>
        var videoSources = [{file:"/play/1001/video.mp4?for=x",label:"1080p"},{file:"/play/1001/hls/index.m3u8",label:"720p"},{bk:"BACKUP",label:"480p"}];
    </script></html>"#;

    /// The dub embed page: one absolute mp4.
    const DUB_EMBED_PAGE: &str = r#"<html><script>
        var videoSources = [{file:"https://www.animegg.org/play/1002/dub.mp4",label:'1080p'}];
    </script></html>"#;

    const ANILIST_JSON: &str = r#"{"data":{"Page":{"media":[
        {"id":21,"idMal":21,"title":{"romaji":"One Piece","english":"One Piece"},"format":"TV","seasonYear":1999}
    ]}}}"#;

    #[test]
    fn series_slugs_scan_out_of_raw_html() {
        let hits =
            scan_series_slugs(r#"<a href="/series/one-piece">One</a> /series/one-piece-movie"#);
        let slugs: Vec<&str> = hits.iter().map(|hit| hit.slug.as_str()).collect();
        assert_eq!(slugs, vec!["one-piece", "one-piece-movie"]);
        // Deduped, and 2-char slugs are dropped.
        assert_eq!(scan_series_slugs("/series/ab /series/ab").len(), 1);
        assert_eq!(scan_series_slugs("/series/ab").len(), 0);
    }

    #[test]
    fn episode_links_scan_with_digits_at_the_end() {
        let episodes = scan_episode_links(
            r#"<a href="/one-piece-episode-5">5</a><a href="/one-piece-episode-50">50</a><a href="/one-piece-episode-5x">5x</a><a href="https://x/one-piece-episode-6">6</a>"#,
        );
        assert_eq!(
            episodes.len(),
            2,
            "5, 50 — the 5x and absolute hrefs never match"
        );
        assert_eq!(episodes[0].slug, "one-piece-episode-5");
        assert_eq!(episodes[0].number, 5);
        assert_eq!(episodes[1].number, 50);
    }

    #[test]
    fn embed_iframes_classify_by_context() {
        // The upstream window is ±500 chars; the markers must sit
        // further apart than that, or the else-if chain (dub first)
        // would hand the first iframe to dub.
        let html = format!(
            r#"<div class="subbed"><iframe src="/embed/1001"></iframe></div>{}<div class="dubbed"><iframe src='/embed/1002'></iframe></div>"#,
            "<p>filler</p>".repeat(150)
        );
        let iframes = scan_embed_iframes(&html);
        assert_eq!(iframes.len(), 2);
        let (sub, dub) = classify_embeds(&html, &iframes);
        assert_eq!(sub.as_deref(), Some("1001"));
        assert_eq!(dub.as_deref(), Some("1002"));
    }

    #[test]
    fn video_sources_become_json() {
        let array = r#"[{file:'/play/1.mp4',label:'1080p'},{file:'/play/2.m3u8', label: "720p"}]"#;
        let parsed: Vec<VideoSource> = serde_json::from_str(&json_ify(array))
            .unwrap_or_else(|error| panic!("the fixed-up array must parse: {error}"));
        assert_eq!(parsed[0].file.as_deref(), Some("/play/1.mp4"));
        assert_eq!(parsed[1].label.as_deref(), Some("720p"));
    }

    #[test]
    fn percent_decode_mirrors_decode_uri_component() {
        assert_eq!(percent_decode("https%3A%2F%2Fx%2Ff.mp4"), "https://x/f.mp4");
        // Invalid escapes pass through (the JS would drop the entry).
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_direct_files() -> Result<(), SourceError> {
        // The bk backup is base64 of a percent-encoded URL.
        let backup = b64(b"https%3A%2F%2Fwww.animegg.org%2Fplay%2F1001%2Ffallback.mp4");
        let sub_embed = SUB_EMBED_PAGE.replace("BACKUP", &backup);

        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("www.animegg.org", "/search/"), SEARCH_PAGE)
        .page(at("www.animegg.org", "/series/one-piece"), SERIES_PAGE)
        .page(
            at("www.animegg.org", "/one-piece-episode-5"),
            episode_page(),
        )
        .page(at("www.animegg.org", "/embed/1001"), sub_embed)
        .page(at("www.animegg.org", "/embed/1002"), DUB_EMBED_PAGE);

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        // Sub: three files (relative mp4, m3u8, bk backup); dub: one.
        assert_eq!(streams.len(), 4);
        let labels: Vec<&str> = streams
            .iter()
            .map(|s| s.label.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            labels,
            vec![
                "One Piece S01E05 (AnimeGG 1080p SUB)",
                "One Piece S01E05 (AnimeGG 720p SUB)",
                "One Piece S01E05 (AnimeGG 480p SUB)",
                "One Piece S01E05 (AnimeGG 1080p DUB)",
            ]
        );
        assert_eq!(
            streams[0].url.as_str(),
            "https://www.animegg.org/play/1001/video.mp4?for=x"
        );
        assert_eq!(
            streams[1].url.as_str(),
            "https://www.animegg.org/play/1001/hls/index.m3u8"
        );
        assert_eq!(
            streams[2].url.as_str(),
            "https://www.animegg.org/play/1001/fallback.mp4"
        );
        assert_eq!(
            streams[3].url.as_str(),
            "https://www.animegg.org/play/1002/dub.mp4"
        );
        // The provider's own format parse (the extractor hardcodes
        // Mp4 — see the module doc).
        assert_eq!(streams[0].format, Format::Mp4);
        assert_eq!(streams[1].format, Format::Hls);
        for stream in &streams {
            assert_eq!(stream.ttl, TTL);
            assert_eq!(stream.meta.source_id.as_deref(), Some(ID));
            // The hotlink Referer upstream's proxy hop sent.
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://www.animegg.org/")
            );
            let expected_languages = if stream.label.as_deref().is_some_and(|l| l.ends_with("DUB)"))
            {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            };
            assert_eq!(stream.meta.languages, expected_languages);
        }
        // The resolutions came from the quality labels.
        assert_eq!(streams[0].meta.resolution, Some(1080));
        assert_eq!(streams[1].meta.resolution, Some(720));
        assert_eq!(streams[2].meta.resolution, Some(480));
        // The embed page carried the episode page as its Referer.
        assert_eq!(
            fetcher.header_sent_to("/embed/1001", "Referer").as_deref(),
            Some("https://www.animegg.org/one-piece-episode-5")
        );
        // The episode page carried the site as its Referer.
        assert_eq!(
            fetcher
                .header_sent_to("/one-piece-episode-5", "Referer")
                .as_deref(),
            Some("https://www.animegg.org/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn retries_with_the_anilist_alternate_title() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        // The raw-name search finds nothing…
        .page(at_query("www.animegg.org", "/search/", "q=One+Piece"), "<html></html>")
        // …AniList supplies the alternate title…
        .page(at("graphql.anilist.co", "/"), ANILIST_JSON)
        // …which is then searched on the site.
        .page(
            at_query("www.animegg.org", "/search/", "q=One+Piece"),
            r#"<html><a href="/series/one-piece">One Piece</a></html>"#,
        );

        let (media, mut meta) = one_piece();
        meta.name = "One Piece: Alternate".to_string();
        let ctx = ctx(&fetcher, Some(meta));
        // The full chain is not scripted past the search: an empty
        // answer proves the alternate-title retry ran (the AniList POST
        // only happens on the retry path).
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(streams.is_empty());
        assert!(
            fetcher
                .requests()
                .iter()
                .any(|request| request.url.host_str() == Some("graphql.anilist.co")),
            "the AniList retry must have run"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_search_hits_anywhere_is_an_empty_answer() -> Result<(), SourceError> {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(
            at("www.animegg.org", "/search/"),
            "<html><body>nothing</body></html>",
        );

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(streams.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_media_metadata_is_not_found() {
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };
        let (media, _) = one_piece();
        let ctx = ctx(&fetcher, None);
        match provider().resolve(&ctx, &media).await {
            Err(SourceError::NotFound) => {}
            other => panic!("without a title there is nothing to search: {other:?}"),
        }
    }
}
