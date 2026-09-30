//! `Netlio`: passthrough for the rotating Netlio CDN HLS playlists.
//!
//! Ports `src/extractor/Netlio.js`. The Netlio source resolves to direct
//! HLS master playlists on dozens of rotating CDN hosts — too many to
//! list — so URLs are claimed by path marker instead: `cf-master`,
//! `/v4/`, or `/hls3/`. Upstream additionally required the markers for
//! `*.workers.dev` hosts (Cloudflare's shared worker domain, where a
//! bare host match would hijack unrelated embed APIs) — a guard that
//! collapses to the same path-marker predicate for every host.
//!
//! Cut for the library port: upstream probed the CDN from the addon's
//! own egress to choose between a `/proxy` wrap (server-side fetch with
//! Cloudflare bypass and playlist rewriting) and a direct stream with
//! request headers. There is no server here, so both paths ship the
//! direct playlist with `Referer: https://netlio.vercel.app/` and a
//! Chrome User-Agent in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! — exactly the headers upstream attached on its direct path — and the
//! egress probe (plus the `nuvioDirectWithHeaders` meta flag it set for
//! the addon's router) is gone.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 1h.
const TTL: Duration = Duration::from_hours(1);
/// The Referer upstream sent for Netlio CDN playlists.
const REFERER: &str = "https://netlio.vercel.app/";
/// The browser User-Agent upstream shipped on its direct path.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Whether the URL carries a Netlio path marker: `cf-master`, `/v4/`,
/// or `/hls3/` (case-insensitive).
///
/// Shared with `animedirect`, which claims the same URLs for its
/// Netlio-marker branch.
#[must_use]
pub(crate) fn has_netlio_path_marker(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains("cf-master") || path.contains("/v4/") || path.contains("/hls3/")
}

/// The `Netlio` extractor.
#[derive(Debug, Default)]
pub struct Netlio;

impl Netlio {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Netlio {
    fn id(&self) -> &'static str {
        "netlio"
    }

    fn label(&self) -> &'static str {
        "Netlio"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        has_netlio_path_marker(url)
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let mut stream = Stream::new(url.clone(), Format::Hls)
            .with_label(self.label())
            .with_ttl(TTL)
            .with_referer(REFERER);
        stream.meta = stream.meta.with_header("User-Agent", UA);
        Ok(vec![stream])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    #[test]
    fn claims_urls_by_path_marker() {
        let extractor = Netlio::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |url: &str| {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };

        // The Netlio path markers, on any (rotating) CDN host.
        assert!(supports(
            "https://hightechsecurity.shop/cf-master/abc/playlist.m3u8"
        ));
        assert!(supports("https://onlineartacademy.site/v4/xyz/master.m3u8"));
        assert!(supports("https://x.mortgagerefinance.cfd/hls3/movie.m3u8"));
        // Case-insensitive, as upstream lower-cases the path.
        assert!(supports("https://x.example.com/CF-MASTER/abc.m3u8"));
        // workers.dev hosts must also carry a marker (shared Cloudflare
        // worker domain — unrelated embed APIs live there).
        assert!(supports(
            "https://netlio-cdn.example.workers.dev/v4/abc.m3u8"
        ));
        assert!(!supports(
            "https://api.anicine-embed.workers.dev/player.html"
        ));
        // Plain paths on other hosts are not Netlio's.
        assert!(!supports("https://example.com/files/video.mp4"));
        assert!(!supports("https://example.com/v5/abc.m3u8"));
    }

    #[tokio::test]
    async fn passes_the_playlist_through_with_netlio_headers() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hightechsecurity.shop/cf-master/abc/master.m3u8")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = Netlio::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://hightechsecurity.shop/cf-master/abc/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("Netlio"));
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(REFERER)
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(UA)
        );
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream's only fetch was the (cut) egress probe.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://1hyahuwewhyvwmq.mortgagerefinance.cfd/hls3/squid-game.m3u8")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = Netlio::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert!(
            fetcher.requests().is_empty(),
            "the passthrough must not fetch"
        );
    }
}
