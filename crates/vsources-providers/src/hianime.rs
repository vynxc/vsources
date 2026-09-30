//! `HiAnime`: sub+dub anime HLS from `hianime.at`.
//!
//! Ports `src/source/HiAnime.js` (flow verified live upstream, pure
//! `Node.js`, no browser):
//!
//! 1. Search: `GET /search?keyword={name}` (Referer `hianime.at/`) →
//!    anime slug + numeric id (`.film-name a`, falling back to any
//!    `/watch/{slug}-{id}` link).
//! 2. Episodes: `GET /api/theme/episode/list/{animeId}`
//!    (`X-Requested-With: XMLHttpRequest`, Referer `hianime.at/watch/`)
//!    → HTML fragment with `.ssl-item` rows (`data-id` +
//!    `data-number`).
//! 3. Servers: `GET /api/theme/episode/servers?episodeId={id}` →
//!    `.server-item` rows (`data-type`, `data-server-name`, and a
//!    base64 `data-hash` that decodes to the stream page URL).
//! 4. Stream page: the decoded URL → `window.__P="…"`.
//! 5. Deobfuscate: base64-decode → XOR with `"otaku-embed-v1"` → JSON
//!    with `{src: m3u8}`.
//! 6. The m3u8 only plays with `Referer: https://zokoanime.video/`;
//!    upstream routed it through a server-side `/proxy` for that, this
//!    library has no server, so the playlist ships through the
//!    `hianime` extractor with the Referer in
//!    [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).
//!
//! Both SUB (Japanese audio) and DUB (English audio) are emitted; the
//! first 2 servers of each category are probed (upstream's timeout
//! guard).
//!
//! Ported mapping notes:
//! - Upstream `getTmdbId`/`getTmdbNameAndYear` become the engine's
//!   TMDB resolution: name/year come from `ctx.media`, season/episode
//!   from the [`MediaRef`]. Without `ctx.media` there is no title to
//!   search → [`SourceError::NotFound`].
//! - The JS's two-transport ladder (`fetcher.text`, then
//!   `got-scraping`) collapses onto the shared fetcher, which already
//!   owns browser TLS impersonation, redirects, and retries; the same
//!   for the per-request 12s timeouts (the fetcher enforces them).
//! - `meta.title` becomes the stream label, `meta.countryCodes` become
//!   `meta.languages`, `meta.height` becomes `meta.resolution`
//!   (`HiAnime` streams are typically 1080p). `this.ttl` (10min) is the
//!   stream TTL.
//! - Cut: the JS's `console.log` diagnostics (no logging facade in
//!   this crate) and the per-source result cache (the parent's
//!   `CachedSource` owns it).
//! - Patterns are ported as `scraper` selectors and byte scanners
//!   (this crate has no regex engine).

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
const ID: &str = "hianime";
/// Upstream result lifetime: 10min.
const TTL: Duration = Duration::from_mins(10);
/// Fuzzy match threshold (upstream: "require fuzzy score >= 60 to
/// avoid false matches, e.g. searching Supergirl must not return
/// One-Punch Man").
const MIN_MATCH_SCORE: f64 = 60.0;
/// Servers probed per audio category (upstream's timeout guard).
const SERVERS_PER_CATEGORY: usize = 2;
/// The XOR key for the `window.__P` payload — upstream's central
/// `site-secrets.cjs` registry (`OTAKU_XOR_KEY`), reverse-engineered
/// out of the public site, with the same default value.
const OBF_KEY: &str = "otaku-embed-v1";

static BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://hianime.at").unwrap_or_else(|_| panic!("the HiAnime base URL must parse"))
});

static FILM_NAME_LINKS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".film-name a").unwrap_or_else(|_| panic!("valid .film-name a selector"))
});
static ALL_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a").unwrap_or_else(|_| panic!("valid a selector")));
static SSL_ITEMS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".ssl-item").unwrap_or_else(|_| panic!("valid .ssl-item selector"))
});
static SERVER_ITEMS: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".server-item").unwrap_or_else(|_| panic!("valid .server-item selector"))
});

/// A search result row (upstream `{title, url, id, slug}`).
struct SearchResult {
    /// The result title.
    title: String,
    /// The numeric site id (from the `-{id}` href suffix).
    id: u64,
}

/// An episode row from the episode list.
#[derive(Clone)]
struct Episode {
    /// The episode id (`data-id`).
    id: String,
    /// The episode number (`data-number`).
    number: u32,
}

/// A server row from the servers list.
struct Server {
    /// The audio category: `sub` or `dub` (`data-type`).
    category: String,
    /// The server display name (`data-server-name`).
    name: String,
    /// The stream page URL (decoded from the base64 `data-hash`).
    url: Url,
}

/// The episode-list API response.
#[derive(Deserialize)]
struct EpisodeListResponse {
    /// The pre-rendered episode list HTML.
    #[serde(default)]
    html: Option<String>,
}

/// The servers API response.
#[derive(Deserialize)]
struct ServersResponse {
    /// The pre-rendered server list HTML.
    #[serde(default)]
    html: Option<String>,
}

/// The deobfuscated `window.__P` payload.
#[derive(Deserialize)]
struct StreamPayload {
    /// The m3u8 URL.
    #[serde(default)]
    src: Option<String>,
}

/// The `HiAnime` provider.
pub struct HiAnime {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The extractor chain that claims the resolved m3u8 URLs.
    registry: Arc<ExtractorRegistry>,
}

impl HiAnime {
    /// Build the provider over an extractor registry.
    #[must_use]
    pub fn new(registry: Arc<ExtractorRegistry>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: "HiAnime".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::Ja, CountryCode::En],
                base_url: Some(BASE.clone()),
                priority: 0,
                domain_key: None,
            },
            registry,
        }
    }

    /// Resolve the stream-page URLs of both categories' servers.
    async fn resolve_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        servers: &[Server],
        title_base: &str,
    ) -> Result<Vec<Stream>, SourceError> {
        let mut streams = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for category in ["sub", "dub"] {
            let category_servers: Vec<&Server> = servers
                .iter()
                .filter(|server| server.category == category)
                .take(SERVERS_PER_CATEGORY)
                .collect();
            for server in category_servers {
                // Step 5: fetch the stream page and deobfuscate __P.
                let referer = referer_url("/");
                let Some(html) = fetch_html(ctx, &server.url, &referer).await else {
                    // Upstream skips a failed server.
                    continue;
                };
                let Some(payload) = window_p_payload(&html) else {
                    continue;
                };
                let Some(data) = deobfuscate(payload) else {
                    continue;
                };
                let Some(src) = data.src.filter(|src| !src.is_empty()) else {
                    continue;
                };
                // Dedup by URL.
                if !seen.insert(src.clone()) {
                    continue;
                }
                let Ok(url) = Url::parse(&src) else {
                    continue;
                };

                // The hianime extractor claims URLs from this source
                // and attaches the zokoanime Referer.
                let extract_ctx = ResolveCtx {
                    fetcher: ctx.fetcher,
                    media: None,
                    source_id: Some(ID),
                    referer: Some(&server.url),
                };
                let Ok(extracted) = self.registry.extract(&extract_ctx, &url).await else {
                    continue;
                };
                let is_dub = category == "dub";
                for mut stream in extracted {
                    stream.format = Format::Hls;
                    stream.label = Some(format!(
                        "{title_base} (HiAnime {} {})",
                        server.name,
                        if is_dub { "DUB" } else { "SUB" }
                    ));
                    stream.ttl = TTL;
                    stream.meta.languages = if is_dub {
                        vec![CountryCode::Multi, CountryCode::En]
                    } else {
                        vec![CountryCode::Multi, CountryCode::Ja]
                    };
                    stream.meta.source_id = Some(ID.to_string());
                    stream.meta.source_label = Some("HiAnime".to_string());
                    // HiAnime streams are typically 1080p.
                    stream.meta.resolution = Some(1080);
                    streams.push(stream);
                }
            }
        }
        Ok(streams)
    }
}

#[async_trait]
impl Source for HiAnime {
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

        // Step 1: search by title.
        let search_url = Url::parse_with_params("https://hianime.at/search", &[("keyword", name)])
            .map_err(|error| {
                SourceError::scrape(ID, format!("the search URL is invalid: {error}"))
            })?;
        let Some(search_html) = fetch_html(ctx, &search_url, &referer_url("/")).await else {
            // Upstream: "search page unavailable" → [].
            return Ok(Vec::new());
        };
        let results = parse_search_results(&search_html);
        if results.is_empty() {
            return Ok(Vec::new());
        }

        // Pick the best match — a fuzzy score below 60 is refused.
        let Some(best) = pick_best_match(&results, name) else {
            return Ok(Vec::new());
        };

        // Step 2: get the episode list.
        let list_url = Url::parse(&format!(
            "https://hianime.at/api/theme/episode/list/{}",
            best.id
        ))
        .map_err(|error| {
            SourceError::scrape(ID, format!("the episode list URL is invalid: {error}"))
        })?;
        let Some(list) =
            fetch_api_json::<EpisodeListResponse>(ctx, &list_url, &referer_url("/watch/")).await
        else {
            return Ok(Vec::new());
        };
        let Some(episode_html) = list.html.filter(|html| !html.is_empty()) else {
            return Ok(Vec::new());
        };
        let mut episodes = parse_episodes(&episode_html);
        if episodes.is_empty() {
            episodes = parse_episodes_fallback(&episode_html);
        }
        if episodes.is_empty() {
            return Ok(Vec::new());
        }
        episodes.sort_by_key(|episode| episode.number);

        // Step 3: find the target episode.
        let target = season.map_or(1, |_| media.episode.unwrap_or(1));
        let episode = episodes
            .iter()
            .find(|episode| episode.number == target)
            .unwrap_or(&episodes[0]);

        // Step 4: get the servers for the episode (sub and dub).
        let servers_url = Url::parse_with_params(
            "https://hianime.at/api/theme/episode/servers",
            &[("episodeId", episode.id.as_str())],
        )
        .map_err(|error| SourceError::scrape(ID, format!("the servers URL is invalid: {error}")))?;
        let Some(servers) =
            fetch_api_json::<ServersResponse>(ctx, &servers_url, &referer_url("/watch/")).await
        else {
            return Ok(Vec::new());
        };
        let Some(server_html) = servers.html.filter(|html| !html.is_empty()) else {
            return Ok(Vec::new());
        };
        let servers = parse_servers(&server_html);
        if servers.is_empty() {
            return Ok(Vec::new());
        }

        // Step 5: fetch streams from both sub and dub servers.
        self.resolve_streams(ctx, &servers, &title_base).await
    }
}

/// `BASE` joined with `path`, as the JS `Referer` header value.
fn referer_url(path: &str) -> Url {
    BASE.join(path)
        .unwrap_or_else(|_| panic!("the {path} referer must join with the base URL"))
}

/// Fetch a page as HTML with a Referer — the upstream `fetchText`
/// (non-200 answers become `None`).
async fn fetch_html(ctx: &ResolveCtx<'_>, url: &Url, referer: &Url) -> Option<String> {
    let request = FetchRequest::get(url.clone())
        .with_header("Accept", "text/html,*/*")
        .with_header("Referer", referer.as_str())
        .with_timeout(Duration::from_secs(12));
    let response = ctx.fetcher.request(request).await.ok()?;
    let body = response.body;
    (!body.is_empty()).then_some(body)
}

/// Fetch an AJAX endpoint as JSON — the upstream `fetchJson`
/// (`X-Requested-With`, `Accept: application/json`, Referer).
async fn fetch_api_json<T: serde::de::DeserializeOwned>(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &Url,
) -> Option<T> {
    let request = FetchRequest::get(url.clone())
        .with_header("Accept", "application/json")
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_header("Referer", referer.as_str())
        .with_timeout(Duration::from_secs(12));
    let response = ctx.fetcher.request(request).await.ok()?;
    response.json().ok()
}

/// Parse the search page: `.film-name a` first, then any
/// `/watch/{slug}-{id}` link. Ports the two upstream scrapers with
/// their shared id dedup.
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();

    // Primary: `.film-name a` rows — title is the link text as-is.
    for link in doc.select(&FILM_NAME_LINKS) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        // `/([^/]+)-(\d+)$` — the trailing `-{id}` of the last segment.
        let Some((_, id)) = parse_trailing_id(href) else {
            continue;
        };
        if seen.insert(id) {
            let title = link.text().collect::<String>().trim().to_string();
            results.push(SearchResult { title, id });
        }
    }

    // Fallback: any `a` whose href ends in `/watch/{slug}-{id}` — the
    // title falls back to the slug's spaces when the link text is
    // empty (poster-card anchors carry no text).
    if results.is_empty() {
        for link in doc.select(&ALL_LINKS) {
            let Some(href) = link.value().attr("href") else {
                continue;
            };
            if !href.contains("/watch/") {
                continue;
            }
            let Some((slug, id)) = parse_trailing_id(href) else {
                continue;
            };
            if seen.insert(id) {
                let title = link.text().collect::<String>().trim().to_string();
                let title = if title.is_empty() {
                    slug.replace('-', " ")
                } else {
                    title
                };
                results.push(SearchResult { title, id });
            }
        }
    }
    results
}

/// `link.match(/\/([^/]+)-(\d+)$/)` — a non-empty slug, a numeric id,
/// and nothing after it.
fn parse_trailing_id(href: &str) -> Option<(&str, u64)> {
    let segment = href.rsplit('/').next()?;
    if segment.is_empty() {
        return None;
    }
    let pos = segment.rfind('-')?;
    let (slug, digits) = (&segment[..pos], &segment[pos + 1..]);
    if slug.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse().ok()?;
    Some((slug, id))
}

/// Pick the best fuzzy match (≥ 60), ports the upstream scorer:
/// exact = 100, one contains the other = length ratio × 90.
fn pick_best_match<'a>(results: &'a [SearchResult], name: &str) -> Option<&'a SearchResult> {
    let name_norm = normalize(name);
    let mut best: Option<(&SearchResult, f64)> = None;
    for result in results {
        let title_norm = normalize(&result.title);
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
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((result, score));
        }
    }
    let (result, score) = best?;
    (score >= MIN_MATCH_SCORE).then_some(result)
}

/// Lowercase, non-alphanumerics to spaces, collapse — the upstream
/// `normalize`.
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse the episode list fragment: `.ssl-item` rows.
fn parse_episodes(html: &str) -> Vec<Episode> {
    let doc = Html::parse_document(html);
    doc.select(&SSL_ITEMS)
        .filter_map(|item| {
            let id = item.value().attr("data-id")?;
            let number = item
                .value()
                .attr("data-number")
                .and_then(|number| number.parse().ok())
                .unwrap_or(0);
            Some(Episode {
                id: id.to_string(),
                number,
            })
        })
        .collect()
}

/// The episode-list fallback scan, ports
/// `data-number="(\d+)"[^>]*data-id="(\d+)"`.
fn parse_episodes_fallback(html: &str) -> Vec<Episode> {
    const NUMBER_MARKER: &str = "data-number=\"";
    const ID_MARKER: &str = "data-id=\"";
    let mut episodes = Vec::new();
    let mut from = 0;
    while let Some(found) = html[from..].find(NUMBER_MARKER) {
        let number_start = from + found + NUMBER_MARKER.len();
        let after_number = &html[number_start..];
        let Some(number_end) = after_number.find('"') else {
            break;
        };
        let number = &after_number[..number_end];
        // `[^>]*` between the two attributes: no tag close before
        // data-id.
        let between = &after_number[number_end + 1..];
        if let Some(tag_end) = between.find('>')
            && let Some(id_found) = between[..tag_end].find(ID_MARKER)
        {
            let id_start = id_found + ID_MARKER.len();
            let id_rest = &between[id_start..];
            if let Some(id_end) = id_rest.find('"') {
                let id = &id_rest[..id_end];
                if !id.is_empty()
                    && id.bytes().all(|b| b.is_ascii_digit())
                    && !number.is_empty()
                    && number.bytes().all(|b| b.is_ascii_digit())
                    && let Ok(number) = number.parse()
                {
                    episodes.push(Episode {
                        id: id.to_string(),
                        number,
                    });
                }
            }
        }
        from = number_start;
    }
    episodes
}

/// Parse the servers fragment: `.server-item` rows with the base64
/// `data-hash` decoding to the stream page URL.
fn parse_servers(html: &str) -> Vec<Server> {
    let doc = Html::parse_document(html);
    doc.select(&SERVER_ITEMS)
        .filter_map(|item| {
            let category = item.value().attr("data-type").unwrap_or("sub").to_string();
            let name = item
                .value()
                .attr("data-server-name")
                .unwrap_or("unknown")
                .to_string();
            let hash = item.value().attr("data-hash").unwrap_or_default();
            let decoded = String::from_utf8_lossy(&decode_base64_lenient(hash)).into_owned();
            let url = Url::parse(&decoded).ok()?;
            Some(Server {
                category,
                name,
                url,
            })
        })
        .collect()
}

/// The `window.__P="…"` payload — upstream
/// `streamHtml.match(/window\.__P="([^"]+)"/)`.
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
/// need the padding. A decode or parse failure is the upstream caught
/// exception (the server is skipped).
fn deobfuscate(payload: &str) -> Option<StreamPayload> {
    let raw = decode_base64_lenient(payload);
    let key = OBF_KEY.as_bytes();
    let out: Vec<u8> = raw
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ key[i % key.len()])
        .collect();
    serde_json::from_str(&String::from_utf8_lossy(&out)).ok()
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
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::MediaId;
    use vsources_extractors::hosts::hianime::HiAnime as HiAnimeExtractor;

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

    /// A provider wired to a registry with the hianime passthrough
    /// extractor (it attaches the zokoanime Referer).
    fn provider() -> HiAnime {
        HiAnime::new(Arc::new(ExtractorRegistry::new(vec![Arc::new(
            HiAnimeExtractor::new(),
        )])))
    }

    const SEARCH_PAGE: &str = r#"<html><body>
        <div class="film-name"><a href="/watch/one-piece-21">One Piece</a></div>
        <div class="film-name"><a href="/watch/one-piece-film-4112">One Piece Film</a></div>
    </body></html>"#;

    const EPISODE_LIST_JSON: &str = r#"{"html":"<div class=\"ssl-item\" data-id=\"1234\" data-number=\"1\"></div><div class=\"ssl-item\" data-id=\"1235\" data-number=\"5\"></div><div class=\"ssl-item\" data-id=\"1236\" data-number=\"9\"></div>"}"#;

    #[test]
    fn decodes_the_xor_payload() {
        let payload = r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/master.m3u8"}"#;
        let page = stream_page(payload);
        let extracted = window_p_payload(&page)
            .and_then(deobfuscate)
            .unwrap_or_else(|| panic!("the payload must deobfuscate"));
        assert_eq!(
            extracted.src.as_deref(),
            Some("https://hls2.aniwatchtv.uk/v/abc/master.m3u8")
        );
        // Garbage payloads fail closed (the upstream skips the server).
        assert!(window_p_payload("<html>nothing</html>").is_none());
        assert!(deobfuscate("!!!not-base64!!").is_none());
    }

    #[test]
    fn parses_trailing_link_ids() {
        assert_eq!(
            parse_trailing_id("/watch/one-piece-21"),
            Some(("one-piece", 21))
        );
        assert_eq!(
            parse_trailing_id("/watch/one-piece-2-1"),
            Some(("one-piece-2", 1))
        );
        // No trailing id, or a trailing slash: no match.
        assert_eq!(parse_trailing_id("/watch/one-piece"), None);
        assert_eq!(parse_trailing_id("/watch/one-piece-21/"), None);
    }

    #[tokio::test]
    async fn resolves_sub_and_dub_hls_streams() -> Result<(), SourceError> {
        // Two sub servers (the second yields the same src — dedup) and
        // one dub server.
        let sub_hash = b64(b"https://zoko.watch/e/sub1");
        let second_sub_hash = b64(b"https://zoko.watch/e/sub2");
        let dub_hash = b64(b"https://zoko.watch/e/dub");
        let servers_json = format!(
            r#"{{"html":"<div class=\"server-item\" data-type=\"sub\" data-server-name=\"HD-1\" data-hash=\"{sub_hash}\"></div><div class=\"server-item\" data-type=\"sub\" data-server-name=\"HD-2\" data-hash=\"{second_sub_hash}\"></div><div class=\"server-item\" data-type=\"dub\" data-server-name=\"HD-1\" data-hash=\"{dub_hash}\"></div>"}}"#
        );
        let payload = r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/master.m3u8"}"#;

        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(at("hianime.at", "/search"), SEARCH_PAGE)
        .page(
            at("hianime.at", "/api/theme/episode/list/21"),
            EPISODE_LIST_JSON,
        )
        .page(at("hianime.at", "/api/theme/episode/servers"), servers_json)
        .page(at("zoko.watch", "/e/sub1"), stream_page(payload))
        .page(
            at("zoko.watch", "/e/sub2"),
            // Same src as sub1: the dedup keeps one sub stream.
            stream_page(payload),
        )
        .page(
            at("zoko.watch", "/e/dub"),
            stream_page(r#"{"src":"https://hls2.aniwatchtv.uk/v/abc/dub/master.m3u8"}"#),
        );

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;

        assert_eq!(streams.len(), 2, "one sub (deduped) + one dub");
        let sub = &streams[0];
        assert_eq!(
            sub.label.as_deref(),
            Some("One Piece S01E05 (HiAnime HD-1 SUB)")
        );
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(
            sub.url.as_str(),
            "https://hls2.aniwatchtv.uk/v/abc/master.m3u8"
        );
        let dub = &streams[1];
        assert_eq!(
            dub.label.as_deref(),
            Some("One Piece S01E05 (HiAnime HD-1 DUB)")
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
            // The zokoanime Referer upstream's proxy hop sent.
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://zokoanime.video/")
            );
        }
        // The AJAX endpoints carried the upstream headers.
        assert_eq!(
            fetcher
                .header_sent_to("/api/theme/episode/list/21", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            fetcher
                .header_sent_to("/api/theme/episode/list/21", "Referer")
                .as_deref(),
            Some("https://hianime.at/watch/")
        );
        // The stream pages were fetched with the site referer.
        assert_eq!(
            fetcher.header_sent_to("/e/sub1", "Referer").as_deref(),
            Some("https://hianime.at/")
        );
        Ok(())
    }

    #[tokio::test]
    async fn refuses_fuzzy_mismatches() -> Result<(), SourceError> {
        // The search page has no `.film-name` rows: the fallback `a`
        // scan finds a watch link, but the title does not match.
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
        .page(
            at("hianime.at", "/search"),
            r#"<html><body><a href="/watch/unrelated-show-999">Unrelated Show</a></body></html>"#,
        );

        let (media, meta) = one_piece();
        let ctx = ctx(&fetcher, Some(meta));
        let streams = provider().resolve(&ctx, &media).await?;
        assert!(
            streams.is_empty(),
            "a non-matching title must yield nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_search_page_is_an_empty_answer() -> Result<(), SourceError> {
        // Nothing scripted: the search request 404s.
        let fetcher = ScriptedFetcher {
            pages: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        };

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
