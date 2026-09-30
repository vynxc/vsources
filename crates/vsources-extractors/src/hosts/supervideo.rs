//! `SuperVideo`: direct HLS from packed player pages, with a liveness
//! gate on the playlist.
//!
//! Ports `src/extractor/SuperVideo.js`. The embed page ships a
//! Dean-Edwards-packed player whose unpacked `sources:[{file:…}]`
//! initializer holds the playlist, with the hotlink Referer pinned to
//! the canonical host. Before shipping it, the playlist is probed: the
//! supervideo HLS edge stochastically serves an HTML `Loading…` gate
//! page instead of the playlist, and a gated URL is a guaranteed
//! playback error — a probe response that is not an `#EXTM3U` playlist
//! is dropped as a miss (a probe transport failure keeps the stream,
//! best-effort). Pages that only allow embedding retry once on the
//! `/e` form of their path.
//!
//! Cut from the upstream port: the cheerio `.download__title` selection
//! (feeds `meta.title`, which
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no field for)
//! and the `meta.height` passthrough (no provider height on
//! [`ResolveCtx`]). The liveness probe travels through the context
//! fetcher instead of a raw `fetch` call — the library has no side
//! channel.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};
use vsources_core::unpack::extract_url_from_packed;

use crate::helpers::{fetch_page_with, host_matcher};

host_matcher!(HOSTS, r"supervideo");

static PACKED_SOURCES: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"sources:\[\{file:"(.*?)""#)
        .unwrap_or_else(|e| panic!("valid sources pattern: {e}"))
});
static HEIGHT_AND_SIZE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d{3,}x(\d{3,}), ([\d.]+ ?[GM]B)")
        .unwrap_or_else(|e| panic!("valid height and size pattern: {e}"))
});
static UNAVAILABLE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(The file was deleted|The file expired|Video is processing)")
        .unwrap_or_else(|e| panic!("valid unavailable pattern: {e}"))
});
static PLAYLIST_HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid playlist height pattern: {e}"))
});

/// Upstream result lifetime: 3h.
const TTL: Duration = Duration::from_hours(3);
/// The playlist hotlink Referer, fixed to the canonical host.
const PLAYLIST_REFERER: &str = "https://supervideo.cc/";
/// The upstream liveness probe's client UA and timeout.
const PROBE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Bounded retries when the page demands the `/e` embed form.
const EMBED_ONLY_RETRIES: usize = 3;

/// The `SuperVideo` extractor.
#[derive(Debug, Default)]
pub struct SuperVideo;

impl SuperVideo {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for SuperVideo {
    fn id(&self) -> &'static str {
        "supervideo"
    }

    fn label(&self) -> &'static str {
        "SuperVideo"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    fn normalize(&self, url: &Url) -> Url {
        // `url.href.replace('/e/', '/').replace('/k/', '/').replace('/embed-', '/')`
        // — JS string replace swaps the first occurrence of each form.
        let normalized = url
            .as_str()
            .replacen("/e/", "/", 1)
            .replacen("/k/", "/", 1)
            .replacen("/embed-", "/", 1);
        Url::parse(&normalized).unwrap_or_else(|e| {
            panic!("supervideo normalization must produce a valid URL: {e}");
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);

        // `This video can be watched as embed only` → retry on
        // `/e${pathname}` at the same origin. Upstream recurses without
        // a bound; a page that stays embed-only is pathological and a
        // miss after a few retries.
        let mut current = url.clone();
        let html;
        let mut retries = 0;
        loop {
            let page = fetch_page_with(ctx, &current, referer).await?;
            if !page.contains("This video can be watched as embed only") {
                html = page;
                break;
            }
            retries += 1;
            if retries > EMBED_ONLY_RETRIES {
                return Err(ExtractorError::NotFound);
            }
            let embed_url = format!(
                "{}://{}/e{}",
                current.scheme(),
                current.host_str().unwrap_or_default(),
                current.path()
            );
            current = Url::parse(&embed_url).map_err(|e| {
                ExtractorError::extraction(self.id(), format!("invalid embed URL: {e}"))
            })?;
        }

        if UNAVAILABLE.is_match(&html).unwrap_or(false) {
            return Err(ExtractorError::NotFound);
        }

        // `extractUrlFromPacked` throws upstream when the packed block or
        // the stream link is missing; the Option port surfaces the same
        // failure as a scrape error.
        let playlist = extract_url_from_packed(&html, std::slice::from_ref(&*PACKED_SOURCES))
            .ok_or_else(|| {
                ExtractorError::extraction(self.id(), "no stream link in the packed embed")
            })?;

        // Liveness gate: a playlist that answers with anything but an
        // `#EXTM3U` header (the HTML "Loading…" gate) is a miss; a probe
        // that cannot complete keeps the stream.
        let probe = ctx
            .fetcher
            .request(
                FetchRequest::get(playlist.clone())
                    .with_header("User-Agent", PROBE_USER_AGENT)
                    .with_header("Referer", PLAYLIST_REFERER)
                    .with_timeout(PROBE_TIMEOUT),
            )
            .await;
        if let Ok(response) = probe
            && let Some(head) = response.body.get(..400.min(response.body.len()))
            && !head.contains("#EXTM3U")
        {
            tracing::debug!(
                host = playlist.host_str().unwrap_or_default(),
                status = response.status,
                "supervideo playlist gated or dead — dropping"
            );
            return Err(ExtractorError::NotFound);
        }

        let caps = HEIGHT_AND_SIZE
            .captures(&html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?;
        let (height, size) = match caps {
            Some(caps) => (
                caps.get(1)
                    .and_then(|group| group.as_str().parse::<u16>().ok()),
                caps.get(2)
                    .map(|group| group.as_str().to_string())
                    .as_deref()
                    .and_then(parse_size_bytes),
            ),
            // `meta.height ?? guessHeightFromPlaylist(…)` — no provider
            // height in this SDK, so the playlist probe is the fallback.
            None => (guess_height_from_playlist(ctx, &playlist).await, None),
        };

        let mut stream = Stream::new(playlist, Format::Hls)
            .with_ttl(TTL)
            .with_referer(PLAYLIST_REFERER);
        stream.meta.resolution = height;
        stream.meta.size = size;
        Ok(vec![stream])
    }
}

/// Ports `guessHeightFromPlaylist` for this host's playlist Referer:
/// fetch the playlist and take the tallest `\d+x(\d+)`/`(\d+)p` height.
async fn guess_height_from_playlist(ctx: &ResolveCtx<'_>, playlist: &Url) -> Option<u16> {
    let request = FetchRequest::get(playlist.clone()).with_header("Referer", PLAYLIST_REFERER);
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

/// Ports `bytes.parse` for the `[GM]B` sizes these pages carry: binary
/// multipliers with a floored result, exactly like the npm package.
fn parse_size_bytes(size: &str) -> Option<u64> {
    let compact = size.replace(' ', "");
    let split = compact.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = (&compact[..split], &compact[split..]);
    let multiplier = match unit {
        "GB" => 1u64 << 30,
        "MB" => 1u64 << 20,
        _ => return None,
    };
    // Exact decimal math: floor((digits × multiplier) / scale).
    let (digits, decimals) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let scale = 10u64.checked_pow(u32::try_from(decimals.len()).ok()?)?;
    let fraction: u64 = if decimals.is_empty() {
        0
    } else {
        decimals.parse().ok()?
    };
    let numerator = whole.checked_mul(scale)?.checked_add(fraction)?;
    numerator.checked_mul(multiplier).map(|bytes| bytes / scale)
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://supervideo.cc/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A packed player page in the upstream shape with the page-level
    /// `WxH, size` markers, plus a live playlist to probe.
    fn packed_page() -> String {
        r#"<html><body><h1 class="download__title">Some Movie</h1><p>1920x1080, 1.4 GB</p><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('sources:[{file:"1"}]',10,3,'1|https://hfs.serversicuro.example/8f2/master.m3u8|'.split('|'),0,{}))</script></body></html>"#
            .to_string()
    }

    #[test]
    fn matches_the_supervideo_family() {
        let extractor = SuperVideo::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("supervideo.cc"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_the_embed_player_and_download_forms() {
        let extractor = SuperVideo::new();
        let forms = [
            (
                "https://supervideo.cc/e/abc123",
                "https://supervideo.cc/abc123",
            ),
            (
                "https://supervideo.cc/k/abc123",
                "https://supervideo.cc/abc123",
            ),
            (
                "https://supervideo.cc/embed-abc123",
                "https://supervideo.cc/abc123",
            ),
        ];
        for (input, expected) in forms {
            let url = Url::parse(input).unwrap_or_else(|e| panic!("valid URL: {e}"));
            assert_eq!(extractor.normalize(&url).as_str(), expected);
        }
    }

    #[tokio::test]
    async fn ships_the_playlist_after_the_liveness_gate() {
        let fetcher = ScriptedFetcher::default()
            .page("/abc123", packed_page())
            .page(
                "/8f2/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = SuperVideo::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the packed page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://hfs.serversicuro.example/8f2/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://supervideo.cc/")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        // `bytes.parse("1.4 GB")` = floor(1.4 × 1024³).
        assert_eq!(stream.meta.size, Some(1_503_238_553));

        // The probe carried the pinned Referer and UA.
        assert_eq!(
            fetcher
                .sent_header("/8f2/master.m3u8", "Referer")
                .as_deref(),
            Some("https://supervideo.cc/")
        );
        assert_eq!(
            fetcher
                .sent_header("/8f2/master.m3u8", "User-Agent")
                .as_deref(),
            Some(PROBE_USER_AGENT)
        );
    }

    #[tokio::test]
    async fn guesses_height_from_the_playlist_when_the_page_has_none() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/abc123",
                packed_page().replace("1920x1080, 1.4 GB", "watch online"),
            )
            .page(
                "/8f2/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\n720p/index.m3u8\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = SuperVideo::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the height fallback must resolve: {e}"));
        assert_eq!(streams[0].meta.resolution, Some(720));
        assert_eq!(streams[0].meta.size, None);
    }

    #[tokio::test]
    async fn gated_playlists_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/abc123", packed_page())
            .page("/8f2/master.m3u8", "<html><body>Loading...</body></html>");
        let ctx = ctx_for(&fetcher, None);

        match SuperVideo::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a gated playlist must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deleted_expired_and_processing_files_are_misses() {
        for marker in [
            "The file was deleted",
            "The file expired",
            "Video is processing",
        ] {
            let fetcher = ScriptedFetcher::default().page("/abc123", format!("<p>{marker}</p>"));
            let ctx = ctx_for(&fetcher, None);

            match SuperVideo::new().extract(&ctx, &url()).await {
                Err(ExtractorError::NotFound) => {}
                other => panic!("a {marker} page must be a NotFound, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn embed_only_pages_retry_on_the_embed_form() {
        let fetcher = ScriptedFetcher::default()
            .page("/abc123", "This video can be watched as embed only")
            .page("/e/abc123", packed_page())
            .page("/8f2/master.m3u8", "#EXTM3U\n");
        let ctx = ctx_for(&fetcher, None);

        let streams = SuperVideo::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the embed retry must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://hfs.serversicuro.example/8f2/master.m3u8",
        );
    }

    #[tokio::test]
    async fn pages_without_a_packed_link_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/abc123", "<p>nothing packed here</p>");
        let ctx = ctx_for(&fetcher, None);

        match SuperVideo::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("an unpackable page must be a scrape failure, got {other:?}"),
        }
    }
}
