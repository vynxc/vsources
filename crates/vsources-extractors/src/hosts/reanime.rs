//! `ReAnime`: passthrough for already-direct playable HLS.
//!
//! Ports `src/extractor/ReAnime.js`. Claims URLs from the `reanime`
//! source: upstream's `/reanime-proxy` endpoint (part of the source
//! pipeline, not the extractor) has already XOR-decrypted the playlist
//! and rewritten its segments by the time the extractor sees the URL,
//! so the extractor only re-labels it as HLS. The port returns the
//! direct stream unchanged — no fetch, no headers.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 5min.
const TTL: Duration = Duration::from_mins(5);

/// The `ReAnime` extractor.
#[derive(Debug, Default)]
pub struct ReAnime;

impl ReAnime {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for ReAnime {
    fn id(&self) -> &'static str {
        "reanime"
    }

    fn label(&self) -> &'static str {
        "ReAnime"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
        // Only URLs from the ReAnime source.
        ctx.source_id == Some(self.id())
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Already-direct HLS — the URL ships as-is.
        Ok(vec![
            Stream::new(url.clone(), Format::Hls)
                .with_label(self.label())
                .with_ttl(TTL),
        ])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://reanime.example/proxy/hls/abc123/master.m3u8")
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
    fn claims_only_reanime_source_urls() {
        let extractor = ReAnime::new();
        let fetcher = ScriptedFetcher::default();
        let url = url();

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("reanime")), &url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("other")), &url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &url));
    }

    #[tokio::test]
    async fn returns_the_direct_stream_unchanged() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = ReAnime::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://reanime.example/proxy/hls/abc123/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("ReAnime"));
        assert_eq!(stream.ttl, TTL);
        // Already playable: no request headers attached.
        assert!(stream.meta.request_headers.is_empty());
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for ReAnime either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = ReAnime::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert!(
            fetcher.requests().is_empty(),
            "the passthrough must not fetch"
        );
    }
}
