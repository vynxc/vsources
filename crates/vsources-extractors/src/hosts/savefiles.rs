//! `SaveFiles`: direct HLS from `file:"…"` literals.
//!
//! Ports `src/extractor/SaveFiles.js`. The embed (or download) page
//! carries the playlist in a lazy `file:"…"` value plus a `[WxH` size
//! marker; locked or deleted files are misses.
//!
//! Cut from the upstream port: the cheerio `.download-title` selection
//! (feeds `meta.title`, which
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no field for).

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"savefiles|streamhls");

static FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"file:"(.*?)""#).unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
static HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\[\d{3,}x(\d{3,})")
        .unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});
static UNAVAILABLE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)(file was locked|file was deleted)")
        .unwrap_or_else(|e| panic!("valid unavailable pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `SaveFiles` family extractor.
#[derive(Debug, Default)]
pub struct SaveFiles;

impl SaveFiles {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for SaveFiles {
    fn id(&self) -> &'static str {
        "savefiles"
    }

    fn label(&self) -> &'static str {
        "SaveFiles"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    fn normalize(&self, url: &Url) -> Url {
        // `url.href.replace('/e/', '/').replace('/d/', '/')` — JS string
        // replace swaps the first occurrence of each form.
        let normalized = url.as_str().replacen("/e/", "/", 1).replacen("/d/", "/", 1);
        Url::parse(&normalized).unwrap_or_else(|e| {
            panic!("savefiles normalization must produce a valid URL: {e}");
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);
        let html = fetch_page_with(ctx, url, referer).await?;

        if UNAVAILABLE.is_match(&html).unwrap_or(false) {
            return Err(ExtractorError::NotFound);
        }

        // `html.match(/file:"(.*?)"/)[1]` — a missing match is the
        // upstream null dereference: a scrape failure.
        let file = first_capture(&FILE, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or_else(|| ExtractorError::extraction(self.id(), "no file: entry on the page"))?;
        let playlist = Url::parse(&file).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid playlist URL: {e}"))
        })?;

        let height = first_capture(&HEIGHT, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .and_then(|height| height.parse::<u16>().ok());

        let mut stream = Stream::new(playlist, Format::Hls).with_ttl(TTL);
        stream.meta.resolution = height;
        Ok(vec![stream])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://savefiles.com/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_savefiles_family() {
        let extractor = SaveFiles::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("savefiles.com"));
        assert!(supports("streamhls.com"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_the_embed_and_download_forms() {
        let extractor = SaveFiles::new();
        let forms = [
            (
                "https://savefiles.com/e/abc123",
                "https://savefiles.com/abc123",
            ),
            (
                "https://savefiles.com/d/abc123",
                "https://savefiles.com/abc123",
            ),
        ];
        for (input, expected) in forms {
            let url = Url::parse(input).unwrap_or_else(|e| panic!("valid URL: {e}"));
            assert_eq!(extractor.normalize(&url).as_str(), expected);
        }
    }

    #[tokio::test]
    async fn extracts_the_playlist_with_the_size_marker_height() {
        let fetcher = ScriptedFetcher::default().page(
            "/abc123",
            r#"<html><body><h1 class="download-title">Some Movie</h1><div class="videodetails">[1920x1080]</div><script>file:"https://cdn.savefiles.example/hls/master.m3u8"</script></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = SaveFiles::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the file page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.savefiles.example/hls/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.meta.resolution, Some(1080));
        // Upstream ships the result without request headers.
        assert!(stream.meta.request_headers.is_empty());
    }

    #[tokio::test]
    async fn locked_and_deleted_files_are_misses() {
        for marker in ["file was locked", "File Was Deleted"] {
            let fetcher = ScriptedFetcher::default().page("/abc123", format!("<p>{marker}</p>"));
            let ctx = ctx_for(&fetcher, None);

            match SaveFiles::new().extract(&ctx, &url()).await {
                Err(ExtractorError::NotFound) => {}
                other => panic!("a {marker} page must be a NotFound, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn pages_without_a_file_entry_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match SaveFiles::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a file-less page must be a scrape failure, got {other:?}"),
        }
    }
}
