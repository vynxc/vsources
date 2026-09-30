//! `PrimeShows`: scraped multi-server embeds at `primeshows.gd`.
//!
//! Ports `src/source/PrimeShows.js`:
//!
//! 1. Resolve the TMDB id — IMDb-keyed references map through the context
//!    media or [`TmdbClient`] (`getTmdbId`), then fetch name/year
//!    (`getTmdbNameAndYear`).
//! 2. Fetch the watch page (`/watch/movie/{id}` or
//!    `/watch/tv/{id}/season/{s}/episode/{e}`) with `Cookie: hv=1`. The
//!    page is a JS-cookie-gated SPA: without the cookie a ~676-byte
//!    interstitial runs `document.cookie = "hv=1; …"` and replaces
//!    itself — scrapers see no iframe and zero streams. When the
//!    interstitial still appears, its cookie assignment is parsed
//!    (self-healing a gate rotation) and the page is retried once
//!    (Task 56 — the production zero-streams fix).
//! 3. Collect servers three ways, in upstream order: the client-side
//!    `SRV_MAP` JS object on the real page — one fetch covering every
//!    server the site offers (vidsrc.mov, vidsrc.fyi, vidrock, vidnest,
//!    vidking, vidlink, vidfast, vidup, videasy, 111movies, 2embed,
//!    multiembed, superflix, peachify); the legacy `?server=` pages with
//!    one `playerFrame` iframe each (Promise.allSettled — a failed
//!    legacy fetch is skipped); and the page's own `playerFrame` as
//!    the default server. TV watch paths use the site's
//!    flattened-season form
//!    (`/watch/tv/{id}/season/{s}/episode/{e}`) verbatim.
//! 4. Resolve each embed through the [`ExtractorRegistry`] with the watch
//!    page as the extract context's referer. The JS passed `meta.vidking`
//!    for **movies only** (the `VidKing` speedracelight API is TMDB-based
//!    and needs no JS); for series the remaining hosts without dedicated
//!    extractors resolve to nothing upstream — the registry's media
//!    fallback implements that routing: the extract context carries media
//!    with a TMDB id for movies only.
//!
//! Cuts from the upstream, mapped rather than dropped:
//!
//! - `meta.title` — `StreamMeta` has no title field; the JS title string
//!   (`{name} ({year})` / `{name} S01E02`, plus the server label) is
//!   carried as [`Stream::label`] on every resolved stream.
//! - the interstitial retry's `BROWSER_HEADERS` user agent — the fetcher
//!   layer already sends browser-like headers.
//! - `meta.vidking` — the registry joins the `vidking` extractor when the
//!   extract context carries media with a TMDB id (movies only here).
//! - upstream sources returned embed URLs that `StreamResolver` extracted
//!   afterwards; this provider resolves inline through the registry, and
//!   one embed's failure is skipped (the resolver's `.catch(() => [])`).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, ResolvedMedia, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};
use vsources_extractors::ExtractorRegistry;

/// The provider id, upstream `this.id`.
const ID: &str = "primeshows";
/// The display label, upstream `this.label`.
const LABEL: &str = "PrimeShows";
/// The site origin, upstream `this.baseUrl`.
const BASE_URL: &str = "https://primeshows.gd";

/// The watch-page timeout, upstream `timeout: 8000`.
const WATCH_TIMEOUT: Duration = Duration::from_secs(8);

/// The cookie-gate value the interstitial sets (upstream's hard-coded
/// `Cookie: hv=1`).
const GATE_COOKIE: &str = "hv=1";

/// `iframe#playerFrame` — the per-server embed frame.
static PLAYER_FRAME: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("iframe#playerFrame")
        .unwrap_or_else(|e| panic!("valid playerFrame selector: {e}"))
});

/// The legacy `?server=` keys, used only when the page carries no
/// `SRV_MAP` (the pre-migration scheme, kept as a safety net).
const LEGACY_SERVERS: &[&str] = &[
    "vidsrcto",
    "vidsrcfyi",
    "vidnest",
    "vidlink",
    "vidfast",
    "2embed",
];

/// The `PrimeShows` provider.
pub struct PrimeShows {
    /// The descriptor served by `info`.
    info: SourceInfo,
    /// The registry the embeds resolve through.
    extractors: Arc<ExtractorRegistry>,
    /// TMDB identity and metadata resolution.
    tmdb: Arc<TmdbClient>,
}

impl PrimeShows {
    /// Build the provider over an extractor registry and a TMDB client.
    #[must_use]
    pub fn new(extractors: Arc<ExtractorRegistry>, tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            extractors,
            tmdb,
        }
    }

    /// GET the watch page with the `hv=1` cookie gate; a rotated gate is
    /// retried once with the cookie the interstitial itself sets — ports
    /// `fetchWatchPage`.
    async fn fetch_watch_page(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<String, SourceError> {
        let html = self.fetch_with_cookie(ctx, url, GATE_COOKIE).await?;
        if is_interstitial(&html) {
            // Cookie gate rotated or the preset cookie was rejected —
            // parse what the page wants and retry once.
            let cookie = interstitial_cookie(&html);
            return self.fetch_with_cookie(ctx, url, &cookie).await;
        }
        Ok(html)
    }

    /// GET `url` with `Cookie: cookie`.
    async fn fetch_with_cookie(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
        cookie: &str,
    ) -> Result<String, SourceError> {
        let request = FetchRequest::get(url.clone())
            .with_header("Cookie", cookie.to_string())
            .with_timeout(WATCH_TIMEOUT);
        let response = ctx.fetcher.request(request).await.map_err(source_error)?;
        Ok(response.body)
    }
}

#[async_trait]
impl Source for PrimeShows {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = resolve_tmdb_id(ctx, media, &self.tmdb).await?;
        let (name, year) = name_and_year(ctx, media, &self.tmdb, tmdb_id).await?;
        let is_tv = media.season.is_some();
        let title = embed_title(&name, year, media);

        let base = self
            .info
            .base_url
            .as_ref()
            .ok_or_else(|| SourceError::scrape(ID, "missing base URL"))?;
        let watch_path = if is_tv {
            format!(
                "/watch/tv/{tmdb_id}/season/{}/episode/{}",
                media.season.unwrap_or(1),
                media.episode.unwrap_or(1)
            )
        } else {
            format!("/watch/movie/{tmdb_id}")
        };
        let watch = base
            .join(&watch_path)
            .map_err(|error| SourceError::scrape(ID, format!("invalid watch URL: {error}")))?;

        let html = self.fetch_watch_page(ctx, &watch).await?;

        // 1. `SRV_MAP` — one fetch covers every server the site offers.
        let mut servers = srv_map_servers(&html);

        // 2. Legacy per-server pages with their own `playerFrame`.
        if servers.is_empty() {
            for key in LEGACY_SERVERS {
                let mut page_url = watch.clone();
                page_url.query_pairs_mut().append_pair("server", key);
                // `Promise.allSettled` — a failed legacy fetch is skipped.
                let Ok(page) = self.fetch_watch_page(ctx, &page_url).await else {
                    continue;
                };
                if let Some(src) = player_frame_src(&page) {
                    servers.push(((*key).to_string(), src));
                }
            }
        }

        // 3. The page's own `playerFrame` as the default server.
        if servers.is_empty()
            && let Some(src) = player_frame_src(&html)
        {
            servers.push(("vidsrcto".to_string(), src));
        }

        if servers.is_empty() {
            return Err(SourceError::NotFound);
        }

        // `const vidkingMeta = tmdbId.season ? null : {…}` — movies only.
        let extract_media = (!is_tv).then(|| ResolvedMedia {
            tmdb_id: Some(tmdb_id),
            imdb_id: None,
            name: name.clone(),
            year,
            season: None,
            episode: None,
        });

        let mut streams = Vec::new();
        for (key, embed) in servers {
            let label = format!("{title} ({})", server_label(&key));
            let sub_ctx = ResolveCtx {
                fetcher: ctx.fetcher,
                media: extract_media.clone(),
                source_id: Some(ID),
                referer: Some(&watch),
            };
            // One failed embed extraction is skipped, like the resolver's
            // `.catch(() => [])`.
            let resolved = self
                .extractors
                .extract(&sub_ctx, &embed)
                .await
                .unwrap_or_default();
            streams.extend(resolved.into_iter().map(|stream| tagged(stream, &label)));
        }
        Ok(streams)
    }
}

/// The `SRV_MAP` entries with absolute http(s) URLs — the JS regex
/// capture, `JSON.parse`, `Object.entries` (source order — see
/// [`flat_entries`]), and the `typeof href === 'string'` check.
fn srv_map_servers(html: &str) -> Vec<(String, Url)> {
    let Some(json) = srv_map_json(html) else {
        return Vec::new();
    };
    flat_entries(&json)
        .into_iter()
        .filter_map(|(key, href)| {
            let url = Url::parse(&href).ok()?;
            let absolute = url.scheme() == "http" || url.scheme() == "https";
            absolute.then_some((key, url))
        })
        .collect()
}

/// The `(key, value)` string pairs of a flat JSON object, in source
/// order — a tiny walker instead of `serde_json::Map`, whose `BTreeMap`
/// iteration would re-sort keys, unlike the JS's `Object.entries`.
/// Handles the `\\`, `\"`, and `\/` escapes URL values carry (the
/// `\u0026` form is already unescaped by [`srv_map_json`]); non-string
/// values yield no pair, like the JS's `typeof` check skipping them.
fn flat_entries(json: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut key: Option<String> = None;
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in json.chars() {
        if !in_string {
            // `:`, `,`, `{`, `}`, and whitespace are structure.
            if c == '"' {
                in_string = true;
            }
            continue;
        }
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = false;
            if let Some(key) = key.take() {
                entries.push((key, current.clone()));
            } else {
                key = Some(current.clone());
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    entries
}

/// The `SRV_MAP` object literal as JSON, `\u0026`/`&amp;` unescaped — the
/// JS regex `SRV_MAP\s*=\s*(\{[^}]+\})` over a flat object; slicing to
/// the first `}` is the same capture.
fn srv_map_json(html: &str) -> Option<String> {
    let at = html.find("SRV_MAP")?;
    let after_eq = html[at + "SRV_MAP".len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let object = after_eq.strip_prefix('{')?;
    let end = object.find('}')?;
    let raw = &object[..end];
    Some(
        format!("{{{raw}}}")
            .replace("\\u0026", "&")
            .replace("&amp;", "&"),
    )
}

/// `iframe#playerFrame`'s `src` — the JS regex `<iframe[^>]*id="playerFrame"
/// [^>]*src="([^"]+)"`; scraper decodes the `&amp;` entities the JS
/// replaced manually (attribute order is not enforced, which real pages
/// keep consistent anyway).
fn player_frame_src(html: &str) -> Option<Url> {
    let document = Html::parse_document(html);
    let src = document.select(&PLAYER_FRAME).next()?.value().attr("src")?;
    Url::parse(src).ok()
}

/// The interstitial's fingerprint (upstream `isInterstitial`): a tiny
/// body that sets a cookie and carries no player frame.
fn is_interstitial(html: &str) -> bool {
    html.len() < 4000 && sets_document_cookie(html) && !html.contains("playerFrame")
}

/// `document.cookie\s*=` — an assignment after the property access.
fn sets_document_cookie(html: &str) -> bool {
    html.match_indices("document.cookie").any(|(at, _)| {
        html[at + "document.cookie".len()..]
            .trim_start()
            .starts_with('=')
    })
}

/// `document.cookie = "NAME=VAL; …"` → `NAME=VAL`, defaulting to `hv=1`
/// when the rotation parse fails — ports `interstitialCookie`, so a
/// future cookie-name rotation self-heals instead of zeroing the source.
fn interstitial_cookie(html: &str) -> String {
    for (at, _) in html.match_indices("document.cookie") {
        let Some(after_eq) = html[at + "document.cookie".len()..]
            .trim_start()
            .strip_prefix('=')
        else {
            continue;
        };
        let Some(quoted) = after_eq.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(eq) = quoted.find('=') else {
            continue;
        };
        let (name, rest) = quoted.split_at(eq);
        // `[^";]+` — the value runs to the closing quote or the cookie's
        // attribute separator.
        let value = rest[1..].split(['"', ';']).next().unwrap_or_default();
        let name_ok =
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if name_ok && !value.is_empty() {
            return format!("{name}={value}");
        }
    }
    GATE_COOKIE.to_string()
}

/// Server-key → card label, every key observed in the live `SRV_MAP`
/// (upstream `SERVER_LABELS[key] || key`).
fn server_label(key: &str) -> String {
    let known = match key {
        "vidsrcto" => Some("VidSrc"),
        "vidsrcfyi" => Some("VidSrc.fyi"),
        "vidrock" => Some("VidRock"),
        "vidnest" => Some("Vidnest"),
        "vidking" => Some("VidKing"),
        "vidlink" => Some("VidLink"),
        "vidfast" => Some("VidFast"),
        "vidup" => Some("VidUp"),
        "videasy" => Some("VidEasy"),
        "111movies" => Some("111Movies"),
        "2embed" => Some("2Embed"),
        "multiembed" => Some("MultiEmbed"),
        "superflix" => Some("SuperFlix"),
        "peachify" => Some("Peachify"),
        _ => None,
    };
    known.unwrap_or(key).to_string()
}

/// A fetch failure as a source error — 404s are misses (the upstream
/// fetcher's `NotFoundError`), everything else a real failure.
fn source_error(error: FetchError) -> SourceError {
    match error {
        FetchError::NotFound { .. } => SourceError::NotFound,
        other => SourceError::Fetch(other),
    }
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn resolve_tmdb_id(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => tmdb.tmdb_id_from_imdb(imdb, media.kind).await,
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear` (whose errors propagate upstream).
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = tmdb.name_and_year(tmdb_id, media.kind, None).await?;
    Ok((name.name, name.year))
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the upstream
/// `meta.title`, carried as the stream label.
fn embed_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// Attach the provider identity and the JS's title to an
/// extractor-produced stream.
fn tagged(mut stream: Stream, label: &str) -> Stream {
    stream.label = Some(label.to_string());
    stream.meta.languages = vec![CountryCode::Multi];
    stream.meta.source_id = Some(ID.to_string());
    stream.meta.source_label = Some(LABEL.to_string());
    stream
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use url::Url;
    use vsources_core::error::{ExtractorError, FetchError, SourceError};
    use vsources_core::tmdb::TmdbClient;
    use vsources_core::traits::{
        Extractor, FetchRequest, FetchResponse, Fetcher, ResolveCtx, ResolvedMedia, Source,
    };
    use vsources_core::types::{CountryCode, Format, MediaId, MediaRef, MediaType, Stream};
    use vsources_extractors::ExtractorRegistry;

    use super::PrimeShows;

    // -- the scripted fetcher ------------------------------------------------

    /// A fetcher serving canned bodies keyed by URL path (or `path?query`)
    /// in call order — the last body repeats — recording every request.
    /// Query-bearing lookups fall back to the bare path, so TMDB requests
    /// (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<String>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `body`; earlier registrations pop first.
        fn page(self, key: impl Into<String>, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push(body.into());
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// The requests whose lookup key is exactly `key`.
        fn requests_for(&self, key: &str) -> Vec<FetchRequest> {
            self.requests()
                .into_iter()
                .filter(|request| key_of(&request.url) == key)
                .collect()
        }

        /// A header of the first request whose lookup key is `key`.
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests_for(key).into_iter().find_map(|request| {
                request
                    .headers
                    .iter()
                    .find(|(header, _)| header.eq_ignore_ascii_case(name))
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
            let body = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let body = match body {
                Some(body) => Some(body),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(request.url.path())
                    .map(|bodies| bodies[0].clone()),
            };
            match body {
                Some(body) => Ok(FetchResponse {
                    url: request.url,
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "text/html".to_string(),
                    )]),
                    body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    // -- the stub extractor --------------------------------------------------

    /// One extraction a stub observed.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        url: String,
        source_id: Option<String>,
        referer: Option<String>,
        tmdb_id: Option<u64>,
    }

    /// What a stub answers.
    enum Outcome {
        /// `ExtractorError::NotFound`.
        Miss,
        /// One direct HLS stream.
        Direct,
    }

    /// An extractor that records its calls and answers a canned result —
    /// with the id `vidking` it doubles as the registry's media fallback.
    struct StubExtractor {
        id: &'static str,
        label: &'static str,
        hosts: &'static [&'static str],
        outcome: Outcome,
        calls: Mutex<Vec<Call>>,
    }

    impl StubExtractor {
        /// A stub claiming `hosts` with the given outcome.
        fn build(id: &'static str, hosts: &'static [&'static str], outcome: Outcome) -> Arc<Self> {
            Arc::new(Self {
                id,
                label: id,
                hosts,
                outcome,
                calls: Mutex::new(Vec::new()),
            })
        }

        /// The recorded calls.
        fn calls(&self) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Extractor for StubExtractor {
        fn id(&self) -> &str {
            self.id
        }

        fn label(&self) -> &str {
            self.label
        }

        fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
            self.hosts
                .iter()
                .any(|host| url.host_str().is_some_and(|h| h.ends_with(host)))
        }

        async fn extract(
            &self,
            ctx: &ResolveCtx<'_>,
            url: &Url,
        ) -> Result<Vec<Stream>, ExtractorError> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Call {
                    url: url.to_string(),
                    source_id: ctx.source_id.map(str::to_string),
                    referer: ctx.referer.map(ToString::to_string),
                    tmdb_id: ctx.media.as_ref().and_then(|media| media.tmdb_id),
                });
            match self.outcome {
                Outcome::Miss => Err(ExtractorError::NotFound),
                Outcome::Direct => Ok(vec![Stream::new(
                    Url::parse("https://cdn.example.com/hls/master.m3u8")
                        .unwrap_or_else(|error| panic!("valid test URL: {error}")),
                    Format::Hls,
                )]),
            }
        }
    }

    // -- fixtures ------------------------------------------------------------

    /// The provider over a registry of `stubs` and a TMDB client that
    /// shares the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>, stubs: &[Arc<StubExtractor>]) -> PrimeShows {
        let extractors = ExtractorRegistry::new(
            stubs
                .iter()
                .map(|stub| {
                    let extractor: Arc<dyn Extractor> = stub.clone();
                    extractor
                })
                .collect(),
        );
        let tmdb = TmdbClient::new("test-key", mock.clone());
        PrimeShows::new(Arc::new(extractors), Arc::new(tmdb))
    }

    /// A context over the scripted fetcher, optionally with media.
    fn ctx_for(fetcher: &Arc<ScriptedFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = fetcher.as_ref();
        ResolveCtx {
            fetcher,
            media,
            source_id: None,
            referer: None,
        }
    }

    /// A real watch page carrying an `SRV_MAP` with two servers (the
    /// escaped `\/`, `\u0026` forms included, like the live page).
    const WATCH_PAGE: &str = r#"<html><body><script>
        var SRV_MAP = {"vidsrcto":"https:\/\/vidsrc.mov\/embed\/movie\/27205","vidlink":"https:\/\/vidlink.pro\/movie\/27205?autoplay=true\u0026title=true"};
        </script><iframe id="playerFrame" src="https://vidsrc.mov/embed/movie/27205"></iframe></body></html>"#;

    /// The interstitial the cookie gate serves (tiny, sets a rotated
    /// cookie, replaces itself, no player frame).
    const INTERSTITIAL: &str = r#"<html><body><script>
        document.cookie = "psv=2; path=/"; location.replace(location.href);
        </script></body></html>"#;

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock, &[]);
        let info = provider.info();
        assert_eq!(info.id, "primeshows");
        assert_eq!(info.label, "PrimeShows");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(info.country_codes, vec![CountryCode::Multi]);
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://primeshows.gd/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[tokio::test]
    async fn parses_srv_map_servers() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/watch/movie/27205", WATCH_PAGE)
                .page(
                    "/3/movie/27205",
                    r#"{"title":"Inception","release_date":"2010-07-16"}"#,
                ),
        );
        let generic =
            StubExtractor::build("generic", &["vidsrc.mov", "vidlink.pro"], Outcome::Miss);
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic, vidking.clone()]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        // Both SRV_MAP servers resolved through the media fallback, with
        // the `\u0026` escape decoded into the vidlink query.
        assert_eq!(streams.len(), 2);
        let calls = vidking.calls();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://vidsrc.mov/embed/movie/27205",
                "https://vidlink.pro/movie/27205?autoplay=true&title=true",
            ]
        );
        for call in &calls {
            // The watch page is the embed's referer.
            assert_eq!(
                call.referer.as_deref(),
                Some("https://primeshows.gd/watch/movie/27205")
            );
            assert_eq!(call.tmdb_id, Some(27205));
            assert_eq!(call.source_id.as_deref(), Some("primeshows"));
        }
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Inception (2010) (VidSrc)")
        );
        assert_eq!(
            streams[1].label.as_deref(),
            Some("Inception (2010) (VidLink)")
        );
        for stream in &streams {
            assert_eq!(stream.meta.source_id.as_deref(), Some("primeshows"));
            assert_eq!(stream.meta.source_label.as_deref(), Some("PrimeShows"));
            assert_eq!(stream.meta.languages, vec![CountryCode::Multi]);
        }
        // The gate cookie traveled on the watch-page fetch.
        assert_eq!(
            mock.sent_header("/watch/movie/27205", "Cookie").as_deref(),
            Some("hv=1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn retries_the_interstitial_with_the_rotated_cookie() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/watch/movie/27205", INTERSTITIAL)
                .page("/watch/movie/27205", WATCH_PAGE)
                .page(
                    "/3/movie/27205",
                    r#"{"title":"Inception","release_date":"2010-07-16"}"#,
                ),
        );
        let generic =
            StubExtractor::build("generic", &["vidsrc.mov", "vidlink.pro"], Outcome::Direct);
        let provider = provider(&mock, &[generic]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        assert_eq!(streams.len(), 2);
        // The gate rotated: the first fetch saw the interstitial, the
        // retry used the cookie it asked for.
        let watch_requests = mock.requests_for("/watch/movie/27205");
        assert_eq!(watch_requests.len(), 2);
        assert_eq!(
            watch_requests[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Cookie"))
                .map(|(_, value)| value.as_str()),
            Some("hv=1")
        );
        assert_eq!(
            watch_requests[1]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Cookie"))
                .map(|(_, value)| value.as_str()),
            Some("psv=2")
        );
        Ok(())
    }

    #[tokio::test]
    async fn legacy_server_pages_fall_back_to_player_frames() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page("/watch/tv/1396/season/1/episode/2", "<html><body>no map here</body></html>")
                .page(
                    "/watch/tv/1396/season/1/episode/2?server=vidsrcfyi",
                    r#"<html><body><iframe id="playerFrame" src="https://vidsrc.fyi/embed/tv/1396/1/2&amp;autoplay=1"></iframe></body></html>"#,
                )
                .page("/3/tv/1396", r#"{"name":"Breaking Bad","first_air_date":"2008-01-20"}"#),
        );
        let generic = StubExtractor::build("generic", &["vidsrc.fyi"], Outcome::Direct);
        let vidking = StubExtractor::build("vidking", &[], Outcome::Direct);
        let provider = provider(&mock, &[generic.clone(), vidking]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::series(MediaId::Tmdb(1396), 1, 2))
            .await?;

        // Only the scripted legacy page produced a server; the unscripted
        // legacy fetches were skipped (Promise.allSettled), and the
        // `&amp;` entity came back decoded.
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Breaking Bad S01E02 (VidSrc.fyi)")
        );
        assert_eq!(
            generic.calls()[0].url,
            "https://vidsrc.fyi/embed/tv/1396/1/2&autoplay=1"
        );
        assert_eq!(
            generic.calls()[0].referer.as_deref(),
            Some("https://primeshows.gd/watch/tv/1396/season/1/episode/2")
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_pages_own_player_frame_is_the_last_resort() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    "/watch/movie/27205",
                    r#"<html><body><iframe id="playerFrame" src="https://vidsrc.mov/embed/movie/27205"></iframe></body></html>"#,
                )
                // Every legacy `?server=` page exists but carries no
                // player frame — the legacy scheme finds nothing.
                .page("/watch/movie/27205?server=vidsrcto", "<html></html>")
                .page("/watch/movie/27205?server=vidsrcfyi", "<html></html>")
                .page("/watch/movie/27205?server=vidnest", "<html></html>")
                .page("/watch/movie/27205?server=vidlink", "<html></html>")
                .page("/watch/movie/27205?server=vidfast", "<html></html>")
                .page("/watch/movie/27205?server=2embed", "<html></html>")
                .page("/3/movie/27205", r#"{"title":"Inception","release_date":"2010-07-16"}"#),
        );
        let generic = StubExtractor::build("generic", &["vidsrc.mov"], Outcome::Direct);
        let provider = provider(&mock, &[generic]);
        let ctx = ctx_for(&mock, None);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await?;

        // No SRV_MAP, no legacy pages — the page's own frame ships as the
        // default server under the `vidsrcto` label.
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].label.as_deref(),
            Some("Inception (2010) (VidSrc)")
        );
        // Six legacy fetches were attempted and all missed.
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|request| request
                    .url
                    .query()
                    .is_some_and(|query| query.starts_with("server=")))
                .count(),
            6
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_servers_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    "/watch/movie/27205",
                    "<html><body>nothing here</body></html>",
                )
                .page(
                    "/3/movie/27205",
                    r#"{"title":"Inception","release_date":"2010-07-16"}"#,
                ),
        );
        let provider = provider(&mock, &[]);
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(27205)))
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("a server-less watch page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tmdb_miss_is_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default().page("/3/find/tt0000000", r#"{"movie_results":[]}"#),
        );
        let provider = provider(&mock, &[]);
        let ctx = ctx_for(&mock, None);

        match provider
            .resolve(
                &ctx,
                &MediaRef::movie(MediaId::Imdb("tt0000000".to_string())),
            )
            .await
        {
            Err(SourceError::NotFound) => {}
            other => panic!("an unmapped IMDb id must be a NotFound, got {other:?}"),
        }
    }

    #[test]
    fn the_interstitial_fingerprint_and_cookie_parse() {
        assert!(super::is_interstitial(INTERSTITIAL));
        assert!(!super::is_interstitial(WATCH_PAGE));
        assert_eq!(super::interstitial_cookie(INTERSTITIAL), "psv=2");
        // A rotation that does not parse falls back to the gate default.
        assert_eq!(
            super::interstitial_cookie("<script>document.cookie = 1;</script>"),
            "hv=1"
        );
    }
}
