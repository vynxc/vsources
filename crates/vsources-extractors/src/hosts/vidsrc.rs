//! `VidSrc`: the vidsrc.to mirror family's `CloudStream Pro` server.
//!
//! Ports `src/extractor/VidSrc.js`:
//!
//! 1. Re-host the embed URL on a random mirror domain (upstream's domain
//!    rotation, retrying the remaining mirrors on rate-limit/block) and
//!    fetch the page.
//! 2. Read `#player_iframe`'s src — the iframe origin everything else
//!    resolves against — plus the `.server` element named
//!    `CloudStream Pro` and its `data-hash`.
//! 3. `/rcp/{dataHash}` on the iframe origin returns a page whose
//!    `src: '…'` points at the player page; that page carries a
//!    `https://{vN}/… or …` file URL whose `{vN}` token is substituted
//!    with the iframe host — the final m3u8.
//!
//! The m3u8 ships with no request headers, exactly like the upstream
//! result (only the height probe sends a `Referer`).
//!
//! Cut from the upstream: the random `ctx.ip` spoofing (the resolve
//! context carries no client-IP injection) and the constructor-injected
//! mirror domain list (never populated in the upstream registry — the
//! family mirrors attested across the codebase ship as the default,
//! overridable via [`VidSrc::with_domains`]). `meta.title` has no
//! `StreamMeta` field — the stream label carries the server name.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;
use vsources_core::error::{ExtractorError, FetchError};
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream, StreamMeta};

use crate::helpers::{first_capture, host_matcher};

host_matcher!(HOSTS, r"vidsrc|vsrc|vsembed");

/// The mirror domain rotation. Upstream injects these through the
/// constructor; these are the family hosts attested across the codebase
/// (`vidsrc.to`/`vidsrc.net`/`vidsrc-embed.ru` from the embed hosts,
/// `vidsrc.mov`/`vidsrc.fyi` from the PrimeShows/WatchSeries server maps,
/// `vsembed` from the host regex).
const DEFAULT_DOMAINS: &[&str] = &[
    "vidsrc.to",
    "vidsrc.net",
    "vidsrc.mov",
    "vidsrc.fyi",
    "vidsrc-embed.ru",
    "vsembed.com",
];

static PLAYER_IFRAME: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("#player_iframe").unwrap_or_else(|e| panic!("valid iframe selector: {e}"))
});
static SERVER: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".server").unwrap_or_else(|e| panic!("valid server selector: {e}"))
});

static SRC_IN_IFRAME: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"src:\s?'(.*)'").unwrap_or_else(|e| panic!("valid src pattern: {e}"))
});
static FILE_IN_PLAYER: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(https://.*?\{v\d}.*?) or")
        .unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
static V_TOKEN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\{v\d}").unwrap_or_else(|e| panic!("valid v-token pattern: {e}"))
});
static HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});

/// Upstream result lifetime: 3h.
const TTL: Duration = Duration::from_hours(3);

/// The `VidSrc` extractor.
#[derive(Debug)]
pub struct VidSrc {
    /// The mirror domains to rotate through.
    domains: Vec<String>,
}

impl Default for VidSrc {
    fn default() -> Self {
        Self {
            domains: DEFAULT_DOMAINS
                .iter()
                .map(|domain| (*domain).to_string())
                .collect(),
        }
    }
}

impl VidSrc {
    /// A new extractor over the default mirror domains.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the mirror domain list — ports the constructor's
    /// `domains` injection.
    #[must_use]
    pub fn with_domains(domains: Vec<String>) -> Self {
        Self { domains }
    }
}

#[async_trait]
impl Extractor for VidSrc {
    fn id(&self) -> &'static str {
        "vidsrc"
    }

    fn label(&self) -> &'static str {
        "VidSrc"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let mut domains = self.domains.clone();
        if domains.is_empty() {
            return Err(ExtractorError::NotFound);
        }
        loop {
            // `domains.splice(randomIndex, 1)` — pull one mirror and
            // re-host the embed URL on it.
            let domain = domains.remove(random_index(domains.len()));
            let mut mirror = url.clone();
            mirror.set_host(Some(&domain)).map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid mirror domain: {e}"))
            })?;

            match fetch_embed_page(ctx, &mirror).await {
                Ok(html) => return self.extract_from_page(ctx, &mirror, &html).await,
                // Mirror rotation: retry the remaining domains on
                // rate-limit/block, ports the splice-retry loop.
                Err(ExtractorError::Fetch(
                    FetchError::RateLimited { .. } | FetchError::Blocked { .. },
                )) if !domains.is_empty() => {}
                Err(error) => return Err(error),
            }
        }
    }
}

impl VidSrc {
    /// Parse the mirror page and walk the `CloudStream Pro` server chain.
    async fn extract_from_page(
        &self,
        ctx: &ResolveCtx<'_>,
        mirror: &Url,
        html: &str,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Strip HTML comments before parsing (upstream's replace pair —
        // sites hide the server markup in comments).
        //
        // The DOM extraction happens before any await: scraper's
        // element refs are not `Send`, so the loop below only sees
        // owned strings.
        let (iframe_url, servers) = {
            let document = Html::parse_document(&html.replace("<!--", "").replace("-->", ""));

            let iframe_src = document
                .select(&PLAYER_IFRAME)
                .find_map(|element| element.value().attr("src"))
                .ok_or_else(|| {
                    ExtractorError::extraction(self.id(), "no #player_iframe src on the page")
                })?;
            // `src.replace(/^\/\//, 'https://')`.
            let iframe_src = iframe_src
                .strip_prefix("//")
                .map_or_else(|| iframe_src.to_string(), |rest| format!("https://{rest}"));
            let iframe_url = Url::parse(&iframe_src).map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid player iframe URL: {e}"))
            })?;

            // `.server` elements: `$(el).text()` and `data('hash')`,
            // filtered to `serverName === 'CloudStream Pro'`. Servers
            // without a data-hash cannot build an rcp URL (upstream
            // would fetch `/rcp/undefined`).
            let servers: Vec<(String, String)> = document
                .select(&SERVER)
                .filter_map(|element| {
                    let server_name = element.text().collect::<String>();
                    if server_name != "CloudStream Pro" {
                        return None;
                    }
                    let data_hash = element.value().attr("data-hash")?.to_string();
                    Some((server_name, data_hash))
                })
                .collect();
            (iframe_url, servers)
        };

        let mut streams = Vec::new();
        for (server_name, data_hash) in &servers {
            let stream = self
                .cloudstream_stream(ctx, &iframe_url, mirror, data_hash, server_name)
                .await?;
            streams.push(stream);
        }

        if streams.is_empty() {
            return Err(ExtractorError::NotFound);
        }
        Ok(streams)
    }

    /// Resolve one `CloudStream Pro` server: `/rcp/{hash}` → the player
    /// page → the `{vN}`-templated m3u8.
    async fn cloudstream_stream(
        &self,
        ctx: &ResolveCtx<'_>,
        iframe_url: &Url,
        mirror: &Url,
        data_hash: &str,
        server_name: &str,
    ) -> Result<Stream, ExtractorError> {
        // `new URL(\`/rcp/${dataHash}\`, iframeUrl.origin)`.
        let rcp_url = iframe_url
            .join(&format!("/rcp/{data_hash}"))
            .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid rcp URL: {e}")))?;
        // The rcp page is fetched with the mirror's origin as Referer.
        let iframe_html = fetch_text_with_referer(ctx, &rcp_url, &origin_of(mirror)).await?;

        // `iframeHtml.match(\`src:\\\\s?'(.*)'\`)` — a missing src is the
        // upstream's NotFoundError.
        let src_path = first_capture(&SRC_IN_IFRAME, &iframe_html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or(ExtractorError::NotFound)?;
        // `new URL(srcPath, iframeUrl.origin)`.
        let player_url = Url::parse(&origin_of(iframe_url))
            .and_then(|base| base.join(&src_path))
            .map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid player URL: {e}"))
            })?;
        let player_html = fetch_text_with_referer(ctx, &player_url, rcp_url.as_str()).await?;

        // `playerHtml.match(\`(https:\\\\/\\\\/.*?{v\\\\d}.*?) or\`)` —
        // the file URL with its `{vN}` host token; a missing file is the
        // upstream's NotFoundError.
        let file_url = first_capture(&FILE_IN_PLAYER, &player_html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or(ExtractorError::NotFound)?;
        // `fileUrl.replace(/{v\\d}/, iframeUrl.host)`.
        let replaced = replace_v_token(&file_url, iframe_url.host_str().unwrap_or_default());
        let m3u8_url = Url::parse(&replaced)
            .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid file URL: {e}")))?;

        // Best-effort height from the playlist (Referer: the iframe
        // page), ports `guessHeightFromPlaylist`.
        let headers = BTreeMap::from([("Referer".to_string(), iframe_url.to_string())]);
        let resolution = guess_height_from_playlist(ctx, &m3u8_url, &headers).await;

        let meta = StreamMeta {
            resolution,
            ..StreamMeta::default()
        };
        let mut stream = Stream::new(m3u8_url, Format::Hls)
            .with_label(server_name)
            .with_ttl(TTL);
        stream.meta = meta;
        Ok(stream)
    }
}

/// Fetch the embed page from a mirror (the `queueLimit: 1` fetch).
async fn fetch_embed_page(ctx: &ResolveCtx<'_>, url: &Url) -> Result<String, ExtractorError> {
    let mut request = FetchRequest::get(url.clone());
    request.queue_limit = Some(1);
    Ok(ctx.fetcher.request(request).await?.body)
}

/// Fetch `url` with an explicit `Referer` header value.
async fn fetch_text_with_referer(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &str,
) -> Result<String, ExtractorError> {
    let request = FetchRequest::get(url.clone()).with_header("Referer", referer);
    Ok(ctx.fetcher.request(request).await?.body)
}

/// Ports `guessHeightFromPlaylist` from `src/utils/height.js`: the max
/// `WxH`/`NNNp` height advertised in the playlist. Best-effort — errors
/// map to `None` like the upstream try/catch.
async fn guess_height_from_playlist(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    headers: &BTreeMap<String, String>,
) -> Option<u16> {
    let mut request = FetchRequest::get(url.clone());
    for (name, value) in headers {
        request = request.with_header(name, value);
    }
    let playlist = ctx.fetcher.request(request).await.ok()?.body;
    let mut best: Option<u16> = None;
    for captures in HEIGHT.captures_iter(&playlist).flatten() {
        let height = captures
            .get(1)
            .or_else(|| captures.get(2))
            .and_then(|group| group.as_str().parse::<u16>().ok());
        if let Some(height) = height {
            best = Some(best.map_or(height, |current: u16| current.max(height)));
        }
    }
    best
}

/// Replace the first `{vN}` token with `host` — ports
/// `fileUrl.replace(/{v\\d}/, iframeUrl.host)`.
fn replace_v_token(file_url: &str, host: &str) -> String {
    match V_TOKEN.find(file_url) {
        Ok(Some(token)) => {
            format!(
                "{}{host}{}",
                &file_url[..token.start()],
                &file_url[token.end()..]
            )
        }
        _ => file_url.to_string(),
    }
}

/// WHATWG `url.origin` as a string (`scheme://host[:port]`).
fn origin_of(url: &Url) -> String {
    format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default())
}

/// A pseudo-random index for the mirror rotation (ports
/// `Math.floor(Math.random() * domains.length)` — timestamp entropy like
/// the workspace's `random_token`).
fn random_index(len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |elapsed| elapsed.as_nanos());
    usize::try_from(nanos % len as u128).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use vsources_core::error::BlockedReason;
    use vsources_core::traits::{FetchResponse, Fetcher};

    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://vidsrc.to/embed/movie/tt0944947")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// The embed page: the iframe (whose origin hosts the rcp/player
    /// pages) and the server list — the `CloudStream Pro` markup hidden
    /// in an HTML comment (the upstream comment-strip "uncomments" it —
    /// that is the point of the replace), plus a non-Pro server.
    const EMBED_PAGE: &str = r#"<html><head><title>Example Movie (2023)</title></head><body>
<!-- <div class="server" data-hash="8f3d2a">CloudStream Pro</div> -->
<iframe id="player_iframe" src="//vidsrc.dev/api/iframe/0/1/tt0944947"></iframe>
<div class="server" data-hash="free01">CloudStream Free</div>
</body></html>"#;

    /// The `/rcp/{hash}` page: `src: '…'` pointing at the player page.
    const RCP_PAGE: &str = r"<html><body><script>var player = {hash: '8f3d2a', src: '/vsrc/prod/tt0944947-8f3d2a'}</script></body></html>";

    /// The player page: the file URL with its `{v1}` host token and the
    /// ` or`-separated fallback.
    const PLAYER_PAGE: &str = r"<html><body><script>var sources = 'https://{v1}/vsrc/tt0944947/master.m3u8 or fallback.mp4'</script></body></html>";

    const PLAYLIST: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1280x720\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=854x480\n480.m3u8\n";

    fn vidsrc_fixtures() -> ScriptedFetcher {
        ScriptedFetcher::default()
            .page("/embed/movie/tt0944947", EMBED_PAGE)
            .page("/rcp/8f3d2a", RCP_PAGE)
            .page("/vsrc/prod/tt0944947-8f3d2a", PLAYER_PAGE)
            .page("/vsrc/tt0944947/master.m3u8", PLAYLIST)
    }

    #[test]
    fn matches_the_vidsrc_family() {
        let extractor = VidSrc::default();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/embed/movie/tt0944947"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("vidsrc.to"));
        assert!(supports("vidsrc.mov"));
        assert!(supports("vidsrc-embed.ru"));
        assert!(supports("vsrc.cc"));
        assert!(supports("vsembed.com"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn resolves_the_cloudstream_pro_chain() {
        let fetcher = vidsrc_fixtures();
        let ctx = ctx_for(&fetcher, None);

        let streams = VidSrc::with_domains(vec!["vidsrc.to".to_string()])
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the CloudStream Pro chain must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://vidsrc.dev/vsrc/tt0944947/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL);
        assert_eq!(stream.label.as_deref(), Some("CloudStream Pro"));
        assert_eq!(stream.meta.resolution, Some(720));
        // The upstream result carries no hotlink headers.
        assert!(stream.meta.request_headers.is_empty());

        // The wire shape: mirror origin on /rcp, the rcp href on the
        // player page, the iframe href on the playlist, and the
        // queue-limited embed fetch.
        assert_eq!(
            fetcher.sent_header("/rcp/8f3d2a", "Referer").as_deref(),
            Some("https://vidsrc.to")
        );
        assert_eq!(
            fetcher
                .sent_header("/vsrc/prod/tt0944947-8f3d2a", "Referer")
                .as_deref(),
            Some("https://vidsrc.dev/rcp/8f3d2a")
        );
        assert_eq!(
            fetcher
                .sent_header("/vsrc/tt0944947/master.m3u8", "Referer")
                .as_deref(),
            Some("https://vidsrc.dev/api/iframe/0/1/tt0944947")
        );
        assert_eq!(
            fetcher.requests().first().and_then(|r| r.queue_limit),
            Some(1)
        );
    }

    #[tokio::test]
    async fn pages_without_the_player_iframe_fail() {
        let fetcher = ScriptedFetcher::default().page(
            "/embed/movie/tt0944947",
            "<html><body>no player here</body></html>",
        );
        let ctx = ctx_for(&fetcher, None);

        match VidSrc::with_domains(vec!["vidsrc.to".to_string()])
            .extract(&ctx, &url())
            .await
        {
            Err(ExtractorError::Extraction { .. }) => {}
            other => {
                panic!("a page without #player_iframe must be an extraction error, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn rcp_pages_without_a_src_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/embed/movie/tt0944947", EMBED_PAGE)
            .page("/rcp/8f3d2a", "<html>nothing here</html>");
        let ctx = ctx_for(&fetcher, None);

        match VidSrc::with_domains(vec!["vidsrc.to".to_string()])
            .extract(&ctx, &url())
            .await
        {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a src-less rcp page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retries_blocked_mirrors() {
        /// Serves a Cloudflare block for the first mirror request, then
        /// delegates to the scripted pages.
        struct BlockedFirst {
            pages: ScriptedFetcher,
            served: AtomicBool,
        }

        #[async_trait]
        impl Fetcher for BlockedFirst {
            async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
                if !self.served.swap(true, Ordering::SeqCst) {
                    return Err(FetchError::Blocked {
                        url: request.url.clone(),
                        reason: BlockedReason::CloudflareChallenge,
                    });
                }
                self.pages.request(request).await
            }
        }

        let fetcher = BlockedFirst {
            pages: vidsrc_fixtures(),
            served: AtomicBool::new(false),
        };
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: None,
        };

        let streams = VidSrc::with_domains(vec!["vidsrc.to".to_string(), "vidsrc.net".to_string()])
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the mirror retry must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://vidsrc.dev/vsrc/tt0944947/master.m3u8",
        );
    }
}
