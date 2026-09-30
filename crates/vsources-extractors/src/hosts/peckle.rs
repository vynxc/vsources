//! `2Peckle`: passthrough for `shegu.net` direct MKV/HLS URLs.
//!
//! Ports `src/extractor/Peckle.js`. Claims URLs from the `peckle`
//! source: ORG links are direct `.mkv`/`.mp4` files on
//! `usa*-as05.shegu.net`, everything else is a transcoded
//! `hls.shegu.net` playlist. Both play without Referer or auth
//! (verified upstream with HTTP 200/206), so no request headers are
//! attached and the URL ships exactly as the source resolved it.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 10min — stream URLs carry time-limited
/// signatures.
const TTL: Duration = Duration::from_mins(10);

/// The `2Peckle` extractor.
#[derive(Debug, Default)]
pub struct Peckle;

impl Peckle {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Peckle {
    fn id(&self) -> &'static str {
        "peckle"
    }

    fn label(&self) -> &'static str {
        "2Peckle"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
        // Only URLs from the 2Peckle source.
        ctx.source_id == Some(self.id())
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // shegu.net URLs play directly — no Referer/Auth needed.
        let is_mkv = url.path().contains(".mkv") || url.path().contains(".mp4");
        let format = if is_mkv { Format::Mp4 } else { Format::Hls };
        Ok(vec![Stream::new(url.clone(), format).with_ttl(TTL)])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

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
    fn claims_only_peckle_source_urls() {
        let extractor = Peckle::new();
        let fetcher = ScriptedFetcher::default();
        let url = Url::parse("https://usa7-as05.shegu.net/vip/movie.mkv?KEY1=abc")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("peckle")), &url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("other")), &url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &url));
    }

    #[tokio::test]
    async fn infers_the_format_from_the_path() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let extractor = Peckle::new();

        // Upstream checks the path for `.mkv`/`.mp4`; everything else is
        // the transcoded HLS shape.
        let cases = [
            (
                "https://usa7-as05.shegu.net/vip/movie.mkv?KEY1=abc",
                Format::Mp4,
            ),
            (
                "https://usa7-as05.shegu.net/vip/movie.mp4?KEY1=abc",
                Format::Mp4,
            ),
            (
                "https://hls.shegu.net/abc123.m3u8?sign=xyz&t=123",
                Format::Hls,
            ),
            ("https://hls.shegu.net/playlist?id=abc", Format::Hls),
        ];
        for (url, format) in cases {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            let streams = extractor
                .extract(&ctx, &url)
                .await
                .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
            assert_direct_stream(&streams, format, url.as_str());
            assert_eq!(streams[0].ttl, TTL);
            assert!(
                streams[0].meta.request_headers.is_empty(),
                "shegu.net plays without headers"
            );
        }
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for 2Peckle either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://hls.shegu.net/abc123.m3u8?sign=xyz&t=123")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = Peckle::new()
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
