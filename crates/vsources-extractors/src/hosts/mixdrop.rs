//! `MixDrop`: direct MP4 hotlinks from packed `MDCore.wurl` players.
//!
//! Ports `src/extractor/MixDrop.js`. `MixDrop` embeds
//! (`mixdrop.ag`/`.ch`/`.to`/…, delivery on `*.mxcontent.net`) ship an
//! eval-packed player whose token table decodes to
//! `MDCore.wurl = "//<vserver>.mxcontent.net/v2/<id>.mp4?s=<token>&e=<ts>"`
//! — a direct MP4 whose hotlink token and expiry live in the query
//! string. Mirrors that serve the variable pre-unpacked (unpacked JS)
//! are read directly.
//!
//! The stream ships with **no** Referer on purpose: mxcontent.net is
//! IP-gated (datacenter ranges get 403 regardless of Referer, verified
//! upstream), so the MP4 is treated as a plain direct file —
//! residential IPs pass.
//!
//! Cut from the upstream port: `meta.title`/`meta.provider` (the
//! `'MixDrop'` display fields —
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no title or
//! provider field; the registry attributes the stream to this
//! extractor).

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};
use vsources_core::unpack::extract_url_from_packed;

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"mixdrop");

static RAW_WURL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"MDCore\.wurl\s*=\s*["']([^"']+)["']"#)
        .unwrap_or_else(|e| panic!("valid wurl pattern: {e}"))
});
static PACKED_WURL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"MDCore\.wurl="([^"]+)""#)
        .unwrap_or_else(|e| panic!("valid packed wurl pattern: {e}"))
});
static UNAVAILABLE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)(File (was deleted|Not Found)|video is processing)")
        .unwrap_or_else(|e| panic!("valid unavailable pattern: {e}"))
});

/// Upstream result lifetime: 30min — hotlink tokens rotate faster than
/// filehosts.
const TTL: Duration = Duration::from_mins(30);

/// The `MixDrop` family extractor.
#[derive(Debug, Default)]
pub struct MixDrop;

impl MixDrop {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for MixDrop {
    fn id(&self) -> &'static str {
        "mixdrop"
    }

    fn label(&self) -> &'static str {
        "MixDrop"
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
        // Upstream always sends the embed itself as the Referer.
        let html = fetch_page_with(ctx, url, url).await?;

        if UNAVAILABLE.is_match(&html).unwrap_or(false) {
            return Err(ExtractorError::NotFound);
        }

        // `MDCore.wurl` may appear pre-unpacked OR inside the packed
        // eval — try the raw path first, then the packed one. A missing
        // link is the upstream `new URL`/unpacker throw: a scrape
        // failure.
        let direct = if let Some(raw) = first_capture(&RAW_WURL, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
        {
            // `raw.startsWith('//') ? `https:${raw}` : raw`.
            if let Some(rest) = raw.strip_prefix("//") {
                format!("https://{rest}")
            } else {
                raw
            }
        } else {
            extract_url_from_packed(&html, std::slice::from_ref(&*PACKED_WURL))
                .ok_or_else(|| {
                    ExtractorError::extraction(self.id(), "no MDCore.wurl link on the page")
                })?
                .to_string()
        };
        let direct = Url::parse(&direct)
            .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid wurl: {e}")))?;

        // NO Referer on purpose — see the module docs. The MP4 ships
        // direct; attribution comes from the registry.
        Ok(vec![Stream::new(direct, Format::Mp4).with_ttl(TTL)])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://mixdrop.ag/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A packed player page: the payload tokenizes the whole
    /// `MDCore.wurl` assignment so only the unpacked text matches.
    const PACKED_PAGE: &str = r#"<html><body><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('0="1"',10,2,'MDCore.wurl|//v1.mxcontent.net/v2/abc123.mp4?s=tok&e=1700000000'.split('|'),0,{}))</script></body></html>"#;

    #[test]
    fn matches_the_mixdrop_family() {
        let extractor = MixDrop::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("mixdrop.ag"));
        assert!(supports("mixdrop.to"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn reads_a_preunpacked_wurl() {
        let fetcher = ScriptedFetcher::default().page(
            "/e/abc123",
            r#"<script>MDCore.wurl = "//v1.mxcontent.net/v2/abc123.mp4?s=tok&e=1700000000";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = MixDrop::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the pre-unpacked page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://v1.mxcontent.net/v2/abc123.mp4",
        );
        // Deliberately no request headers — the host is IP-gated, not
        // Referer-gated.
        assert!(streams[0].meta.request_headers.is_empty());
        assert_eq!(streams[0].ttl, TTL);
    }

    #[tokio::test]
    async fn unpacks_the_packed_wurl() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", PACKED_PAGE);
        let ctx = ctx_for(&fetcher, None);

        let streams = MixDrop::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the packed page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://v1.mxcontent.net/v2/abc123.mp4",
        );
        assert!(streams[0].meta.request_headers.is_empty());
    }

    #[tokio::test]
    async fn the_embed_fetch_carries_the_embed_as_referer() {
        let fetcher = ScriptedFetcher::default().page(
            "/e/abc123",
            r#"<script>MDCore.wurl = "https://v2.mxcontent.net/v2/abc123.mp4";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = MixDrop::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the absolute wurl must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Mp4,
            "https://v2.mxcontent.net/v2/abc123.mp4",
        );
        assert_eq!(
            fetcher.sent_header("/e/abc123", "Referer").as_deref(),
            Some("https://mixdrop.ag/e/abc123")
        );
    }

    #[tokio::test]
    async fn deleted_and_processing_files_are_misses() {
        for marker in ["File was deleted", "File Not Found", "video is processing"] {
            let fetcher = ScriptedFetcher::default().page("/e/abc123", format!("<p>{marker}</p>"));
            let ctx = ctx_for(&fetcher, None);

            match MixDrop::new().extract(&ctx, &url()).await {
                Err(ExtractorError::NotFound) => {}
                other => panic!("a {marker} page must be a NotFound, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn pages_without_a_wurl_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match MixDrop::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a wurl-less page must be a scrape failure, got {other:?}"),
        }
    }
}
