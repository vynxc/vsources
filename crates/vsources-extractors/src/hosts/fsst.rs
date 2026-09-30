//! `Fsst`: direct MP4 from multi-quality `file:` lists.
//!
//! Ports `src/extractor/Fsst.js`. The embed page carries one
//! `file:"…"` value holding a comma-separated list of per-quality
//! entries — the last one wins; each entry is `[<height>p]<url>`, and
//! the URL is followed through one redirect hop to the direct MP4.
//!
//! Cut from the upstream port: the cheerio `title` selection (feeds
//! `meta.title`, which
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no field for)
//! and `noProxyHeaders` (addon-specific proxy routing — there is no
//! server here).

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"fsst");

static FILE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"file:"(.*)""#).unwrap_or_else(|e| panic!("valid file pattern: {e}"))
});
static HEIGHT_AND_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\[?([\d]*)p?]?(.*)")
        .unwrap_or_else(|e| panic!("valid height and URL pattern: {e}"))
});

/// Upstream result lifetime: 3h.
const TTL: Duration = Duration::from_hours(3);

/// The `Fsst` extractor.
#[derive(Debug, Default)]
pub struct Fsst;

impl Fsst {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Fsst {
    fn id(&self) -> &'static str {
        "fsst"
    }

    fn label(&self) -> &'static str {
        "Fsst"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);
        let html = fetch_page_with(ctx, url, referer).await?;

        // `html.match(/file:"(.*)"/)[1].split(',').pop()` — the greedy
        // `file:` value's last comma-separated entry. A missing match is
        // the upstream null dereference: a scrape failure.
        let files = first_capture(&FILE, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or_else(|| ExtractorError::extraction(self.id(), "no file: entry on the page"))?;
        let last_file = files.split(',').next_back().unwrap_or_default();

        // `/\[?([\d]*)p?]?(.*)/` — an optional `[NNNp]` quality prefix,
        // then the file URL.
        let caps = HEIGHT_AND_URL
            .captures(last_file)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or_else(|| ExtractorError::extraction(self.id(), "no URL in the file: entry"))?;
        let height = caps
            .get(1)
            .and_then(|group| group.as_str().parse::<u16>().ok());
        let file_href = caps
            .get(2)
            .map(|group| group.as_str().to_string())
            .unwrap_or_default();

        let file_url = Url::parse(&file_href)
            .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid file URL: {e}")))?;
        let direct = final_redirect_url(ctx, &file_url, referer).await?;

        let mut stream = Stream::new(direct, Format::Mp4).with_ttl(TTL);
        stream.meta.resolution = height;
        Ok(vec![stream])
    }
}

/// Ports `getFinalRedirectUrl(url, { headers }, 1)`: a HEAD request with
/// redirects disabled; a 3xx with a `Location` resolves against `url`,
/// anything else returns `url` unchanged.
async fn final_redirect_url(
    ctx: &ResolveCtx<'_>,
    url: &Url,
    referer: &Url,
) -> Result<Url, ExtractorError> {
    let request = FetchRequest::head(url.clone())
        .with_redirects_disabled()
        .with_header("Referer", referer.to_string());
    let response = ctx.fetcher.request(request).await?;
    if (300..400).contains(&response.status)
        && let Some(location) = response.header("location")
        && let Ok(next) = url.join(location)
    {
        return Ok(next);
    }
    Ok(url.clone())
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://fsst.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_fsst_family() {
        let extractor = Fsst::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("fsst.to"));
        assert!(supports("fsst1.com"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn takes_the_last_quality_entry_and_follows_redirects() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/e/abc123",
                r#"<html><title>Some Movie</title><script>file:"240phttps://cdn.fsst.example/240.mp4,360phttps://cdn.fsst.example/360.mp4"</script></html>"#,
            )
            // The redirect-hop HEAD lands on the file itself.
            .page("/360.mp4", "");
        let ctx = ctx_for(&fetcher, None);

        let streams = Fsst::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the file list must resolve: {e}"));
        assert_direct_stream(&streams, Format::Mp4, "https://cdn.fsst.example/360.mp4");
        let stream = &streams[0];
        assert_eq!(stream.meta.resolution, Some(360));
        // Upstream ships the result without request headers.
        assert!(stream.meta.request_headers.is_empty());

        // The HEAD hop carried the embed page as its Referer.
        assert_eq!(
            fetcher.sent_header("/360.mp4", "Referer").as_deref(),
            Some("https://fsst.to/e/abc123")
        );
    }

    #[tokio::test]
    async fn bracketed_quality_prefixes_parse_too() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/e/abc123",
                r#"<script>file:"[360]https://cdn.fsst.example/file.mp4"</script>"#,
            )
            // The redirect-hop HEAD lands on the file itself.
            .page("/file.mp4", "");
        let ctx = ctx_for(&fetcher, None);

        let streams = Fsst::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the bracketed entry must resolve: {e}"));
        assert_eq!(streams[0].meta.resolution, Some(360));
        assert_direct_stream(&streams, Format::Mp4, "https://cdn.fsst.example/file.mp4");
    }

    #[tokio::test]
    async fn pages_without_a_file_entry_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match Fsst::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a file-less page must be a scrape failure, got {other:?}"),
        }
    }
}
