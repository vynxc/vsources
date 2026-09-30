//! `AnimeDirect`: passthrough and page-scraper for direct anime
//! streams.
//!
//! Ports `src/extractor/AnimeDirect.js` (display label `Anime`, as
//! upstream). Three URL families, in upstream's order:
//!
//! 1. **Direct HLS CDNs** — exact hosts (`play.zephyrix.top`,
//!    `prox.anicore.tv`, `playeng.animeapps.top`, the `megap.*` `AniVault`
//!    CDNs) and suffixes (`.dramiyos-cdn.com`, `.harborlanecreativeworks.space`,
//!    `.pinecliffdesigncollective.store`, `.creativewritingtips.site`,
//!    `.savannahridgedesignlab.cyou`, `.netrocdn.site`) pass through
//!    with a per-host Referer, or the Referer the source supplied.
//! 2. **Netlio path-marker URLs** (`cf-master` / `/v4/` / `/hls3/`) —
//!    passed through with the Netlio Referer.
//! 3. **Embed pages** (`gn1r5n.org`, `playmogo.com`, `gogoanime.com.by`)
//!    — the HTML is scraped for the real stream URL, following
//!    `source:`/`file:` initializers, a second scrape when the found URL
//!    is itself a gogoanime page, and a `megaplay.buzz` iframe detour
//!    resolved through the `getSourcesNew` API (with a best-effort
//!    playlist probe for the variant resolution).
//!
//! Upstream wrapped every result in a server-side `/proxy` that sent the
//! Referer and rewrote playlist URLs; this library has no server, so the
//! direct URL ships with the Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! (only the `workers.dev`/`cloudflare` results carried one upstream —
//! the embed's origin). Fetch failures surface as typed
//! [`ExtractorError::Fetch`] where upstream swallowed them into an empty
//! result — the registry's chain keeps going either way; pattern misses
//! stay [`ExtractorError::NotFound`].

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::first_capture;
use crate::hosts::netlio::has_netlio_path_marker;

/// Upstream result lifetime: 1h.
const TTL: Duration = Duration::from_hours(1);
/// Upstream id, for typed extraction errors.
const ID: &str = "animedirect";
/// Upstream display label.
const LABEL: &str = "Anime";

/// The browser User-Agent for the megaplay resolution hops.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Referer sent when fetching embed pages.
const EMBED_REFERER: &str = "https://hianime.win/";
/// Referer for megaplay.buzz stream results.
const MEGAPLAY_REFERER: &str = "https://megaplay.buzz/";
/// Referer for Netlio path-marker URLs.
const NETLIO_REFERER: &str = "https://netlio.vercel.app/";
/// The megaplay.buzz API endpoint.
const MEGAPLAY_API: &str = "https://megaplay.buzz/stream/getSourcesNew";

/// Direct HLS CDN hosts — passed through as-is, with a Referer.
const DIRECT_HLS_HOSTS: &[&str] = &[
    "play.zephyrix.top",
    // AniKage — prox.anicore.tv serves direct HLS (Referer: anikage.cc).
    "prox.anicore.tv",
    // AniBD — playeng.animeapps.top serves direct HLS (Referer: anibd.app).
    "playeng.animeapps.top",
    // AniVault — megap.* serves direct HLS (Referer: megaplay.buzz);
    // megap.akirax.buzz is also used by 2Dhive + StreamXTV.
    "megap.norami.top",
    "megap.shiora.top",
    "megap.shiora.site",
    "megap.mikora.top",
    "megap.akirax.buzz",
];

/// CDN host suffixes that serve direct HLS (`AniNeko` + Netlio CDNs).
const DIRECT_HLS_SUFFIXES: &[&str] = &[
    ".dramiyos-cdn.com",
    ".harborlanecreativeworks.space",
    ".pinecliffdesigncollective.store",
    ".creativewritingtips.site",
    ".savannahridgedesignlab.cyou",
    ".netrocdn.site",
];

/// Embed page hosts — the HTML is scraped for the actual stream URL.
const EMBED_HOSTS: &[&str] = &["gn1r5n.org", "playmogo.com", "gogoanime.com.by"];

/// A megaplay.buzz iframe on an embed page (gogoanime embeds).
static MEGAPLAY_IFRAME: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)<iframe[^>]+src=["'](https://megaplay\.buzz/stream/[^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid megaplay iframe pattern: {e}"))
});
/// A bare HLS URL in a player page.
static HLS_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)https?://[^"'\s<>]+\.m3u8[^"'\s<>]*"#)
        .unwrap_or_else(|e| panic!("valid hls pattern: {e}"))
});
/// A bare MP4 URL in a player page.
static MP4_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)https?://[^"'\s<>]+\.mp4[^"'\s<>]*"#)
        .unwrap_or_else(|e| panic!("valid mp4 pattern: {e}"))
});
/// A `source: "…"` player initializer.
static SOURCE_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)source:\s*["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid source pattern: {e}"))
});
/// A `file: "…"` player initializer.
static FILE_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)file:\s*["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
/// A Dean-Edwards-packed player upstream cannot evaluate here.
static EVAL_PACKED: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?s)eval\(function\(p,a,c,k,e,d\).*?\)\)")
        .unwrap_or_else(|e| panic!("valid eval pattern: {e}"))
});
/// The megaplay data-id on an embed page.
static DATA_ID: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"data-id="(\d+)""#)
        .unwrap_or_else(|e| panic!("valid data-id pattern: {e}"))
});
/// A playlist variant resolution.
static PLAYLIST_RESOLUTION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)RESOLUTION=\d+x(\d+)")
        .unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// The `AnimeDirect` extractor.
#[derive(Debug, Default)]
pub struct AnimeDirect;

impl AnimeDirect {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for AnimeDirect {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        LABEL
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        is_direct_hls(url) || is_embed_page(url) || has_netlio_path_marker(url)
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Direct HLS — passed through with the source's Referer or the
        // one inferred from the host.
        if is_direct_hls(url) {
            let referer = ctx.referer.map_or_else(
                || infer_direct_hls_referer(url).to_string(),
                ToString::to_string,
            );
            let stream = Stream::new(url.clone(), Format::Hls)
                .with_label(LABEL)
                .with_ttl(TTL)
                .with_referer(referer);
            return Ok(vec![stream]);
        }

        // Netlio path-marker URLs — the Netlio Referer.
        if has_netlio_path_marker(url) {
            let stream = Stream::new(url.clone(), Format::Hls)
                .with_label(LABEL)
                .with_ttl(TTL)
                .with_referer(NETLIO_REFERER);
            return Ok(vec![stream]);
        }

        // Embed pages — scrape the actual stream URL out of the HTML.
        if is_embed_page(url) {
            return extract_embed_page(ctx, url).await;
        }

        // Nothing matched — upstream's empty result.
        Err(ExtractorError::NotFound)
    }
}

/// Whether `url` is served by a direct HLS CDN host (exact or suffix
/// match).
#[must_use]
fn is_direct_hls(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    DIRECT_HLS_HOSTS.contains(&host)
        || DIRECT_HLS_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

/// Whether `url` is an embed page to scrape.
#[must_use]
fn is_embed_page(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| EMBED_HOSTS.contains(&host))
}

/// The Referer a direct HLS CDN requires, inferred from its host.
///
/// Upstream let the source override it via `meta.requestHeaders.Referer`
/// — that maps to the context's embed Referer in `extract`.
#[must_use]
fn infer_direct_hls_referer(url: &Url) -> &'static str {
    let host = url.host_str().unwrap_or_default();
    if host == "play.zephyrix.top" {
        "https://play.zephyrix.top/"
    } else if host == "prox.anicore.tv" {
        "https://anikage.cc/"
    } else if host == "playeng.animeapps.top" {
        "https://anibd.app/"
    } else if host.ends_with(".netrocdn.site") {
        "https://vidspark.to/"
    } else if host.starts_with("megap.") {
        "https://megaplay.buzz/"
    } else {
        "https://anineko.to/"
    }
}

/// The origin of `url` (`scheme://host/`), upstream's `url.origin + '/'`.
fn origin_slash(url: &Url) -> String {
    format!("{}://{}/", url.scheme(), url.host_str().unwrap_or_default())
}

/// Scrape the stream URL out of an embed page.
///
/// Ports the `isEmbedPage` branch: fetch the page, try a megaplay
/// iframe, scan for the stream URL, scrape one level deeper when that
/// URL is itself a gogoanime page, and wrap `workers.dev`/`cloudflare`
/// results with the embed's origin as Referer. Unparsable URLs, missing
/// patterns, and packed eval players are upstream's empty results →
/// `NotFound`.
async fn extract_embed_page(
    ctx: &ResolveCtx<'_>,
    url: &Url,
) -> Result<Vec<Stream>, ExtractorError> {
    let html = fetch_text(ctx, url, EMBED_REFERER, Duration::from_secs(10)).await?;

    // gogoanime embeds carry a megaplay.buzz iframe.
    if let Some(mega_url) = find_megaplay_iframe(&html)?
        && let Some(stream) = resolve_megaplay(ctx, &mega_url).await
    {
        return Ok(vec![stream]);
    }

    if let Some(stream_url) = scan_stream_url(&html)? {
        // Upstream's `new URL(streamUrl)` throw is an empty result.
        let mut parsed = Url::parse(&stream_url).map_err(|_| ExtractorError::NotFound)?;

        // A gogoanime stream URL is itself a page to scrape. (Upstream
        // also matched a literal "streaming.php" inside the hostname —
        // verbatim port of the quirk.)
        let host = parsed.host_str().unwrap_or_default();
        if host.contains("gogoanime") || host.contains("streaming.php") {
            let referer = origin_slash(url);
            let stream_html = fetch_text(ctx, &parsed, &referer, Duration::from_secs(10)).await?;
            if let Some(mega_url) = find_megaplay_iframe(&stream_html)?
                && let Some(stream) = resolve_megaplay(ctx, &mega_url).await
            {
                return Ok(vec![stream]);
            }
            let inner_url = scan_stream_url(&stream_html)?.ok_or(ExtractorError::NotFound)?;
            parsed = Url::parse(&inner_url).map_err(|_| ExtractorError::NotFound)?;
        }

        let format = if parsed.as_str().contains(".m3u8") {
            Format::Hls
        } else {
            Format::Mp4
        };

        // workers.dev / cloudflare hosts were proxied with the embed's
        // origin as the Referer; the port attaches it as a request
        // header instead.
        let needs_origin_referer = parsed
            .host_str()
            .is_some_and(|host| host.ends_with(".workers.dev") || host.contains("cloudflare"));
        let mut stream = Stream::new(parsed, format).with_label(LABEL).with_ttl(TTL);
        if needs_origin_referer {
            stream = stream.with_referer(origin_slash(url));
        }
        return Ok(vec![stream]);
    }

    // Packed eval players cannot be evaluated here — upstream's
    // explicit empty result.
    if EVAL_PACKED.is_match(&html).unwrap_or(false) {
        return Err(ExtractorError::NotFound);
    }

    Err(ExtractorError::NotFound)
}

/// The first stream URL in a player page: a bare HLS link, a bare MP4
/// link, or a `source:`/`file:` initializer (upstream's order).
fn scan_stream_url(text: &str) -> Result<Option<String>, ExtractorError> {
    if let Some(hls) = find_match(&HLS_URL, text)? {
        return Ok(Some(hls));
    }
    if let Some(mp4) = find_match(&MP4_URL, text)? {
        return Ok(Some(mp4));
    }
    if let Some(source) = first_capture(&SOURCE_URL, text)
        .map_err(|e| ExtractorError::extraction(ID, e.to_string()))?
    {
        return Ok(Some(source));
    }
    if let Some(file) =
        first_capture(&FILE_URL, text).map_err(|e| ExtractorError::extraction(ID, e.to_string()))?
    {
        return Ok(Some(file));
    }
    Ok(None)
}

/// The megaplay.buzz stream URL of an `<iframe>` in a player page.
///
/// Returns `None` when there is no iframe; an unparsable src is
/// upstream's `new URL(…)` throw → an empty result.
fn find_megaplay_iframe(text: &str) -> Result<Option<Url>, ExtractorError> {
    let Some(src) = first_capture(&MEGAPLAY_IFRAME, text)
        .map_err(|e| ExtractorError::extraction(ID, e.to_string()))?
    else {
        return Ok(None);
    };
    Url::parse(&src)
        .map(Some)
        .map_err(|_| ExtractorError::NotFound)
}

/// Resolve a megaplay.buzz embed URL to a direct playlist via the
/// `getSourcesNew` API (the same flow as the Megaplay extractor).
///
/// Every failure degrades to `None` — upstream's `catch { return null }`
/// — so the caller falls through to the page scan.
async fn resolve_megaplay(ctx: &ResolveCtx<'_>, mega_url: &Url) -> Option<Stream> {
    // The embed page carries the data-id.
    let page_request = FetchRequest::get(mega_url.clone())
        .with_header("User-Agent", UA)
        .with_header("Referer", EMBED_REFERER)
        .with_timeout(Duration::from_secs(15));
    let page = ctx.fetcher.request(page_request).await.ok()?;
    if page.status != 200 {
        return None;
    }
    let data_id = first_capture(&DATA_ID, &page.body).ok().flatten()?;

    // The getSourcesNew API.
    let api_url = Url::parse(&format!("{MEGAPLAY_API}?id={data_id}")).ok()?;
    let api_request = FetchRequest::get(api_url)
        .with_header("User-Agent", UA)
        .with_header("X-Requested-With", "XMLHttpRequest")
        .with_header("Referer", mega_url.as_str())
        .with_header("Accept", "application/json,text/plain,*/*")
        .with_timeout(Duration::from_secs(15));
    let api = ctx.fetcher.request(api_request).await.ok()?;
    if api.status != 200 {
        return None;
    }

    let data: serde_json::Value = serde_json::from_str(&api.body).ok()?;
    let file = data.get("sources")?.get("file")?.as_str()?;
    let playlist = Url::parse(file).ok()?;

    let mut stream = Stream::new(playlist.clone(), Format::Hls)
        .with_label(LABEL)
        .with_ttl(TTL)
        .with_referer(MEGAPLAY_REFERER);

    // A best-effort playlist probe for the variant resolution — not
    // critical when it fails.
    if let Some(height) = playlist_resolution(ctx, &playlist).await {
        stream.meta.resolution = Some(height);
    }
    Some(stream)
}

/// The first `RESOLUTION=WxH` height in a playlist, when reachable.
async fn playlist_resolution(ctx: &ResolveCtx<'_>, playlist: &Url) -> Option<u16> {
    let request = FetchRequest::get(playlist.clone())
        .with_header("User-Agent", UA)
        .with_header("Referer", MEGAPLAY_REFERER)
        .with_timeout(Duration::from_secs(10));
    let response = ctx.fetcher.request(request).await.ok()?;
    if response.status != 200 {
        return None;
    }
    first_capture(&PLAYLIST_RESOLUTION, &response.body)
        .ok()
        .flatten()
        .and_then(|height| height.parse().ok())
}

/// Fetch a page as text with a Referer and timeout.
async fn fetch_text(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &str,
    timeout: Duration,
) -> Result<String, ExtractorError> {
    let request = FetchRequest::get(url.clone())
        .with_header("Referer", referer)
        .with_timeout(timeout);
    Ok(ctx.fetcher.request(request).await?.body)
}

/// The first whole-pattern match of `regex` in `text`.
fn find_match(regex: &fancy_regex::Regex, text: &str) -> Result<Option<String>, ExtractorError> {
    regex
        .find(text)
        .map(|found| found.map(|match_| match_.as_str().to_string()))
        .map_err(|e| ExtractorError::extraction(ID, e.to_string()))
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, ctx_for};

    use super::*;
    use vsources_core::types::Format;

    fn embed_url() -> Url {
        Url::parse("https://gn1r5n.org/embed/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn claims_direct_cdn_embed_and_netlio_urls() {
        let extractor = AnimeDirect::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |url: &str| {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };

        // Direct HLS hosts, exact and suffix.
        assert!(supports("https://play.zephyrix.top/hls/master.m3u8"));
        assert!(supports("https://megap.akirax.buzz/file.m3u8"));
        assert!(supports("https://x.dramiyos-cdn.com/abc/playlist.m3u8"));
        assert!(supports("https://cdn.netrocdn.site/abc.m3u8"));
        // Embed pages.
        assert!(supports("https://gn1r5n.org/embed/abc"));
        assert!(supports("https://gogoanime.com.by/watch/abc"));
        // Netlio path markers.
        assert!(supports("https://x.example.com/v4/abc.m3u8"));
        assert!(supports("https://x.example.com/hls3/abc.m3u8"));
        // Not claimed.
        assert!(!supports("https://example.com/plain/page.html"));
        assert!(!supports("https://example.com/v5/abc.m3u8"));
    }

    #[tokio::test]
    async fn passes_direct_hls_through_with_the_inferred_referer() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let extractor = AnimeDirect::new();

        let cases = [
            (
                "https://play.zephyrix.top/hls/master.m3u8",
                "https://play.zephyrix.top/",
            ),
            ("https://prox.anicore.tv/hls/x.m3u8", "https://anikage.cc/"),
            (
                "https://playeng.animeapps.top/hls/x.m3u8",
                "https://anibd.app/",
            ),
            (
                "https://cdn.netrocdn.site/hls/x.m3u8",
                "https://vidspark.to/",
            ),
            (
                "https://megap.shiora.top/hls/x.m3u8",
                "https://megaplay.buzz/",
            ),
            (
                "https://x.dramiyos-cdn.com/hls/x.m3u8",
                "https://anineko.to/",
            ),
        ];
        for (url, referer) in cases {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("direct HLS must pass through: {e}"));
            assert_eq!(streams.len(), 1, "one stream for {url}");
            let stream = &streams[0];
            assert_eq!(stream.format, Format::Hls);
            assert_eq!(stream.label.as_deref(), Some(LABEL));
            assert_eq!(stream.ttl, TTL);
            assert_eq!(
                stream
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some(referer)
            );
        }
        assert!(
            fetcher.requests().is_empty(),
            "the passthrough must not fetch"
        );
    }

    #[tokio::test]
    async fn prefers_the_context_referer_for_direct_hls() {
        let fetcher = ScriptedFetcher::default();
        let referer =
            Url::parse("https://anineko.to/watch/xyz").unwrap_or_else(|e| panic!("valid URL: {e}"));
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: Some(&referer),
        };
        let url = Url::parse("https://x.dramiyos-cdn.com/hls/x.m3u8")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = AnimeDirect::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("direct HLS must pass through: {e}"));
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://anineko.to/watch/xyz")
        );
    }

    #[tokio::test]
    async fn passes_netlio_marker_urls_with_the_netlio_referer() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://aurorionacademy.site/cf-master/abc.m3u8")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = AnimeDirect::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("netlio markers must pass through: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(NETLIO_REFERER)
        );
        assert!(
            fetcher.requests().is_empty(),
            "the passthrough must not fetch"
        );
    }

    #[tokio::test]
    async fn scrapes_the_stream_url_from_an_embed_page() {
        let fetcher = ScriptedFetcher::default().page(
            "/embed/abc123",
            r#"<html><script>var player = { file: "https://cdn.example.net/hls/ep1.m3u8" };</script></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeDirect::new()
            .extract(&ctx, &embed_url())
            .await
            .unwrap_or_else(|e| panic!("the embed page must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.net/hls/ep1.m3u8"
        );
        assert_eq!(streams[0].format, Format::Hls);
        assert_eq!(streams[0].label.as_deref(), Some(LABEL));
        assert_eq!(streams[0].ttl, TTL);
        // The embed fetch carried the upstream Referer.
        assert_eq!(
            fetcher.sent_header("/embed/abc123", "Referer").as_deref(),
            Some(EMBED_REFERER)
        );
        // A plain CDN host: no request headers on the result.
        assert!(streams[0].meta.request_headers.is_empty());
    }

    #[tokio::test]
    async fn scrapes_source_and_mp4_initializers_too() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/embed/one",
                r#"<script>source: "https://cdn.example.net/ep1.mp4"</script>"#,
            )
            .page(
                "/embed/two",
                r"<script>player.file = 'https://cdn.example.net/ep2.mp4';</script>",
            );
        let ctx = ctx_for(&fetcher, None);
        let extractor = AnimeDirect::new();

        for path in ["/embed/one", "/embed/two"] {
            let url = Url::parse(&format!("https://gn1r5n.org{path}"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("the initializer scan must resolve: {e}"));
            assert_eq!(streams.len(), 1, "one stream for {path}");
            assert_eq!(streams[0].format, Format::Mp4);
            assert!(
                streams[0]
                    .url
                    .as_str()
                    .starts_with("https://cdn.example.net/ep")
            );
        }
    }

    #[tokio::test]
    async fn scrapes_one_level_deeper_for_gogoanime_pages() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/embed/abc123",
                r#"<script>var player = { file: "https://stream76.gogoanime.net/v/123" };</script>"#,
            )
            .page(
                "/v/123",
                r#"<script>jwplayer("player").setup({ file: "https://cdn.example.net/hls/inner.m3u8" });</script>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeDirect::new()
            .extract(&ctx, &embed_url())
            .await
            .unwrap_or_else(|e| panic!("the two-level scrape must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.net/hls/inner.m3u8"
        );
        assert_eq!(streams[0].format, Format::Hls);
        // The inner scrape carried the embed's origin as its Referer.
        assert_eq!(
            fetcher.sent_header("/v/123", "Referer").as_deref(),
            Some("https://gn1r5n.org/")
        );
    }

    #[tokio::test]
    async fn wraps_worker_and_cloudflare_results_with_the_embed_origin() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/embed/abc123",
                r#"<script>var player = { file: "https://player-api.example.workers.dev/v4/x.m3u8" };</script>"#,
            )
            .page(
                "/embed/def456",
                r#"<script>var player = { file: "https://files.cloudflarestorage.example/x.m3u8" };</script>"#,
            );
        let ctx = ctx_for(&fetcher, None);
        let extractor = AnimeDirect::new();

        for path in ["/embed/abc123", "/embed/def456"] {
            let url = Url::parse(&format!("https://playmogo.com{path}"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("the worker wrap must resolve: {e}"));
            assert_eq!(streams.len(), 1, "one stream for {path}");
            assert_eq!(
                streams[0]
                    .meta
                    .request_headers
                    .get("Referer")
                    .map(String::as_str),
                Some("https://playmogo.com/")
            );
        }
    }

    #[tokio::test]
    async fn resolves_megaplay_iframes_through_the_api() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/embed/abc123",
                r#"<iframe src="https://megaplay.buzz/stream/s-2/12345/sub" allowfullscreen></iframe>"#,
            )
            .page("/stream/s-2/12345/sub", r#"<div class="player" data-id="12345"></div>"#)
            .page(
                "/stream/getSourcesNew",
                r#"{"sources":{"file":"https://cdn.megaplay.example/hls/master.m3u8"}}"#,
            )
            .page(
                "/hls/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1920x1080\n1080.m3u8\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeDirect::new()
            .extract(&ctx, &embed_url())
            .await
            .unwrap_or_else(|e| panic!("the megaplay flow must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://cdn.megaplay.example/hls/master.m3u8"
        );
        assert_eq!(stream.format, Format::Hls);
        assert_eq!(stream.label.as_deref(), Some(LABEL));
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(MEGAPLAY_REFERER)
        );
        assert_eq!(stream.meta.resolution, Some(1080));

        // The wire requests match the upstream flow.
        assert_eq!(
            fetcher
                .sent_header("/stream/s-2/12345/sub", "Referer")
                .as_deref(),
            Some(EMBED_REFERER)
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/s-2/12345/sub", "User-Agent")
                .as_deref(),
            Some(UA)
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/getSourcesNew", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/getSourcesNew", "Referer")
                .as_deref(),
            Some("https://megaplay.buzz/stream/s-2/12345/sub")
        );
        assert_eq!(
            fetcher
                .sent_header("/hls/master.m3u8", "Referer")
                .as_deref(),
            Some(MEGAPLAY_REFERER)
        );
    }

    #[tokio::test]
    async fn megaplay_failures_fall_through_to_the_page_scan() {
        // The iframe resolves, but the embed page has no data-id → the
        // API is never called and the page scan picks up the file.
        let fetcher = ScriptedFetcher::default()
            .page(
                "/embed/abc123",
                r#"<iframe src="https://megaplay.buzz/stream/s-2/12345/sub"></iframe><script>file: "https://cdn.example.net/hls/ep1.m3u8"</script>"#,
            )
            .page("/stream/s-2/12345/sub", "<p>no id here</p>");
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeDirect::new()
            .extract(&ctx, &embed_url())
            .await
            .unwrap_or_else(|e| panic!("the page scan must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://cdn.example.net/hls/ep1.m3u8"
        );
        // The megaplay page was fetched, but the API never was.
        assert!(
            !fetcher.requests().iter().any(|request| {
                request.url.host_str() == Some("megaplay.buzz")
                    && request.url.path() == "/stream/getSourcesNew"
            }),
            "the API must not be called without a data-id"
        );
    }

    #[tokio::test]
    async fn embed_pages_without_streams_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/embed/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match AnimeDirect::new().extract(&ctx, &embed_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn packed_eval_pages_are_misses() {
        let fetcher = ScriptedFetcher::default().page(
            "/embed/abc123",
            r"<script>eval(function(p,a,c,k,e,d){ /* packed */ }()))</script>",
        );
        let ctx = ctx_for(&fetcher, None);

        match AnimeDirect::new().extract(&ctx, &embed_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a packed page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparsable_scraped_urls_are_misses() {
        // A relative file: value cannot become an absolute URL —
        // upstream's `new URL(...)` throw degrades to an empty result.
        let fetcher = ScriptedFetcher::default()
            .page("/embed/abc123", r#"<script>file: "get/file/123"</script>"#);
        let ctx = ctx_for(&fetcher, None);

        match AnimeDirect::new().extract(&ctx, &embed_url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an unparsable URL must be a NotFound, got {other:?}"),
        }
    }
}
