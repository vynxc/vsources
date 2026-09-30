//! `FileMoon`: direct HLS from packed-JW embed pages.
//!
//! Ports `src/extractor/FileMoon.js`. The embed page ships a
//! Dean-Edwards-packed player bundle; the playlist URL hides inside the
//! unpacked `sources:[{file:…}]` initializer, with a plain direct URL as
//! the fallback.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};
use vsources_core::unpack::extract_url_from_packed;

use crate::helpers::{direct_stream, fetch_page, format_for_url, host_matcher};

host_matcher!(HOSTS, r"filemoon");

/// `vimeos.net` & friends serve the same packed-JW pages as filemoon
/// (verified upstream 2025-09).
const SAME_PLAYER_HOSTS: &[&str] = &["furher.in", "moonmov.pro", "cinegrab.com", "vimeos.net"];

static PACKED_SOURCES: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"sources:\[\{file:"(.*?)""#)
        .unwrap_or_else(|e| panic!("valid sources pattern: {e}"))
});
static PACKED_FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"file:"(https?://[^"]*\.m3u8[^"]*)""#)
        .unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
static DIRECT_MEDIA: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^'"]*\.(?:m3u8|mp4)[^'"]*)"#)
        .unwrap_or_else(|e| panic!("valid direct pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `FileMoon` family extractor.
#[derive(Debug, Default)]
pub struct FileMoon;

impl FileMoon {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for FileMoon {
    fn id(&self) -> &'static str {
        "filemoon"
    }

    fn label(&self) -> &'static str {
        "FileMoon"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            HOSTS.is_match(host).unwrap_or(false) || SAME_PLAYER_HOSTS.contains(&host)
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let html = fetch_page(ctx, url).await?;

        // Upstream lets extractUrlFromPacked throw and falls through; the
        // Option-based port makes the same fallthrough explicit.
        if let Some(playlist) =
            extract_url_from_packed(&html, &[PACKED_SOURCES.clone(), PACKED_FILE.clone()])
        {
            return Ok(vec![direct_stream(playlist, Format::Hls, TTL, url)]);
        }

        // Fallback: a direct m3u8/mp4 URL in the raw page.
        if let Ok(Some(direct)) = crate::helpers::first_capture(&DIRECT_MEDIA, &html)
            && let Ok(direct) = Url::parse(&direct)
        {
            let format = format_for_url(&direct);
            return Ok(vec![direct_stream(direct, format, TTL, url)]);
        }

        Err(ExtractorError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://filemoon.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A minimal packed player page in the upstream shape: the
    /// `sources:[{file:…}]` initializer survives unpacking.
    const PACKED_PAGE: &str = r#"<html><body><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('sources:[{file:"1"}]',10,3,'1|https://cdn.example.com/hls/master.m3u8|'.split('|'),0,{}))</script></body></html>"#;

    #[test]
    fn matches_the_filemoon_family() {
        let extractor = FileMoon::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("filemoon.to"));
        assert!(supports("vimeos.net"));
        assert!(supports("furher.in"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn unpacks_the_playlist_url() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", PACKED_PAGE);
        let ctx = ctx_for(&fetcher, None);

        let streams = FileMoon::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the packed page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.example.com/hls/master.m3u8",
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://filemoon.to/")
        );
    }

    #[tokio::test]
    async fn falls_back_to_a_direct_url() {
        let fetcher = ScriptedFetcher::default().page(
            "/e/abc123",
            r#"<script>var x = "https://cdn.example.com/direct/file.mp4";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = FileMoon::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the direct fallback must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://cdn.example.com/direct/file.mp4",
        );
    }

    #[tokio::test]
    async fn unresolvable_pages_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match FileMoon::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty page must be a NotFound, got {other:?}"),
        }
    }
}
