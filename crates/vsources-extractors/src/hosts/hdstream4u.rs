//! `HDStream4U`: HLS from JW embeds, MP4 from the download page.
//!
//! Ports `src/extractor/HDStream4U.js`. The `/embed/` page's JW player
//! `file:` value (or any master playlist URL) resolves the HLS stream;
//! when the embed yields nothing, the `/download/` page is scraped for
//! a direct MP4/MKV/AVI link. Both results hotlink on the site's own
//! Referer. A fetch failure on either page is a miss (upstream catches
//! and returns []), not an error.
//!
//! No upstream cuts — `meta` passthrough needs nothing extra.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::{fetch_page_with, first_capture};

/// The canonical host this extractor serves (upstream matches by exact
/// `host.includes`).
const CANONICAL_HOST: &str = "hdstream4u.com";
/// The hotlink Referer for both result shapes.
const SITE_REFERER: &str = "https://hdstream4u.com/";

static JW_FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"file\s*:\s*["']([^"']*\.m3u8[^"']*)["']"#)
        .unwrap_or_else(|e| panic!("valid JW file pattern: {e}"))
});
static MASTER_M3U8: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(https?://[^\s"'<>]+master\.m3u8[^\s"'<>]*)"#)
        .unwrap_or_else(|e| panic!("valid master pattern: {e}"))
});
static ANY_M3U8: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(https?://[^\s"'<>]+\.m3u8[^\s"'<>]*)"#)
        .unwrap_or_else(|e| panic!("valid m3u8 pattern: {e}"))
});
static DIRECT_FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^\s"'<>]+\.(?:mp4|mkv|avi)[^\s"'<>]*)"#)
        .unwrap_or_else(|e| panic!("valid direct file pattern: {e}"))
});
static DOWNLOAD_LINK: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r#"(?i)href\s*=\s*["'](https?://[^"']+)["'][^>]*>\s*(?:Download|Direct|Click)"#,
    )
    .unwrap_or_else(|e| panic!("valid download link pattern: {e}"))
});

/// Upstream result lifetime: 5min.
const TTL: Duration = Duration::from_secs(300);

/// The `HDStream4U` extractor.
#[derive(Debug, Default)]
pub struct HDStream4U;

impl HDStream4U {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Ports `extractM3u8Url`: the three patterns in order, each only
    /// kept when the capture parses as a URL.
    fn extract_m3u8_url(&self, html: &str) -> Result<Option<Url>, ExtractorError> {
        for pattern in [&JW_FILE, &MASTER_M3U8, &ANY_M3U8] {
            if let Some(candidate) = first_capture(pattern, html)
                .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
                && let Ok(url) = Url::parse(&candidate)
            {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    /// Ports `extractDirectUrl`: the media-file URL, then the labeled
    /// download link.
    fn extract_direct_url(&self, html: &str) -> Result<Option<Url>, ExtractorError> {
        if let Some(candidate) = first_capture(&DIRECT_FILE, html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            && let Ok(url) = Url::parse(&candidate)
        {
            return Ok(Some(url));
        }
        if let Some(candidate) = first_capture(&DOWNLOAD_LINK, html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            && let Ok(url) = Url::parse(&candidate)
        {
            return Ok(Some(url));
        }
        Ok(None)
    }
}

#[async_trait]
impl Extractor for HDStream4U {
    fn id(&self) -> &'static str {
        "hdstream4u"
    }

    fn label(&self) -> &'static str {
        "HDStream4U"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        // Upstream matches with `url.host.includes('hdstream4u.com')`.
        url.host_str()
            .is_some_and(|host| host.contains(CANONICAL_HOST))
    }

    fn normalize(&self, url: &Url) -> Url {
        // `new URL(`https://hdstream4u.com/embed/${code}`)` — the last
        // path segment becomes the embed code.
        let code = url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        Url::parse(&format!("https://{CANONICAL_HOST}/embed/{code}")).unwrap_or_else(|e| {
            panic!("hdstream4u normalization must produce a valid URL: {e}");
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);

        // Fetch failures are misses upstream (the try/catch returns []).
        let Ok(html) = fetch_page_with(ctx, url, referer).await else {
            return Err(ExtractorError::NotFound);
        };

        if let Some(m3u8) = self.extract_m3u8_url(&html)? {
            return Ok(vec![
                Stream::new(m3u8, Format::Hls)
                    .with_ttl(TTL)
                    .with_referer(SITE_REFERER),
            ]);
        }

        // Fallback: the download page for a direct MP4 link.
        let code = url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let download_url = Url::parse(&format!("https://{CANONICAL_HOST}/download/{code}"))
            .map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid download URL: {e}"))
            })?;
        if let Ok(download_html) = fetch_page_with(ctx, &download_url, referer).await
            && let Some(mp4) = self.extract_direct_url(&download_html)?
        {
            return Ok(vec![
                Stream::new(mp4, Format::Mp4)
                    .with_ttl(TTL)
                    .with_referer(SITE_REFERER),
            ]);
        }

        Err(ExtractorError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://hdstream4u.com/embed/abc123")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_hdstream4u_host() {
        let extractor = HDStream4U::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/embed/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("hdstream4u.com"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_file_urls_to_the_embed_form() {
        let extractor = HDStream4U::new();
        let forms = [
            (
                "https://hdstream4u.com/file/abc123",
                "https://hdstream4u.com/embed/abc123",
            ),
            (
                "https://mirror.example/embed/abc123",
                "https://hdstream4u.com/embed/abc123",
            ),
        ];
        for (input, expected) in forms {
            let url = Url::parse(input).unwrap_or_else(|e| panic!("valid URL: {e}"));
            assert_eq!(extractor.normalize(&url).as_str(), expected);
        }
    }

    #[tokio::test]
    async fn resolves_the_jw_player_playlist() {
        let fetcher = ScriptedFetcher::default().page(
            "/embed/abc123",
            r#"<script>jwplayer("v").setup({file:"https://cdn.hdstream4u.example/hls/master.m3u8?tok=1"});</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = HDStream4U::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the JW embed must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.hdstream4u.example/hls/master.m3u8",
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://hdstream4u.com/")
        );
    }

    #[tokio::test]
    async fn resolves_a_bare_master_playlist_url() {
        let fetcher = ScriptedFetcher::default().page(
            "/embed/abc123",
            r#"<script>var src = "https://cdn.hdstream4u.example/stream/master.m3u8";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = HDStream4U::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the master URL must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.hdstream4u.example/stream/master.m3u8",
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_download_page() {
        let fetcher = ScriptedFetcher::default()
            .page("/embed/abc123", "<p>no player here</p>")
            .page(
                "/download/abc123",
                r#"<a href="https://cdn.hdstream4u.example/files/video.mp4">Download</a>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HDStream4U::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the download fallback must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://cdn.hdstream4u.example/files/video.mp4",
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://hdstream4u.com/")
        );

        // The download page fetch carried the embed page as its Referer.
        assert_eq!(
            fetcher
                .sent_header("/download/abc123", "Referer")
                .as_deref(),
            Some("https://hdstream4u.com/embed/abc123")
        );
    }

    #[tokio::test]
    async fn empty_embed_fetches_are_misses() {
        // No page scripted at all: the fetch itself fails.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        match HDStream4U::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a failed embed fetch must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unresolvable_pages_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/embed/abc123", "<p>nothing here</p>")
            .page("/download/abc123", "<p>nothing here either</p>");
        let ctx = ctx_for(&fetcher, None);

        match HDStream4U::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty page must be a NotFound, got {other:?}"),
        }
    }
}
