//! `Pantyflix`: direct-download passthrough that resolves the
//! `fastdlserver` redirect chain first.
//!
//! Ports `src/extractor/Pantyflix.js`. Claims URLs from the `pantyflix`
//! and `bollyflix` sources — both serve `dl.fastdlserver.site` links.
//! Those resolve through a chain (as of upstream's notes):
//! `fastdlserver.site/?id={base64}` → 302 → `gdflix.dev/file/{id}` (an
//! HTML page with an "Instant DL" button) → `instant.busycdn.xyz/{hash}
//! ::{hash}?bytes={size}` → 302 → `fastdl-one.pages.dev/?url={direct}`,
//! where the `url` query parameter is the direct playable URL (often on
//! `googleusercontent.com`). Legacy `cloud-dl` links on the `/file/`
//! page are already direct. Everything else — `googleusercontent`,
//! `workers.dev`, `hakunaymatata` — passes through untouched, inferring
//! HLS from `.m3u8`/`/hls/` paths.
//!
//! Cut for the library port: the module-level 5-minute resolve cache
//! (the registry's result cache already covers it) and every
//! server-side proxy hop. Upstream wrapped `googleusercontent.com`
//! hosts in `/range-proxy` (Google ignores `Range` requests, breaking
//! seek) and the `animeshrine|valentine|fukggl` CDNs in `/proxy` (they
//! reset connections on direct access); this library has no server, so
//! those URLs ship directly with the browser `User-Agent` those hops
//! sent in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! — Range translation and TLS-fingerprint handling are the player's
//! concern now. When the chain stalls, upstream still returned the
//! original URL wrapped in `/proxy`; the port ships it unwrapped the
//! same way.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, FetchResponse, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::host_matcher;

// CDNs that fail with "Connection reset by peer" when fetched directly.
host_matcher!(NEEDS_PROXY, r"animeshrine|valentine|fukggl");

/// Upstream result lifetime: 30min.
const TTL: Duration = Duration::from_mins(30);
/// The browser User-Agent the proxy hops sent along the chain.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Referer for the `fastdlserver` → `/file/` page hop.
const BOLLYFLIX_REFERER: &str = "https://bollyflix.free/";
/// Referer for the `busycdn` redirect hop.
const GDFLIX_REFERER: &str = "https://new3.gdflix.io/";

/// The "Instant DL" button link on a `/file/` page.
static BUSYCDN_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)https://instant\.busycdn\.xyz/[^"'\s<>]+"#)
        .unwrap_or_else(|e| panic!("valid busycdn pattern: {e}"))
});
/// The legacy `/cflare/` download link on a `/file/` page.
static CLOUD_DL_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)https://cloud-dl[^"'\s<>]+"#)
        .unwrap_or_else(|e| panic!("valid cloud-dl pattern: {e}"))
});

/// The `Pantyflix` extractor.
#[derive(Debug, Default)]
pub struct Pantyflix;

impl Pantyflix {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Pantyflix {
    fn id(&self) -> &'static str {
        "pantyflix"
    }

    fn label(&self) -> &'static str {
        "Pantyflix"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
        // Claims both 'pantyflix' and 'bollyflix' — both sources serve
        // dl.fastdlserver.site URLs that need the chain resolved.
        ctx.source_id == Some(self.id()) || ctx.source_id == Some("bollyflix")
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let host = url.host_str().unwrap_or_default();

        // fastdlserver URLs resolve through the redirect chain first.
        if host.contains("fastdlserver") {
            // Upstream still shipped the original URL through /proxy when
            // the chain stalled, so a player could at least try it.
            let direct = resolve_fastdlserver(ctx, url)
                .await
                .unwrap_or_else(|| url.clone());
            return Ok(vec![proxied_stream(&direct)]);
        }

        // googleusercontent / reset-prone CDNs — upstream's proxy hops.
        if host.contains("googleusercontent.com") || NEEDS_PROXY.is_match(host).unwrap_or(false) {
            return Ok(vec![proxied_stream(url)]);
        }

        // Direct URL — works without a proxy; HLS only when the path
        // says so.
        let format = if url.path().contains(".m3u8") || url.path().contains("/hls/") {
            Format::Hls
        } else {
            Format::Mp4
        };
        Ok(vec![Stream::new(url.clone(), format).with_ttl(TTL)])
    }
}

/// A stream for a URL upstream routed through a server-side proxy hop:
/// the direct URL plus the browser `User-Agent` that hop sent.
fn proxied_stream(url: &Url) -> Stream {
    let mut stream = Stream::new(url.clone(), Format::Mp4).with_ttl(TTL);
    stream.meta = stream.meta.with_header("User-Agent", UA);
    stream
}

/// Resolve a `fastdlserver` URL to the direct playable URL.
///
/// Ports `resolveFastDlServer`: fetch the `/file/` page, find the
/// `instant.busycdn.xyz` link (the FULL URL — without its `?bytes=`
/// parameter busycdn answers 500), follow its 302 without actually
/// following it, and take the `url` query parameter of the
/// `fastdl-one.pages.dev` Location. Every failure — including fetch
/// errors, exactly like upstream's `try/catch` — degrades to `None`.
async fn resolve_fastdlserver(ctx: &ResolveCtx<'_>, url: &Url) -> Option<Url> {
    // Step 1: the /file/ page (redirects are followed).
    let page = chain_hop(ctx, url, BOLLYFLIX_REFERER, true).await.ok()?;
    if page.status >= 400 || page.body.is_empty() {
        return None;
    }

    // Step 2: the busycdn link, or the legacy cloud-dl one.
    let Some(busycdn) = find_match(&BUSYCDN_URL, &page.body) else {
        return find_match(&CLOUD_DL_URL, &page.body).and_then(|cloud| Url::parse(&cloud).ok());
    };
    let busycdn_url = Url::parse(&busycdn).ok()?;

    // Step 3: the 302 — only the Location header is wanted, so the hop
    // must not follow redirects.
    let redirect = chain_hop(ctx, &busycdn_url, GDFLIX_REFERER, false)
        .await
        .ok()?;
    if redirect.status != 302 {
        return None;
    }
    let location = redirect.header("location")?;
    let loc_url = Url::parse(location).ok()?;

    // Step 4: the 'url' query parameter is the direct playable URL.
    let download = loc_url
        .query_pairs()
        .find(|(key, _)| key == "url")
        .map(|(_, value)| value.to_string())?;
    if !download.starts_with("http") {
        return None;
    }
    Url::parse(&download).ok()
}

/// One hop of the resolution chain: browser UA, a Referer, and a 10s
/// cap. Redirects are followed only when `follow` is set — the busycdn
/// hop wants just the `Location` header.
async fn chain_hop(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &str,
    follow: bool,
) -> Result<FetchResponse, ExtractorError> {
    let mut request = FetchRequest::get(url.clone())
        .with_header("User-Agent", UA)
        .with_header("Referer", referer)
        .with_timeout(Duration::from_secs(10));
    if !follow {
        request = request.with_redirects_disabled();
    }
    Ok(ctx.fetcher.request(request).await?)
}

/// The first whole-pattern match of `regex` in `text`.
fn find_match(regex: &fancy_regex::Regex, text: &str) -> Option<String> {
    regex
        .find(text)
        .ok()
        .flatten()
        .map(|found| found.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://dl.fastdlserver.site/?id=YWJjMTIz")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    fn ctx_with_source<'a>(
        fetcher: &'a ScriptedFetcher,
        source_id: Option<&'a str>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            fetcher,
            media: None,
            source_id,
            referer: None,
        }
    }

    #[test]
    fn claims_the_pantyflix_and_bollyflix_sources() {
        let extractor = Pantyflix::new();
        let fetcher = ScriptedFetcher::default();
        let url = Url::parse("https://anything.example.com/x")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("pantyflix")), &url));
        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("bollyflix")), &url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("netlio")), &url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &url));
    }

    #[tokio::test]
    async fn resolves_the_legacy_cloud_dl_link() {
        let fetcher = ScriptedFetcher::default().page(
            "/",
            r#"<html><body><a href="https://cloud-dl.abcd.workers.dev/file/xyz">Instant DL</a></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = Pantyflix::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the cloud-dl link must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://cloud-dl.abcd.workers.dev/file/xyz",
        );
        assert_eq!(streams[0].ttl, TTL);
        // The wire request carried the upstream headers.
        assert_eq!(
            fetcher.sent_header("/", "Referer").as_deref(),
            Some(BOLLYFLIX_REFERER)
        );
        assert_eq!(fetcher.sent_header("/", "User-Agent").as_deref(), Some(UA));
    }

    #[tokio::test]
    async fn attempts_the_busycdn_hop_and_falls_back_when_it_stalls() {
        // Scripted fetchers cannot answer 302s, so the busycdn hop sees
        // a plain 200 and the chain stalls — upstream's own failure mode
        // for a busycdn that serves the file instead of redirecting.
        let fetcher = ScriptedFetcher::default()
            .page(
                "/",
                r#"<a href="https://instant.busycdn.xyz/9a1b2c3d4e::5f6a7b8c9d?bytes=314572800">Instant DL</a>"#,
            )
            .page("/9a1b2c3d4e::5f6a7b8c9d", "");
        let ctx = ctx_for(&fetcher, None);

        let streams = Pantyflix::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the stalled chain must still resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://dl.fastdlserver.site/?id=YWJjMTIz",
        );

        // The busycdn hop was attempted with its Referer and with
        // redirect following disabled.
        assert_eq!(
            fetcher
                .sent_header("/9a1b2c3d4e::5f6a7b8c9d", "Referer")
                .as_deref(),
            Some(GDFLIX_REFERER)
        );
        assert!(
            fetcher.requests().iter().any(|request| {
                request.url.path() == "/9a1b2c3d4e::5f6a7b8c9d" && request.max_redirects == Some(0)
            }),
            "the busycdn hop must not follow redirects"
        );
    }

    #[tokio::test]
    async fn passes_proxy_routed_urls_through_with_the_browser_ua() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let extractor = Pantyflix::new();

        let cases = [
            "https://video-downloads.googleusercontent.com/abc/video.mp4",
            "https://animeshrine.example.net/files/ep1.mkv",
            "https://valentine-cdn.example.net/files/ep1.mp4",
        ];
        for url in cases {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("proxy-routed URLs must pass through: {e}"));
            assert_direct_stream(&streams, Format::Mp4, url.as_str());
            assert_eq!(
                streams[0]
                    .meta
                    .request_headers
                    .get("User-Agent")
                    .map(String::as_str),
                Some(UA)
            );
            assert!(
                !streams[0].meta.request_headers.contains_key("Referer"),
                "upstream sent no Referer for these hops"
            );
        }
        assert!(
            fetcher.requests().is_empty(),
            "no fetch happens for direct URLs"
        );
    }

    #[tokio::test]
    async fn infers_the_format_for_direct_urls() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let extractor = Pantyflix::new();

        let cases = [
            ("https://cdn.example.net/hls/master.m3u8", Format::Hls),
            ("https://cdn.example.net/hls/playlist", Format::Hls),
            ("https://cdn.example.net/files/video.mp4", Format::Mp4),
            ("https://cdn.example.net/files/video.mkv", Format::Mp4),
        ];
        for (url, format) in cases {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("direct URLs must pass through: {e}"));
            assert_direct_stream(&streams, format, url.as_str());
            assert!(streams[0].meta.request_headers.is_empty());
        }
    }
}
