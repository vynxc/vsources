//! `HiAnime`: HLS from the shared `aniwatchtv.uk` backend.
//!
//! Ports `src/extractor/HiAnime.js`. Claims URLs from the `hianime`
//! source — HLS playlists on `hls*.aniwatchtv.uk` that only play with
//! `Referer: https://zokoanime.video/`. Upstream routed them through a
//! server-side `/proxy` because its desktop player could not send
//! per-stream headers; this library has no server, so the playlist
//! ships directly with that Referer in
//! [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers).
//!
//! `zoko_hls_stream` is the shared hop: `AnimeKai` uses the same
//! backend and the same Referer, so it lives here for both.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

/// Upstream result lifetime: 30min.
const TTL: Duration = Duration::from_mins(30);
/// The Referer upstream's proxy hop sent for aniwatchtv.uk playlists.
pub(crate) const ZOKO_REFERER: &str = "https://zokoanime.video/";

/// The shared `aniwatchtv.uk` hop: a playlist that ships directly with
/// the `zokoanime.video` Referer attached.
///
/// `HiAnime` and `AnimeKai` share this backend (upstream keeps two identical
/// extractors for the two source ids); the helper lives here and is
/// `pub(crate)` for `animekai`.
pub(crate) fn zoko_hls_stream(url: &Url, ttl: Duration, label: &str) -> Stream {
    Stream::new(url.clone(), Format::Hls)
        .with_label(label)
        .with_ttl(ttl)
        .with_referer(ZOKO_REFERER)
}

/// The `HiAnime` extractor.
#[derive(Debug, Default)]
pub struct HiAnime;

impl HiAnime {
    /// A new extractor; stateless — the passthrough never fetches.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for HiAnime {
    fn id(&self) -> &'static str {
        "hianime"
    }

    fn label(&self) -> &'static str {
        "HiAnime"
    }

    fn supports(&self, ctx: &ResolveCtx<'_>, _url: &Url) -> bool {
        // Only URLs from the HiAnime source.
        ctx.source_id == Some(self.id())
    }

    async fn extract(
        &self,
        _ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        Ok(vec![zoko_hls_stream(url, TTL, self.label())])
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://hls2.aniwatchtv.uk/v/abc123/master.m3u8?token=xyz")
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
    fn claims_only_hianime_source_urls() {
        let extractor = HiAnime::new();
        let fetcher = ScriptedFetcher::default();
        let url = url();

        assert!(extractor.supports(&ctx_with_source(&fetcher, Some("hianime")), &url));
        assert!(!extractor.supports(&ctx_with_source(&fetcher, Some("animekai")), &url));
        assert!(!extractor.supports(&ctx_for(&fetcher, None), &url));
    }

    #[tokio::test]
    async fn passes_the_playlist_through_with_the_zoko_referer() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = HiAnime::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the passthrough must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://hls2.aniwatchtv.uk/v/abc123/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some(ZOKO_REFERER)
        );
    }

    #[tokio::test]
    async fn resolves_without_any_requests() {
        // A pure passthrough: even with nothing scripted, the URL
        // resolves — upstream never fetched for HiAnime either.
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);

        let streams = HiAnime::new()
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
