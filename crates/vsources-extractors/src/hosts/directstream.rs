//! `DirectStream`: passthrough for direct playable CDN URLs.
//!
//! Ports `src/extractor/DirectStream.js`. The catch-all for CDN hosts
//! no other extractor claims (`CineWave`'s `HdHub`, `Fmovies`, Pixeldrain,
//! Cloudflare R2, the `hakunaymatata.com` CDNs, the generic file
//! hosts, and more) plus Google's Range-unsupported `googleusercontent`
//! CDNs. Every URL ships as-is — no fetch, formats are inferred from
//! the path.
//!
//! Cut for the library port: upstream wrapped the Google hosts
//! (`video-downloads`/`lh3.googleusercontent.com`) in a `/range-proxy`
//! that translated their ignore-Range 200 responses into seekable 206s.
//! There is no server here, so both branches ship the direct URL and
//! Range translation is the player's concern. Source-provided request
//! headers (upstream `meta.requestHeaders`) map to the context's embed
//! Referer and land in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 1h.
const TTL: Duration = Duration::from_hours(1);
/// Upstream display label.
const LABEL: &str = "Direct";

/// Hosts that serve direct playable video files (exact matches).
const DIRECT_CDN_HOSTS: &[&str] = &[
    "streamx.me",
    "thefmovies.sbs",
    "pixeldrain.dev",
    "pixeldrain.com",
    "cdn.fsl-buckets.work",
    "cdn.fukggl.buzz",
    // MovieBox — direct MP4 on the hakunaymatata.com CDN (often
    // rate-limited upstream).
    "bcdnxw2.hakunaymatata.com",
    "bcdnxw.hakunaymatata.com",
    // VidLink — direct MP4 on bcdn.hakunaymatata.com (no Referer).
    "bcdn.hakunaymatata.com",
    "hbcdn.hakunaymatata.com",
    // Generic file hosts (linksdrive-style direct download CDNs).
    "fastdl.zip",
    "vcloud.zip",
    "filebee.xyz",
    "vikingfile.com",
    // VegaCatering — file hosts from nexdrive.
    "vcloud.fit",
    "new26.gdtot.dad",
    "gdtot.dad",
    // AniVault — AnimeHeaven direct MP4.
    "rt.animeheaven.me",
    "co.animeheaven.me",
    // Cinejoy — direct HLS (no Referer needed).
    "info.movieboxnoob.cc",
    // Stellar.rip — direct HLS (needs a browser UA upstream).
    "proxy2.heistotron.uk",
    // NowHDTime — direct HLS API (nhdapi.com/api/hls?t=...).
    "nhdapi.com",
    // AnimeSuge/NikaStream — direct HLS (Referer via request headers).
    "cdn.kryntal.top",
    // CinebyRocks — HLS proxy.
    "scraper.vidbolt.xyz",
    // VidHawk — direct HLS (public CDN, no Referer).
    "edge.vidhawk.buzz",
    // MovieLinkBD — direct MKV with native Range support.
    "cdn.dramalinkbd.tv",
];

/// Host suffixes for wildcard matching (`*.r2.dev`, …).
const DIRECT_CDN_SUFFIXES: &[&str] =
    &[".r2.dev", ".r2.cloudflarestorage.com", ".hakunaymatata.com"];

/// Google CDN hosts that ignore HTTP Range requests.
const NO_RANGE_HOSTS: &[&str] = &[
    "video-downloads.googleusercontent.com",
    "lh3.googleusercontent.com",
];

/// `hostname.endsWith(suffix)`.
fn ends_with_suffix(host: &str, suffix: &str) -> bool {
    host.strip_suffix(suffix)
        .is_some_and(|rest| !rest.is_empty())
}

/// Whether the URL sits on a directly playable CDN host (exact host or
/// wildcard suffix).
#[must_use]
fn is_direct_cdn_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    DIRECT_CDN_HOSTS.contains(&host)
        || DIRECT_CDN_SUFFIXES
            .iter()
            .any(|suffix| ends_with_suffix(host, suffix))
}

/// Whether the URL sits on a Google CDN that ignores Range requests.
#[must_use]
fn is_no_range_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // Exact, or `hostname.endsWith('.' + host)`.
    NO_RANGE_HOSTS.contains(&host)
        || NO_RANGE_HOSTS
            .iter()
            .any(|base| ends_with_suffix(host, &format!(".{base}")))
}

/// Ports `inferFormat`: `.m3u8` in the path → HLS (upstream's
/// ends-with check is subsumed by contains), a `.mp4`/`.mkv`/`.webm`
/// ending → MP4, everything else defaults to MP4.
#[must_use]
fn infer_format(url: &Url) -> Format {
    let path = url.path().to_ascii_lowercase();
    if path.contains(".m3u8") {
        Format::Hls
    } else if matches!(path.rsplit('.').next(), Some("mp4" | "mkv" | "webm")) {
        Format::Mp4
    } else {
        // Default: most direct CDN URLs are MP4/MKV.
        Format::Mp4
    }
}

/// The `DirectStream` extractor.
#[derive(Debug, Default)]
pub struct DirectStream;

impl DirectStream {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for DirectStream {
    fn id(&self) -> &'static str {
        "directstream"
    }

    fn label(&self) -> &'static str {
        LABEL
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        // Direct CDN hosts AND Google's Range-unsupported hosts.
        is_direct_cdn_host(url) || is_no_range_host(url)
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Upstream's two branches differed only in the wrap: the Google
        // hosts went through /range-proxy (Range translation), everything
        // else shipped directly. Without a server both ship the direct
        // URL; source request headers map to the context's embed Referer.
        let mut stream = Stream::new(url.clone(), infer_format(url))
            .with_label(LABEL)
            .with_ttl(TTL);
        if let Some(referer) = ctx.referer {
            stream = stream.with_referer(referer.to_string());
        }
        Ok(vec![stream])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    #[test]
    fn claims_direct_cdn_and_no_range_hosts() {
        let extractor = DirectStream::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |url: &str| {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };

        // Exact CDN hosts.
        assert!(supports("https://streamx.me/file.mp4"));
        assert!(supports("https://cdn.dramalinkbd.tv/movie.mkv"));
        assert!(supports("https://cdn.kryntal.top/hls/x.m3u8"));
        // Wildcard suffixes.
        assert!(supports("https://abc123.r2.dev/file.mp4"));
        assert!(supports("https://bucket.r2.cloudflarestorage.com/file.mp4"));
        assert!(supports("https://whatever.hakunaymatata.com/file.mp4"));
        // Google's Range-unsupported hosts, exact and subdomain.
        assert!(supports("https://video-downloads.googleusercontent.com/a"));
        assert!(supports("https://lh3.googleusercontent.com/a"));
        assert!(supports(
            "https://sub.video-downloads.googleusercontent.com/a"
        ));
        // Not claimed.
        assert!(!supports("https://example.com/file.mp4"));
        assert!(!supports("https://google.com/x"));
    }

    #[tokio::test]
    async fn infers_the_format_from_the_path() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let extractor = DirectStream::new();

        let cases = [
            ("https://abc123.r2.dev/hls/master.m3u8", Format::Hls),
            ("https://edge.vidhawk.buzz/hls/MASTER.M3U8", Format::Hls),
            ("https://pixeldrain.dev/file.mp4", Format::Mp4),
            ("https://cdn.dramalinkbd.tv/movie.mkv", Format::Mp4),
            ("https://fastdl.zip/file.webm", Format::Mp4),
            // The NowHDTime HLS API has no .m3u8 in its path — upstream's
            // default makes it MP4.
            ("https://nhdapi.com/api/hls?t=abc", Format::Mp4),
        ];
        for (url, format) in cases {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
            assert_direct_stream(&streams, format, url.as_str());
            assert_eq!(streams[0].label.as_deref(), Some(LABEL));
            assert_eq!(streams[0].ttl, TTL);
        }
    }

    #[tokio::test]
    async fn forwards_the_context_referer_as_a_request_header() {
        let fetcher = ScriptedFetcher::default();
        let referer = Url::parse("https://embed.example.com/watch/1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        let ctx = ResolveCtx {
            fetcher: &fetcher,
            media: None,
            source_id: None,
            referer: Some(&referer),
        };
        let url = Url::parse("https://cdn.kryntal.top/hls/x.m3u8")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = DirectStream::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://embed.example.com/watch/1")
        );
    }

    #[tokio::test]
    async fn ships_without_headers_when_the_source_set_none() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://video-downloads.googleusercontent.com/a.mp4")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = DirectStream::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert!(streams[0].meta.request_headers.is_empty());
        assert!(
            fetcher.requests().is_empty(),
            "the passthrough must not fetch"
        );
    }
}
