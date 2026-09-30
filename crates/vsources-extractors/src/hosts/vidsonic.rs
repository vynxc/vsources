//! `Vidsonic`: HLS from hex-obfuscated player variables.
//!
//! Ports `src/extractor/Vidsonic.js`. The page hides the playlist URL
//! in a hex literal (`const _0x1 = '…'`): the `|` separators are
//! stripped, the hex pairs decode to characters, and the decoded string
//! is reversed. Playback requires the embed's `Origin` header, which
//! also rides the height probe. The stream's lifetime tracks the URL's
//! `expires` parameter — 15-minute floor, 12-hour cap.
//!
//! Cut from the upstream port: the cheerio `title` selection (feeds
//! `meta.title`, which
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no field for)
//! and the `meta.height` passthrough (no provider height on
//! [`ResolveCtx`]).

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};

use crate::helpers::{first_capture, host_matcher};

host_matcher!(HOSTS, r"vidsonic");

static HEX_URL: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"const _0x1\s*=\s*'([^']+)'")
        .unwrap_or_else(|e| panic!("valid hex pattern: {e}"))
});
static PLAYLIST_HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid playlist height pattern: {e}"))
});

/// Upstream result lifetime: 12h — the cap for the `expires`-derived TTL.
const TTL_CAP: Duration = Duration::from_hours(12);
/// The TTL floor in ms, ports `Math.max(900000, …)`.
const TTL_FLOOR_MS: u64 = 900_000;
/// The expiry slack upstream subtracts from the token's lifetime, ms.
const EXPIRY_SLACK_MS: u64 = 120_000;

/// The `Vidsonic` extractor.
#[derive(Debug, Default)]
pub struct Vidsonic;

impl Vidsonic {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Vidsonic {
    fn id(&self) -> &'static str {
        "vidsonic"
    }

    fn label(&self) -> &'static str {
        "Vidsonic"
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
        // Upstream fetches the page without headers.
        let html = ctx
            .fetcher
            .request(FetchRequest::get(url.clone()))
            .await?
            .body;

        let hex = first_capture(&HEX_URL, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .ok_or_else(|| {
                ExtractorError::extraction(
                    self.id(),
                    "could not find hex-encoded video URL in Vidsonic page",
                )
            })?;
        let playlist = Url::parse(&decode_hex_url(&hex)).map_err(|e| {
            ExtractorError::extraction(self.id(), format!("invalid playlist URL: {e}"))
        })?;

        // Playback and the height probe both carry the embed's Origin.
        let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        let height = guess_height_from_playlist(ctx, &playlist, &origin).await;

        // `Math.max(900000, expires*1000 - now - 120000)`, capped at
        // the 12h class TTL. A missing or past `expires` floors at 15m.
        let ttl = token_ttl(&playlist).min(TTL_CAP);

        let mut stream = Stream::new(playlist, Format::Hls).with_ttl(ttl);
        stream.meta = stream.meta.with_header("Origin", origin);
        stream.meta.resolution = height;
        Ok(vec![stream])
    }
}

/// Ports `decodeHexUrl`: strip the `|` separators, decode each hex pair
/// to a character (a lone trailing digit parses like `parseInt`, an
/// invalid pair becomes NUL like `String.fromCharCode(NaN)`), then
/// reverse the decoded characters.
fn decode_hex_url(hex: &str) -> String {
    let joined: String = hex.split('|').collect();
    let mut decoded = String::with_capacity(joined.len() / 2);
    for pair in joined.as_bytes().chunks(2) {
        let digit = std::str::from_utf8(pair)
            .ok()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .unwrap_or(0);
        decoded.push(char::from(digit));
    }
    decoded.chars().rev().collect()
}

/// The `expires`-derived lifetime with its 15-minute floor.
fn token_ttl(playlist: &Url) -> Duration {
    let expires = playlist
        .query_pairs()
        .find(|(name, _)| name == "expires")
        .and_then(|(_, value)| value.parse::<u64>().ok());
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0));
    let token_ms = expires
        .and_then(|seconds| seconds.checked_mul(1000))
        .map_or(0, |expires_ms| {
            expires_ms
                .saturating_sub(now_ms)
                .saturating_sub(EXPIRY_SLACK_MS)
        });
    Duration::from_millis(token_ms.max(TTL_FLOOR_MS))
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
        Url::parse("https://vidsonic.to/e/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// Hex-encode `text` the way the page does: reversed characters,
    /// each a hex pair, optionally `|`-separated.
    fn hex_encode(text: &str, separator: &str) -> String {
        text.chars()
            .rev()
            .map(|c| format!("{:02x}", u32::from(c)))
            .collect::<Vec<_>>()
            .join(separator)
    }

    #[test]
    fn matches_the_vidsonic_family() {
        let extractor = Vidsonic::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("vidsonic.to"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn decodes_hex_urls_with_and_without_separators() {
        let target = "https://cdn.vidsonic.example/hls/master.m3u8";
        assert_eq!(decode_hex_url(&hex_encode(target, "")), target);
        assert_eq!(decode_hex_url(&hex_encode(target, "|")), target);
    }

    #[tokio::test]
    async fn decodes_the_playlist_and_caps_the_ttl_at_twelve_hours() {
        // A far-future `expires` pins the TTL at the 12h cap regardless
        // of the wall clock.
        let playlist = "https://cdn.vidsonic.example/hls/master.m3u8?expires=4102444800";
        let page = format!(
            "<html><title>Watch Some Movie</title><script>const _0x1 = '{}';</script></html>",
            hex_encode(playlist, "")
        );
        let fetcher = ScriptedFetcher::default().page("/e/abc123", page).page(
            "/hls/master.m3u8",
            "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1920x1080\n1080p/index.m3u8\n",
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = Vidsonic::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the hex page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.vidsonic.example/hls/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL_CAP);
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Origin")
                .map(String::as_str),
            Some("https://vidsonic.to")
        );
        assert_eq!(stream.meta.resolution, Some(1080));

        // The embed page was fetched without a Referer.
        assert!(fetcher.sent_header("/e/abc123", "Referer").is_none());

        // The height probe carried the Origin.
        assert_eq!(
            fetcher.sent_header("/hls/master.m3u8", "Origin").as_deref(),
            Some("https://vidsonic.to")
        );
    }

    #[tokio::test]
    async fn expired_tokens_floor_the_ttl_at_fifteen_minutes() {
        let playlist = "https://cdn.vidsonic.example/hls/master.m3u8?expires=1000";
        let page = format!(
            "<script>const _0x1 = '{}';</script>",
            hex_encode(playlist, "")
        );
        let fetcher = ScriptedFetcher::default()
            .page("/e/abc123", page)
            .page("/hls/master.m3u8", "#EXTM3U\n");
        let ctx = ctx_for(&fetcher, None);

        let streams = Vidsonic::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the hex page must resolve: {e}"));
        assert_eq!(streams[0].ttl, Duration::from_millis(TTL_FLOOR_MS));
    }

    #[tokio::test]
    async fn pages_without_the_hex_variable_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/e/abc123", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match Vidsonic::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("a hex-less page must be a scrape failure, got {other:?}"),
        }
    }
}
