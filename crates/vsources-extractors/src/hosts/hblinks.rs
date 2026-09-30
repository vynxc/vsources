//! `HBLinks`: hub link-directory pages.
//!
//! Ports `src/extractor/HBLinks.js`. An hblinks page lists the hub
//! family's links (hubcloud, hubcdn, hubdrive, …) for one title: the
//! page is fetched once, its links are canonicalized through
//! [`HubExtractor::normalize_async`] **in parallel**, deduplicated by
//! canonical URL, and every survivor is resolved by the shared
//! [`Arc<HubExtractor>`] (upstream Task-41: the archive pages carry 2-3
//! independent multi-hop targets — each a 5-20s redirect chain — and
//! the old sequential loop could not finish inside the wrapper's 33s
//! race).
//!
//! Port notes (upstream features cut, with reasons):
//!
//! - **Per-link error cards are cut.** Upstream's base class converts a
//!   failed link into an external error stream; with no server those
//!   cards are noise — failures are logged and the link skipped, the
//!   same concern the registry's external fallback owns.
//! - **`lazyExtract` and the 2-minute TTL are registry concerns** —
//!   every inner stream already carries the hub family's 5-minute TTL,
//!   so the upstream default never applies.
//! - **`meta.title` is cut** (no title field on
//!   [`StreamMeta`](vsources_core::types::StreamMeta)); the page title
//!   still feeds the language/resolution enrichment below.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use scraper::Html;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::language::find_country_codes;
use vsources_core::resolution::find_height;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

use super::hubcloud::{HUB_HOST_PATTERN, HubMeta, anchor_pairs, title_text};
use super::hubextractor::HubExtractor;
use crate::helpers::fetch_page_with;

/// The `HBLinks` directory extractor, delegating to the shared hub
/// family extractor.
#[derive(Debug)]
pub struct HBLinks {
    /// The hub family front door (upstream `hubExtractor`).
    hub_extractor: Arc<HubExtractor>,
}

impl HBLinks {
    /// A new directory extractor over the shared hub extractor — build
    /// it from the same [`Arc`] the registry registers so the caches
    /// and the in-flight resolution are shared.
    #[must_use]
    pub fn new(hub_extractor: Arc<HubExtractor>) -> Self {
        Self { hub_extractor }
    }
}

#[async_trait]
impl Extractor for HBLinks {
    fn id(&self) -> &'static str {
        "hblinks"
    }

    fn label(&self) -> &'static str {
        "HUBLinks"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| host.to_ascii_lowercase().contains("hblinks"))
    }

    fn cache_version(&self) -> Option<u32> {
        Some(2)
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);
        let html = match fetch_page_with(ctx, url, referer).await {
            Ok(html) => html,
            Err(error) => {
                tracing::warn!("HBLinks page fetch failed for {}: {error}", url.as_str());
                return Err(ExtractorError::NotFound);
            }
        };

        // Parse once and drop the document before the parallel awaits
        // below: `Html` is not `Send`.
        let (page_title, hub_links) = {
            let document = Html::parse_document(&html);
            (
                title_text(&document).trim().to_string(),
                extract_hub_links(&document, url),
            )
        };

        // Page-title enrichment — the incoming meta is empty at the
        // trait boundary, so this is `findCountryCodes` /
        // `meta.height ?? findHeight` over the page title alone.
        let updated_meta = HubMeta {
            country_codes: find_country_codes(&page_title),
            height: find_height(&page_title),
            size: None,
        };

        // Deduplicate by canonical URL — hubdrive and hubcloud may
        // resolve to the same file.
        let canonical = join_all(
            hub_links
                .iter()
                .map(|link| self.hub_extractor.normalize_async(ctx, link)),
        )
        .await;
        let mut seen_canonical = HashSet::new();
        let unique_links: Vec<&Url> = canonical
            .iter()
            .zip(&hub_links)
            .filter_map(|(canonical, link)| {
                seen_canonical.insert(canonical.as_str()).then_some(link)
            })
            .collect();

        // Task-41: resolve the page's hub links in PARALLEL.
        let resolved = join_all(unique_links.iter().map(|link| {
            self.hub_extractor
                .extract_internal(ctx, link, &updated_meta)
        }))
        .await;

        let mut streams = Vec::new();
        for (link, result) in unique_links.iter().zip(resolved) {
            match result {
                Ok(found) => streams.extend(found),
                Err(error) => {
                    tracing::warn!("HBLinks extraction failed for {}: {error}", link.as_str());
                }
            }
        }
        if streams.is_empty() {
            return Err(ExtractorError::NotFound);
        }
        Ok(streams)
    }
}

/// All hub links on the page, deduplicated by URL — upstream
/// `extractHubLinks`: every anchor whose href matches the family
/// pattern, resolved against the page.
fn extract_hub_links(document: &Html, page_url: &Url) -> Vec<Url> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    for (_text, href) in anchor_pairs(document) {
        let Some(href) = href else { continue };
        if !HUB_HOST_PATTERN
            .is_match(&href.to_lowercase())
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(parsed) = page_url.join(&href)
            && seen.insert(parsed.as_str().to_string())
        {
            links.push(parsed);
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::hosts::hubcloud::HUBCLOUD_CACHE_TTL;
    use crate::testing::{ScriptedFetcher, ctx_for};

    use super::*;
    fn url() -> Url {
        Url::parse("https://hblinks.co/dir/1").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A hubcloud chain (redirect page + download page) for `/file/xyz`.
    fn hubcloud_chain(fetcher: ScriptedFetcher) -> ScriptedFetcher {
        fetcher
            .page("/file/xyz", r#"<script>var url = "/dl/xyz";</script>"#)
            .page(
                "/dl/xyz",
                concat!(
                    r#"<html><head><title>Movie 1080p English</title></head><body>"#,
                    r#"<div id="size">1.4 GB</div>"#,
                    r#"<a href="https://hubcloud.one/workers/file1">Download File</a>"#,
                    r#"</body></html>"#,
                ),
            )
    }

    #[test]
    fn matches_hblinks_hosts() {
        let extractor = HBLinks::new(Arc::new(HubExtractor::new()));
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/dir/1"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("hblinks.co"));
        assert!(supports("hblinks.xyz"));
        assert!(!supports("hubcloud.one"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn reports_the_upstream_cache_version() {
        assert_eq!(
            HBLinks::new(Arc::new(HubExtractor::new())).cache_version(),
            Some(2)
        );
    }

    #[tokio::test]
    async fn resolves_the_directory_links() {
        let fetcher = hubcloud_chain(ScriptedFetcher::default()).page(
            "/dir/1",
            concat!(
                r#"<html><head><title>Movie 720p English</title></head><body>"#,
                r#"<a href="https://hubcloud.one/file/xyz?token=a">HubCloud 1</a>"#,
                r#"<a href="https://hubcloud.one/file/xyz?token=b">HubCloud 2</a>"#,
                r#"<a href="https://example.com/nope">Other</a>"#,
                r#"</body></html>"#,
            ),
        );
        // The provider page that linked to the directory — the context
        // referer upstream calls `meta.referer`.
        let provider = Url::parse("https://provider.example/movie")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        let ctx = ctx_for(&fetcher, Some(&provider));

        let streams = HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the directory must resolve: {e}"));
        // The two tokenized links share a canonical hubcloud URL — one
        // extraction survives.
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.url.as_str(), "https://hubcloud.one/workers/file1");
        assert_eq!(stream.ttl, HUBCLOUD_CACHE_TTL);
        // The directory page's title won the enrichment race: 720, not
        // the download page's 1080.
        assert_eq!(stream.meta.resolution, Some(720));
        assert_eq!(
            stream.meta.languages,
            vec![vsources_core::types::CountryCode::En]
        );
        // The directory page and the hubcloud hop both carry the
        // provider page as Referer (upstream `meta.referer ?? url.href`
        // — the referer flows down the chain).
        assert_eq!(
            fetcher.sent_header("/dir/1", "Referer").as_deref(),
            Some("https://provider.example/movie")
        );
        assert_eq!(
            fetcher.sent_header("/file/xyz", "Referer").as_deref(),
            Some("https://provider.example/movie")
        );
        // The download page keeps the hubcloud link itself — the
        // ORIGINAL, still carrying its session token.
        assert_eq!(
            fetcher.sent_header("/dl/xyz", "Referer").as_deref(),
            Some("https://hubcloud.one/file/xyz?token=a")
        );
    }

    #[tokio::test]
    async fn resolves_distinct_links_in_parallel() {
        let fetcher = hubcloud_chain(ScriptedFetcher::default())
            .page(
                "/dir/1",
                concat!(
                    r#"<html><head><title>Movie</title></head><body>"#,
                    r#"<a href="https://hubcloud.one/file/xyz">HubCloud A</a>"#,
                    r#"<a href="https://hubdrive.pics/drive/def">HubDrive B</a>"#,
                    r#"</body></html>"#,
                ),
            )
            .page(
                "/drive/def",
                r#"<a href="https://hubcloud.one/file/abc">HubCloud</a>"#,
            )
            .page("/file/abc", r#"<script>var url = "/dl/abc";</script>"#)
            .page(
                "/dl/abc",
                r#"<html><head><title>Movie</title></head><body><a href="https://hubcloud.one/workers/file2">Download File</a></body></html>"#,
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the directory must resolve: {e}"));
        let urls: Vec<&str> = streams.iter().map(|stream| stream.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://hubcloud.one/workers/file1",
                "https://hubcloud.one/workers/file2"
            ]
        );
    }

    #[tokio::test]
    async fn deduplicates_links_that_resolve_to_the_same_file() {
        let fetcher = hubcloud_chain(ScriptedFetcher::default()).page(
            "/dir/1",
            concat!(
                r#"<html><head><title>Movie</title></head><body>"#,
                r#"<a href="https://hubcloud.one/file/xyz?token=a">HubCloud</a>"#,
                r#"<a href="https://hubdrive.pics/drive/abc">HubDrive</a>"#,
                r#"</body></html>"#,
            ),
        );
        // The hubdrive page resolves to the same hubcloud file.
        let fetcher = fetcher.page(
            "/drive/abc",
            r#"<a href="https://hubcloud.one/file/xyz">HubCloud</a>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the directory must resolve: {e}"));
        assert_eq!(
            streams.len(),
            1,
            "hubdrive and hubcloud collapse into one file"
        );
    }

    #[tokio::test]
    async fn failing_hub_links_are_skipped() {
        // The second hubcloud link has no page behind it — it fails and
        // is skipped while the first resolves.
        let fetcher = hubcloud_chain(ScriptedFetcher::default()).page(
            "/dir/1",
            concat!(
                r#"<html><head><title>Movie</title></head><body>"#,
                r#"<a href="https://hubcloud.one/file/xyz">HubCloud</a>"#,
                r#"<a href="https://hubcloud.one/file/missing">HubCloud</a>"#,
                r#"</body></html>"#,
            ),
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the live link must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://hubcloud.one/workers/file1"
        );
    }

    #[tokio::test]
    async fn pages_without_hub_links_are_misses() {
        let fetcher = ScriptedFetcher::default().page(
            "/dir/1",
            r#"<html><head><title>Movie</title></head><body><a href="https://example.com/nope">Other</a></body></html>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        match HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
        {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a link-less directory must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_page_failures_are_misses() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        match HBLinks::new(Arc::new(HubExtractor::new()))
            .extract(&ctx, &url())
            .await
        {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a failed directory fetch must be a NotFound, got {other:?}"),
        }
    }
}
