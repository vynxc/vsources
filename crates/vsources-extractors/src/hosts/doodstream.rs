//! `DoodStream`: direct MP4 from `dood*` embed pages.
//!
//! Ports `src/extractor/DoodStream.js`. Works without a proxy: the embed
//! page carries a `/pass_md5/` link whose response builds the direct file
//! URL together with a random component and the page token.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Format;
use vsources_core::types::Stream;

use crate::helpers::{direct_stream, fetch_page_with, first_capture, host_matcher, random_token};

host_matcher!(
    HOSTS,
    r"dood|do[0-9]go|doood|dooood|ds2play|ds2video|dsvplay|d0o0d|do0od|d0000d|d000d|myvidplay|vidply|all3do|doply|vide0|vvide0|d-s|playmogo"
);

static PASS_MD5: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(\/pass_md5\/[^"'\s]+)"#)
        .unwrap_or_else(|e| panic!("valid pass_md5 pattern: {e}"))
});
static TOKEN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"'([^']{10,})'\.substr")
        .unwrap_or_else(|e| panic!("valid token pattern: {e}"))
});
static DIRECT_MP4: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^'"]*\.mp4[^'"]*)"#)
        .unwrap_or_else(|e| panic!("valid mp4 pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `DoodStream` family extractor.
#[derive(Debug, Default)]
pub struct DoodStream;

impl DoodStream {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for DoodStream {
    fn id(&self) -> &'static str {
        "doodstream"
    }

    fn label(&self) -> &'static str {
        "DoodStream"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    fn normalize(&self, url: &Url) -> Url {
        // `url.pathname.replace(/\/+$/, '').split('/').pop()` — the video
        // id is the last path segment on any mirror host.
        let video_id = url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        Url::parse(&format!("https://dood.to/e/{video_id}")).unwrap_or_else(|e| {
            panic!("dood normalization must produce a valid URL: {e}");
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Both wire requests carry the embed page as their Referer.
        let referer = ctx.referer.unwrap_or(url);
        let html = fetch_page_with(ctx, url, referer).await?;

        if html.contains("Video not found") {
            return Err(ExtractorError::NotFound);
        }

        let pass_path = first_capture(&PASS_MD5, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?;

        if let Some(pass_path) = pass_path {
            let pass_url = url
                .join(&pass_path)
                .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?;
            let pass_response = fetch_page_with(ctx, &pass_url, referer).await?;
            let random = random_token(10);
            let token = first_capture(&TOKEN, &html)
                .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
                .unwrap_or_default();

            // `new URL(`${pass}${random}.mp4?token=${token}`, url.origin)`:
            // the pass response is a path fragment resolved against the
            // embed's origin.
            let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
            let direct = format!(
                "{origin}/{}{random}.mp4?token={token}",
                pass_response.trim_start_matches('/')
            );
            if let Ok(direct) = Url::parse(&direct) {
                return Ok(vec![direct_stream(direct, Format::Mp4, TTL, url)]);
            }
        }

        // Fallback: a direct mp4 URL in the page.
        if let Ok(Some(direct)) = first_capture(&DIRECT_MP4, &html)
            && let Ok(direct) = Url::parse(&direct)
        {
            return Ok(vec![direct_stream(direct, Format::Mp4, TTL, url)]);
        }

        Err(ExtractorError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://dood.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_dood_family() {
        let extractor = DoodStream::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("dood.to"));
        assert!(supports("d0000d.com"));
        assert!(supports("ds2play.com"));
        assert!(supports("myvidplay.net"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_to_the_canonical_host() {
        let extractor = DoodStream::new();
        let url =
            Url::parse("https://d0000d.com/e/abc123/").unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert_eq!(
            extractor.normalize(&url).as_str(),
            "https://dood.to/e/abc123"
        );
    }

    #[tokio::test]
    async fn builds_the_direct_url_from_pass_md5() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/e/abc123",
                r"<html><script>fetch('/pass_md5/abc/xyz'); var token='abcdefghij'.substr</script></html>",
            )
            .page("/pass_md5/abc/xyz", "8c6c0a1b/d6ac9f2e");
        let ctx = ctx_for(&fetcher, None);

        let streams = DoodStream::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the pass_md5 path must resolve: {e}"));
        assert_direct_stream(&streams, Format::Mp4, "https://dood.to/8c6c0a1b/");
        let stream = &streams[0];
        assert!(stream.url.path().contains(".mp4"));
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://dood.to/")
        );

        // The Referer traveled on both wire requests.
        assert_eq!(
            fetcher
                .sent_header("/pass_md5/abc/xyz", "Referer")
                .as_deref(),
            Some("https://dood.to/e/abc123")
        );
    }

    #[tokio::test]
    async fn falls_back_to_a_direct_mp4_link() {
        let fetcher = ScriptedFetcher::default().page(
            "/e/abc123",
            r#"<video src="https://cdn.example.com/files/video.mp4"></video>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = DoodStream::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the mp4 fallback must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://cdn.example.com/files/video.mp4",
        );
    }

    #[tokio::test]
    async fn video_not_found_is_a_miss() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", "<p>Video not found</p>");
        let ctx = ctx_for(&fetcher, None);

        match DoodStream::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a missing video must be a NotFound, got {other:?}"),
        }
    }
}
