//! `AnimeKai`: HLS from the shared `aniwatchtv.uk` backend.
//!
//! Ports `src/extractor/AnimeKai.js` — the same backend, Referer, and
//! behavior as `HiAnime` (upstream keeps two identical extractors so
//! both source ids get claimed). The playlists only play with
//! `Referer: https://zokoanime.video/`; this library has no server, so
//! the playlist ships directly with that Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! instead of upstream's `/proxy` wrap. See
//! `zoko_hls_stream` for the
//! shared hop.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

use crate::hosts::hianime::zoko_hls_stream;

/// Upstream result lifetime: 30min.
const TTL: Duration = Duration::from_mins(30);

/// The `AnimeKai` extractor.
#[derive(Debug, Default)]
pub struct AnimeKai;

impl AnimeKai {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for AnimeKai {
    fn id(&self) -> &'static str {
        "animekai"
    }

    fn label(&self) -> &'static str {
        "AnimeKai"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
        // Only URLs from the AnimeKai source.
        ctx.source_id == Some(self.id())
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        Ok(vec![zoko_hls_stream(url, TTL, self.label())])
    }
}

#[cfg(test)]
mod tests {
    use crate::hosts::hianime::ZOKO_REFERER;
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;
    use vsources_core::types::Format;

    fn url() -> Url {
        Url::parse("https://hls2.aniwatchtv.uk/v/def456/master.m3u8?token=xyz")
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
    fn claims_only_animekai_source_urls() {
        let extractor = AnimeKai::new();
        let fetcher = ScriptedFetcher::default();
        let url = url();

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("animekai")), &url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("hianime")), &url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &url));
    }

    #[tokio::test]
    async fn passes_the_playlist_through_with_the_zoko_referer() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeKai::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://hls2.aniwatchtv.uk/v/def456/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("AnimeKai"));
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(ZOKO_REFERER)
        );
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for AnimeKai either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeKai::new()
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
