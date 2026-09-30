//! `StreamEmbed`: HLS from `video = {…}` JSON player variables.
//!
//! Ports `src/extractor/StreamEmbed.js`. The page assigns a `video`
//! object (upstream `JSON.parse`s it; this port uses `serde_json`); the
//! playlist URL is built from its `uid`/`md5`/`id`/`status` fields on
//! the embed's own origin, and the quality list's first entry becomes
//! the height.
//!
//! Upstream routes the playlist through `MediaFlowProxy` when the addon
//! configures it — there is no server in this library, so the stream
//! keeps the direct URL and carries the `Referer` the proxy would have
//! sent (the embed origin).
//!
//! Cut from the upstream port: `meta.title`
//! (`decodeURIComponent(video.title)` —
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no title field)
//! and the `MediaFlowProxy` routing itself.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"bullstream|mp4player|watch\.gxplayer");

static VIDEO: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"video ?= ?(.*);")
        .unwrap_or_else(|e| panic!("valid video pattern: {e}"))
});

/// Upstream result lifetime: 6h.
const TTL: Duration = Duration::from_hours(6);

/// The `StreamEmbed` family extractor.
#[derive(Debug, Default)]
pub struct StreamEmbed;

impl StreamEmbed {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for StreamEmbed {
    fn id(&self) -> &'static str {
        "streamembed"
    }

    fn label(&self) -> &'static str {
        "StreamEmbed"
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

        if html.contains("Video is not ready") {
            return Err(ExtractorError::NotFound);
        }

        // `html.match(/video ?= ?(.*);/)` — upstream throws NotFoundError
        // when the assignment is missing.
        let video_json = first_capture(&VIDEO, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or(ExtractorError::NotFound)?;
        let video: Value = serde_json::from_str(&video_json).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid video object: {e}"))
        })?;

        // `${video.uid}` & friends — the template fields upstream
        // interpolates. A missing field would render `undefined` there;
        // here it is a scrape failure.
        let uid = template_field(self.id(), &video, "uid")?;
        let md5 = template_field(self.id(), &video, "md5")?;
        let id = template_field(self.id(), &video, "id")?;
        let status = template_field(self.id(), &video, "status")?;

        // `/m3u8/${uid}/${md5}/master.txt?s=1&id=${id}&cache=${status}`.
        let mut playlist = Url::parse(&format!(
            "{}://{}/m3u8/{}/{}/master.txt",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            uid,
            md5,
        ))
        .map_err(|e| ExtractorError::extraction(self.id(), format!("invalid playlist URL: {e}")))?;
        {
            let mut query = playlist.query_pairs_mut();
            query.append_pair("s", "1");
            query.append_pair("id", &id);
            query.append_pair("cache", &status);
        }

        let height = quality_height(video.get("quality"));
        let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());

        let mut stream = Stream::new(playlist, Format::Hls).with_ttl(TTL);
        // The headers the MediaFlowProxy hop would have carried — the
        // client sends them directly.
        stream.meta = stream.meta.with_header("Referer", origin);
        stream.meta.resolution = height;
        Ok(vec![stream])
    }
}

/// `${video[name]}` — a JSON scalar as a template-literal string.
fn template_field(extractor_id: &str, video: &Value, name: &str) -> Result<String, ExtractorError> {
    video.get(name).and_then(scalar_to_string).ok_or_else(|| {
        ExtractorError::extraction(extractor_id, format!("missing {name} in video object"))
    })
}

/// `String(value)` for the JSON scalars a template literal accepts.
fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// `JSON.parse(video.quality)[0]` parsed with `parseInt` — the first
/// quality entry's leading digits, when the list parses at all.
fn quality_height(quality: Option<&Value>) -> Option<u16> {
    // `video.quality` is a JSON string holding the list; an actual
    // array is taken directly.
    let parsed: Value = match quality? {
        Value::String(text) => serde_json::from_str(text).ok()?,
        other => other.clone(),
    };
    let first = parsed.as_array()?.first()?;
    match first {
        Value::String(text) => parse_leading_u16(text),
        Value::Number(number) => number.as_u64().and_then(|v| u16::try_from(v).ok()),
        _ => None,
    }
}

/// `parseInt(text)` — the digits at the start of `text`, after
/// `parseInt`'s leading-whitespace skip.
fn parse_leading_u16(text: &str) -> Option<u16> {
    let trimmed = text.trim_start();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    trimmed[..digits_end].parse().ok()
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://watch.gxplayer.com/embed/abc123")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A player page in the upstream shape: one `video = {…};` line.
    const VIDEO_PAGE: &str = r#"<html><body><script>
var video = {"uid":"u42","md5":"9a1b","id":1337,"status":1,"quality":"[\"1080\",\"720\"]","title":"My%20Title"};
</script></body></html>"#;

    #[test]
    fn matches_the_streamembed_family() {
        let extractor = StreamEmbed::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("bullstream.com"));
        assert!(supports("mp4player.xyz"));
        assert!(supports("watch.gxplayer.com"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn builds_the_playlist_from_the_video_object() {
        let fetcher = ScriptedFetcher::default().page("/embed/abc123", VIDEO_PAGE);
        let ctx = ctx_for(&fetcher, None);

        let streams = StreamEmbed::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the video object must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://watch.gxplayer.com/m3u8/u42/9a1b/master.txt",
        );
        let stream = &streams[0];
        assert_eq!(
            stream.url.as_str(),
            "https://watch.gxplayer.com/m3u8/u42/9a1b/master.txt?s=1&id=1337&cache=1"
        );
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://watch.gxplayer.com")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
    }

    #[tokio::test]
    async fn unready_videos_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/embed/abc123", "<p>Video is not ready</p>");
        let ctx = ctx_for(&fetcher, None);

        match StreamEmbed::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an unready video must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pages_without_a_video_object_are_misses() {
        let fetcher = ScriptedFetcher::default().page("/embed/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match StreamEmbed::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a video-less page must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_video_json_is_a_scrape_failure() {
        let fetcher = ScriptedFetcher::default().page("/embed/abc123", "var video = {not json};\n");
        let ctx = ctx_for(&fetcher, None);

        match StreamEmbed::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("malformed JSON must be a scrape failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn video_objects_missing_fields_are_scrape_failures() {
        let fetcher =
            ScriptedFetcher::default().page("/embed/abc123", "var video = {\"uid\":\"u42\"};\n");
        let ctx = ctx_for(&fetcher, None);

        match StreamEmbed::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a fieldless video object must be a scrape failure, got {other:?}"),
        }
    }

    #[test]
    fn parses_quality_heights_like_parse_int() {
        let string_list = serde_json::json!("[\"1080\",\"720\"]");
        assert_eq!(quality_height(Some(&string_list)), Some(1080));
        let number_list = serde_json::json!([1080, 720]);
        assert_eq!(quality_height(Some(&number_list)), Some(1080));
        let quality_label = serde_json::json!(r#"["1080p"]"#);
        assert_eq!(quality_height(Some(&quality_label)), Some(1080));
        let empty = serde_json::json!("[]");
        assert_eq!(quality_height(Some(&empty)), None);
        let invalid = serde_json::json!("not json");
        assert_eq!(quality_height(Some(&invalid)), None);
        assert_eq!(quality_height(None), None);
    }
}
