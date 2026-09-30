//! `Dropload`: direct HLS from packed player pages.
//!
//! Ports `src/extractor/Dropload.js`. The embed page ships a Dean
//! Edwards-packed player bundle; the playlist URL hides inside the
//! unpacked `sources:[{file:…}]` initializer, with the hotlink Referer
//! pinned to the canonical mirror host. Height and file size come from
//! the raw page's `WxH,`/`GB` markers, with a playlist probe as the
//! height fallback.
//!
//! Cut from the upstream port: `meta.title` (the cheerio
//! `.videoplayer h1` text) and the `meta.height` passthrough —
//! [`StreamMeta`](vsources_core::types::StreamMeta) has no title field
//! and [`ResolveCtx`] carries no provider height; the playlist probe
//! covers the height fallback.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream};
use vsources_core::unpack::extract_url_from_packed;

use crate::helpers::{fetch_page_with, first_capture, host_matcher};

host_matcher!(HOSTS, r"dropload|dr0pstream");

static PACKED_SOURCES: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"sources:\[\{file:"(.*?)""#)
        .unwrap_or_else(|e| panic!("valid sources pattern: {e}"))
});
static HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d{3,}x(\d{3,}),")
        .unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});
static SIZE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"([\d.]+ ?[GM]B)")
        .unwrap_or_else(|e| panic!("valid size pattern: {e}"))
});
static PLAYLIST_HEIGHT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"\d+x(\d+)|(\d+)p")
        .unwrap_or_else(|e| panic!("valid playlist height pattern: {e}"))
});

/// Upstream result lifetime: 2h.
const TTL: Duration = Duration::from_hours(2);
/// The playlist hotlink Referer, fixed to the canonical mirror host.
const PLAYLIST_REFERER: &str = "https://dr0pstream.com/";

/// The `Dropload` family extractor.
#[derive(Debug, Default)]
pub struct Dropload;

impl Dropload {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Dropload {
    fn id(&self) -> &'static str {
        "dropload"
    }

    fn label(&self) -> &'static str {
        "Dropload"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| HOSTS.is_match(host).unwrap_or(false))
    }

    fn normalize(&self, url: &Url) -> Url {
        // `url.href.replace('/d/', '/').replace('/e/', '/').replace('/embed-', '/')`
        // — JS string replace swaps the first occurrence of each form.
        let normalized = url
            .as_str()
            .replacen("/d/", "/", 1)
            .replacen("/e/", "/", 1)
            .replacen("/embed-", "/", 1);
        Url::parse(&normalized).unwrap_or_else(|e| {
            panic!("dropload normalization must produce a valid URL: {e}");
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = ctx.referer.unwrap_or(url);
        let html = fetch_page_with(ctx, url, referer).await?;

        if html.contains("File Not Found") || html.contains("Pending in queue") {
            return Err(ExtractorError::NotFound);
        }

        // `extractUrlFromPacked` throws upstream when the packed block or
        // the stream link is missing; the Option port surfaces the same
        // failure as a scrape error.
        let playlist = extract_url_from_packed(&html, std::slice::from_ref(&*PACKED_SOURCES))
            .ok_or_else(|| {
                ExtractorError::extraction(self.id(), "no stream link in the packed embed")
            })?;

        let height = match first_capture(&HEIGHT, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
        {
            Some(height) => height.parse::<u16>().ok(),
            None => guess_height_from_playlist(ctx, &playlist).await,
        };
        let size = first_capture(&SIZE, &html)
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?
            .as_deref()
            .and_then(parse_size_bytes);

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
        Url::parse("https://dropload.com/abc123").unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A packed player page in the upstream shape with the page-level
    /// `WxH, size` markers.
    const PACKED_PAGE: &str = r#"<html><body><div class="videoplayer"><h1>Some Movie</h1><p>1920x1080, 1.4 GB</p></div><script>eval(function(p,a,c,k,e,d){e=function(c){return c};if(!''.replace(/^/,String)){while(c--){d[c]=k[c]||c}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('sources:[{file:"1"}]',10,3,'1|https://cdn.dropload.example/hls/master.m3u8|'.split('|'),0,{}))</script></body></html>"#;

    #[test]
    fn matches_the_dropload_family() {
        let extractor = Dropload::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/e/abc"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("dropload.com"));
        assert!(supports("dr0pstream.com"));
        assert!(!supports("example.com"));
    }

    #[test]
    fn normalizes_the_download_and_embed_forms() {
        let extractor = Dropload::new();
        let forms = [
            (
                "https://dropload.com/d/abc123",
                "https://dropload.com/abc123",
            ),
            (
                "https://dr0pstream.com/e/abc123",
                "https://dr0pstream.com/abc123",
            ),
            (
                "https://dropload.com/embed-abc123",
                "https://dropload.com/abc123",
            ),
        ];
        for (input, expected) in forms {
            let url = Url::parse(input).unwrap_or_else(|e| panic!("valid URL: {e}"));
            assert_eq!(extractor.normalize(&url).as_str(), expected);
        }
    }

    #[tokio::test]
    async fn unpacks_the_playlist_with_page_height_and_size() {
        let fetcher = ScriptedFetcher::default().page("/abc123", PACKED_PAGE);
        let ctx = ctx_for(&fetcher, None);

        let streams = Dropload::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the packed page must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.dropload.example/hls/master.m3u8",
        );
        let stream = &streams[0];
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://dr0pstream.com/")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        // `bytes.parse("1.4 GB")` = floor(1.4 × 1024³).
        assert_eq!(stream.meta.size, Some(1_503_238_553));
    }

    #[tokio::test]
    async fn guesses_height_from_the_playlist_when_the_page_has_none() {
        let fetcher = ScriptedFetcher::default()
            .page(
                "/abc123",
                PACKED_PAGE.replace("1920x1080, 1.4 GB", "watch online"),
            )
            .page(
                "/hls/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\n720p/index.m3u8\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = Dropload::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the height fallback must resolve: {e}"));
        assert_eq!(streams[0].meta.resolution, Some(720));

        // The probe carried the playlist Referer.
        assert_eq!(
            fetcher
                .sent_header("/hls/master.m3u8", "Referer")
                .as_deref(),
            Some("https://dr0pstream.com/")
        );
    }

    #[tokio::test]
    async fn missing_files_are_misses() {
        for marker in ["File Not Found", "Pending in queue"] {
            let fetcher = ScriptedFetcher::default().page("/abc123", format!("<p>{marker}</p>"));
            let ctx = ctx_for(&fetcher, None);

            match Dropload::new().extract(&ctx, &url()).await {
                Err(ExtractorError::NotFound) => {}
                other => panic!("a {marker} page must be a NotFound, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn pages_without_a_packed_link_are_scrape_failures() {
        let fetcher = ScriptedFetcher::default().page("/abc123", "<p>nothing packed here</p>");
        let ctx = ctx_for(&fetcher, None);

        match Dropload::new().extract(&ctx, &url()).await {
            Err(ExtractorError::Extraction { .. }) => {}
            other => panic!("an unpackable page must be a scrape failure, got {other:?}"),
        }
    }
}
