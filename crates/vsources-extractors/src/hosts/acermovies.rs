//! `AcerMovies`: passthrough for Google's `video-downloads` CDN.
//!
//! Ports `src/extractor/AcerMovies.js`. The `AcerMovies` source resolves
//! to direct `video-downloads.googleusercontent.com` URLs (Google Drive
//! CDN), and the extractor claims exactly that host — the other
//! `googleusercontent` subdomains are left to their own extractors.
//!
//! Cut for the library port: upstream wrapped these URLs in a
//! `/range-proxy` because the CDN ignores `Range` requests (HTTP 200
//! with the full file, which breaks seeking). There is no server here,
//! so the direct URL ships as-is with no request headers — Range
//! translation becomes the player's concern.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 5min — `GDrive` URLs carry time-limited
/// tokens.
const TTL: Duration = Duration::from_mins(5);

/// The `AcerMovies` extractor.
#[derive(Debug, Default)]
pub struct AcerMovies;

impl AcerMovies {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for AcerMovies {
    fn id(&self) -> &'static str {
        "acermovies"
    }

    fn label(&self) -> &'static str {
        "AcerMovies"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        // Only video-downloads.googleusercontent.com (AcerMovies' direct
        // GDrive CDN); other googleusercontent subdomains belong to their
        // own extractors.
        url.host_str() == Some("video-downloads.googleusercontent.com")
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        Ok(vec![
            Stream::new(url.clone(), Format::Mp4)
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
        Url::parse("https://video-downloads.googleusercontent.com/abc123/video.mp4")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn claims_only_the_video_downloads_subdomain() {
        let extractor = AcerMovies::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |url: &str| {
            let url = Url::parse(url).unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };

        assert!(supports(
            "https://video-downloads.googleusercontent.com/abc/video.mp4"
        ));
        assert!(!supports("https://lh3.googleusercontent.com/abc"));
        assert!(!supports(
            "https://drive.usercontent.googleusercontent.com/abc"
        ));
        assert!(!supports("https://example.com/video.mp4"));
    }

    #[tokio::test]
    async fn passes_the_gdrive_url_through() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AcerMovies::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://video-downloads.googleusercontent.com/abc123/video.mp4",
        );
        let stream = &streams[0];
        assert_eq!(stream.label.as_deref(), Some("AcerMovies"));
        assert_eq!(stream.ttl, TTL);
        // Google URLs need no headers — upstream's range-proxy hop is cut.
        assert!(stream.meta.request_headers.is_empty());
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for AcerMovies either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = AcerMovies::new()
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
