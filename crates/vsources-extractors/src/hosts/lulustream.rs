//! `LuluStream`: direct HLS from packed `/d/` download pages.
//!
//! Ports `src/extractor/LuluStream.js`. The `/e/` embed normalizes to
//! `/e/<id>`; extraction swaps that for the `/d/` download page, whose
//! packed player hides the playlist in `sources:[{file:…}]` (or a
//! plain `file:"…m3u8"`); a direct m3u8/mp4 URL in the raw page is the
//! fallback. Both results hotlink on the embed origin's Referer.
//!
//! Cut from the upstream port: the cheerio `h1`/`title` selections
//! (feed `meta.title`, which
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no field for).

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};
use vsources_core::unpack::extract_url_from_packed;

use crate::helpers::{direct_stream, fetch_page_with, host_matcher};

host_matcher!(HOSTS, r"lulu");

/// Same-player mirrors served from entirely different host names
/// (verified upstream).
const SAME_PLAYER_HOSTS: &[&str] = &["streamhihi.com", "cdn1.site", "d00ds.site"];

static PACKED_SOURCES: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"sources:\[\{file:"(.*?)""#)
        .unwrap_or_else(|e| panic!("valid sources pattern: {e}"))
});
static PACKED_FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"file:"(https?://[^"]*\.m3u8[^"]*)""#)
        .unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
static UNAVAILABLE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(No such file|File Not Found)")
        .unwrap_or_else(|e| panic!("valid unavailable pattern: {e}"))
});
static DIRECT_MEDIA: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^'"]*\.(?:m3u8|mp4)[^'"]*)"#)
        .unwrap_or_else(|e| panic!("valid direct pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `LuluStream` family extractor.
#[derive(Debug, Default)]
pub struct LuluStream;

impl LuluStream {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for LuluStream {
    fn id(&self) -> &'static str {
        "lulustream"
    }

    fn label(&self) -> &'static str {
        "LuluStream"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            HOSTS.is_match(host).unwrap_or(false) || SAME_PLAYER_HOSTS.contains(&host)
        })
    }

    fn normalize(&self, url: &Url) -> Url {
        // `new URL(`/e/${videoId}`, url)` — the last path segment on the
        // embed's own origin.
        let video_id = url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        Url::parse(&format!(
            "{}://{}/e/{}",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            video_id
        ))
        .unwrap_or_else(|e| panic!("lulustream normalization must produce a valid URL: {e}"))
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);

        // `new URL(url.href.replace('/e/', '/d/'))` — first occurrence.
        let file_url = Url::parse(&url.as_str().replacen("/e/", "/d/", 1)).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid download URL: {e}"))
        })?;
        let html = fetch_page_with(ctx, &file_url, referer).await?;

        // Upstream returns [] here — a miss.
        if UNAVAILABLE.is_match(&html).unwrap_or(false) {
            return Err(ExtractorError::NotFound);
        }

        // The packed player first; a failed unpack falls through to the
        // direct URL like the upstream try/catch.
        if let Some(playlist) =
            extract_url_from_packed(&html, &[PACKED_SOURCES.clone(), PACKED_FILE.clone()])
        {
            return Ok(vec![direct_stream(playlist, Format::Hls, TTL, url)]);
        }

        // Fallback: a direct m3u8/mp4 URL in the raw page.
        if let Ok(Some(direct)) = crate::helpers::first_capture(&DIRECT_MEDIA, &html)
            && let Ok(direct) = Url::parse(&direct)
        {
            // `directMatch[1].includes('.m3u8') ? hls : mp4`.
            let format = if direct.as_str().contains(".m3u8") {
                Format::Hls
            } else {
                Format::Mp4
            };
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
        Url::parse("https://lulustream.com/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A packed download page in the upstream shape.
    const PACKED_PAGE: &str = r#"<html><body><h1>Some Movie</h1><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('sources:[{file:"1"}]',10,3,'1|https://cdn.lulustream.example/hls/master.m3u8|'.split('|'),0,{}))</script></body></html>"#;

    #[test]
    fn matches_the_lulustream_family() {
        let extractor = LuluStream::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("lulustream.com"));
        assert!(supports("streamhihi.com"));
        assert!(supports("cdn1.site"));
        assert!(supports("d00ds.site"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_to_the_embed_form() {
        let extractor = LuluStream::new();
        let forms = [
            (
                "https://lulustream.com/f/abc123",
                "https://lulustream.com/e/abc123",
            ),
            (
                "https://lulustream.com/e/abc123",
                "https://lulustream.com/e/abc123",
            ),
            (
                "https://streamhihi.com/v/abc123/",
                "https://streamhihi.com/e/abc123",
            ),
        ];
        for (input, expected) in forms {
            let url = Url::parse(input).unwrap_or_else(|e| panic!("valid URL: {e}"));
            assert_eq!(extractor.normalize(&url).as_str(), expected);
        }
    }

    #[tokio::test]
    async fn fetches_the_download_page_and_unpacks_the_playlist() {
        let fetcher = ScriptedFetcher::default().page("/d/abc123", PACKED_PAGE);
        let ctx = ctx_for(&fetcher, None);

        let streams = LuluStream::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the packed page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.lulustream.example/hls/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://lulustream.com/")
        );

        // The download page fetch carried the embed page as its Referer.
        assert_eq!(
            fetcher.sent_header("/d/abc123", "Referer").as_deref(),
            Some("https://lulustream.com/e/abc123")
        );
    }

    #[tokio::test]
    async fn falls_back_to_a_direct_url() {
        let fetcher = ScriptedFetcher::default().page(
            "/d/abc123",
            r#"<script>var x = "https://cdn.lulustream.example/direct/file.mp4";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = LuluStream::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the direct fallback must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://cdn.lulustream.example/direct/file.mp4",
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://lulustream.com/")
        );
    }

    #[tokio::test]
    async fn missing_files_are_misses() {
        for marker in ["No such file", "File Not Found"] {
            let fetcher = ScriptedFetcher::default().page("/d/abc123", format!("<p>{marker}</p>"));
            let ctx = ctx_for(&fetcher, None);

            match LuluStream::new().extract(&ctx, &url()).await {
                Err(ExtractorError::NotFound) => {}
                other => panic!("a {marker} page must be a NotFound, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unresolvable_pages_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/d/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match LuluStream::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty page must be a NotFound, got {other:?}"),
        }
    }
}
