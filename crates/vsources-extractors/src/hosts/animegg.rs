//! `AnimeGG`: direct MP4 from `animegg.org`, hotlinked on the site Referer.
//!
//! Ports `src/extractor/AnimeGG.js`. Claims `animegg.org` URLs from the
//! `animegg` source and passes them through as MP4. Upstream routed them
//! through a server-side `/proxy` that sent `Referer:
//! https://www.animegg.org/`; this library has no server, so the playable
//! URL ships directly with that Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//! for the player to send.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 30min.
const TTL: Duration = Duration::from_mins(30);
/// The Referer upstream's proxy hop sent for animegg.org media.
const REFERER: &str = "https://www.animegg.org/";

/// The `AnimeGG` extractor.
#[derive(Debug, Default)]
pub struct AnimeGG;

impl AnimeGG {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for AnimeGG {
    fn id(&self) -> &'static str {
        "animegg"
    }

    fn label(&self) -> &'static str {
        "AnimeGG"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        // Only animegg.org URLs from the AnimeGG source.
        ctx.source_id == Some(self.id())
            && url
                .host_str()
                .is_some_and(|host| host.contains("animegg.org"))
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        Ok(vec![
            Stream::new(url.clone(), Format::Mp4)
                .with_ttl(TTL)
                .with_referer(REFERER),
        ])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://www.animegg.org/play/abc123/video.mp4?for=xyz")
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
    fn claims_only_animegg_source_urls_on_the_animegg_host() {
        let extractor = AnimeGG::new();
        let fetcher = ScriptedFetcher::default();
        let animegg_url = url();
        let other_url = Url::parse("https://cdn.example.com/file.mp4")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("animegg")), &animegg_url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("animegg")), &other_url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("other")), &animegg_url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &animegg_url));
    }

    #[tokio::test]
    async fn passes_the_mp4_through_with_the_site_referer() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeGG::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://www.animegg.org/play/abc123/video.mp4",
        );
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(REFERER)
        );
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for AnimeGG either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AnimeGG::new()
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
