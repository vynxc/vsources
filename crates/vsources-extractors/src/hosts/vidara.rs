//! `Vidara`: HLS from the host's JSON stream API.
//!
//! Ports `src/extractor/Vidara.js`. The embed URL's filecode posts to
//! `/api/stream` (`{"filecode":…,"device":"web"}`); the response carries
//! the direct `streaming_url` playlist. Playback requires the embed's
//! `Origin` header, which also rides the height probe.
//!
//! Cut from the upstream port: `meta.title` (the API's `title` field —
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no title field)
//! and the `meta.height` passthrough (no provider height on
//! [`ResolveCtx`]).

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::host_matcher;

host_matcher!(HOSTS, r"vidara");

static PLAYLIST_HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid playlist height pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `Vidara` extractor.
#[derive(Debug, Default)]
pub struct Vidara;

impl Vidara {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Vidara {
    fn id(&self) -> &'static str {
        "vidara"
    }

    fn label(&self) -> &'static str {
        "Vidara"
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
        // `url.pathname.split('/').filter(Boolean).pop()` — the last
        // non-empty segment.
        let filecode = url
            .path()
            .split('/')
            .rfind(|segment| !segment.is_empty())
            .ok_or_else(|| {
                ExtractorError::extraction(self.id(), "could not extract filecode from Vidara URL")
            })?;

        // POST `${origin}/api/stream` with the JSON body.
        let api_url = Url::parse(&format!(
            "{}://{}/api/stream",
            url.scheme(),
            url.host_str().unwrap_or_default()
        ))
        .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid API URL: {e}")))?;
        let body = format!(r#"{{"filecode":"{filecode}","device":"web"}}"#);
        let response = ctx
            .fetcher
            .request(
                FetchRequest::post(api_url, body).with_header("Content-Type", "application/json"),
            )
            .await?;

        let data: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid API response: {e}"))
        })?;
        let streaming_url = data
            .get("streaming_url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ExtractorError::extraction(self.id(), "no streaming_url in Vidara API response")
            })?;
        let playlist = Url::parse(streaming_url).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid playlist URL: {e}"))
        })?;

        // Playback and the height probe both carry the embed's Origin.
        let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        let height = guess_height_from_playlist(ctx, &playlist, &origin).await;

        let mut stream = Stream::new(playlist, Format::Hls).with_ttl(TTL);
        stream.meta = stream.meta.with_header("Origin", origin);
        stream.meta.resolution = height;
        Ok(vec![stream])
    }
}

/// Ports `guessHeightFromPlaylist` with the embed's `Origin` header:
/// fetch the playlist and take the tallest `\d+x(\d+)`/`(\d+)p` height.
async fn guess_height_from_playlist(
    ctx: &ResolveCtx<'_>,
    playlist: &Url,
    origin: &str,
) -> Option<u16> {
    let request = FetchRequest::get(playlist.clone()).with_header("Origin", origin);
    let body = ctx.fetcher.request(request).await.ok()?.body;
    let mut best: Option<u16> = None;
    for caps in PLAYLIST_HEIGHT.captures_iter(&body).flatten() {
        let height = caps
            .get(1)
            .or_else(|| caps.get(2))
            .and_then(|group| group.as_str().parse::<u16>().ok());
        if let Some(height) = height {
            best = Some(best.map_or(height, |current| current.max(height)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://vidara.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    #[test]
    fn matches_the_vidara_family() {
        let extractor = Vidara::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("vidara.to"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn posts_for_the_streaming_url_and_probes_the_height() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/api/stream",
                r#"{"streaming_url":"https://cdn.vidara.example/hls/master.m3u8","title":"Some Movie"}"#,
            )
            .page(
                "/hls/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1920x1080\n1080p/index.m3u8\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = Vidara::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the API response must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.vidara.example/hls/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some("https://vidara.to")
        );
        assert_eq!(stream.meta.resolution, Some(1080));

        // The POST carried the JSON body and content type.
        let posts = fetcher
            .requests()
            .into_iter()
            .filter(|request| request.url.path() == "/api/stream")
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].method, "POST");
        assert_eq!(
            posts[0].body.as_deref(),
            Some(r#"{"filecode":"abc123","device":"web"}"#)
        );
        assert_eq!(
            posts[0].headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );

        // The height probe carried the Origin.
        assert_eq!(
            fetcher.sent_header("/hls/master.m3u8", "Origin").as_deref(),
            Some("https://vidara.to")
        );
    }

    #[tokio::test]
    async fn responses_without_a_streaming_url_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/api/stream", r#"{"error":"not found"}"#);
        let ctx = ctx_for(&fetcher, None);

        match Vidara::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a streamless response must be a scrape failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn urls_without_a_filecode_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://vidara.to/").unwrap_or_else(|e| panic!("valid URL: {e}"));

        match Vidara::new().extract(&ctx, &url).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a filecode-less URL must be a scrape failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_api_json_is_a_scrape_failure() {
        let fetcher = ScriptedFetcher::default().page("/api/stream", "not json");
        let ctx = ctx_for(&fetcher, None);

        match Vidara::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("invalid JSON must be a scrape failure, got {other:?}"),
        }
    }
}
