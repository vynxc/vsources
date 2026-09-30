//! `Megaplay`: anime HLS from `megaplay.buzz` / `vidtube.site` embeds.
//!
//! Ports `src/extractor/Megaplay.js` with the AES decrypt of
//! `src/nuvio/megaplay_decrypt.cjs` folded in as a private helper. The
//! upstream sources that emit these embeds: `Anikoto` (`/stream/s-…`),
//! `StreamXTV` and `AniDoor` (`/stream/ani/…`), `2Dhive` (`/stream/mal/…`),
//! `AllWish` (`/stream/s-1/…`).
//!
//! 1. Fetch the embed page — the upstream anime site as `Referer` — and
//!    extract the player's `data-id`.
//! 2. `GET /stream/getSourcesNew?id={data-id}` with
//!    `X-Requested-With: XMLHttpRequest`. The 2026-09 API returns an
//!    `enc` blob instead of a plaintext `sources.file`; it decrypts
//!    (AES-256-CBC over urlsafe-base64, key/IV hardcoded in the site's
//!    player bundle) to `{ file: "<master.m3u8>" }`.
//! 3. The m3u8 CDN hard-403s datacenter IPs, so the URL ships direct with
//!    its hotlink `Referer` in [`StreamMeta::request_headers`] plus the
//!    API's multi-language subtitle tracks.
//! 4. Fallback: direct m3u8/mp4 URLs embedded in the page HTML.
//!
//! Cut from the upstream: the Node-only global `fetch` shim
//! (`installMegaplayShim`, which rewrote API responses back into the
//! legacy shape for other consumers — this port decrypts `enc` directly)
//! and the explicit Chrome `User-Agent` (the fetcher layer already
//! impersonates Chrome).

use std::sync::LazyLock;
use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{Format, Stream, StreamMeta, SubtitleTrack};

use crate::helpers::{fetch_page_with, first_capture};

/// Hosts that run the megaplay.buzz player backend.
const MEGAPLAY_HOSTS: &[&str] = &["megaplay.buzz", "megaplay-1.buzz", "vidtube.site"];

/// Hostname suffixes (subdomains) of the same backend.
const MEGAPLAY_SUFFIXES: &[&str] = &[".megaplay.buzz", ".vidtube.site"];

static DATA_ID: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"data-id="(\d+)""#)
        .unwrap_or_else(|e| panic!("valid data-id pattern: {e}"))
});
static DIRECT_M3U8: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^"'\s<>]+\.m3u8[^"'\s<>]*)"#)
        .unwrap_or_else(|e| panic!("valid m3u8 pattern: {e}"))
});
static DIRECT_MP4: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r#"(?i)(https?://[^"'\s<>]+\.mp4[^"'\s<>]*)"#)
        .unwrap_or_else(|e| panic!("valid mp4 pattern: {e}"))
});
static RESOLUTION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)RESOLUTION=\d+x(\d+)")
        .unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// Upstream result lifetime: 30min (m3u8 tokens may expire).
const TTL: Duration = Duration::from_mins(30);

/// `trustAesKey` from megaplay.buzz's player bundle: 16 chars,
/// zero-padded to a 32-byte AES-256 key.
const AES_KEY_RAW: &str = "i?LMTAx0Q6,:}50U";

/// `trustAesIv` from the same bundle, used as-is.
const AES_IV_RAW: &str = "W0;27ToaUpl_P%'c";

/// The `Megaplay` family extractor.
#[derive(Debug, Default)]
pub struct Megaplay;

impl Megaplay {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for Megaplay {
    fn id(&self) -> &'static str {
        "megaplay"
    }

    fn label(&self) -> &'static str {
        "Megaplay"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            MEGAPLAY_HOSTS.contains(&host)
                || MEGAPLAY_SUFFIXES
                    .iter()
                    .any(|suffix| host.ends_with(suffix))
        })
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let referer = infer_upstream_referer(url);

        // Step 1: the embed page. A "Not Found" body is a dead link — a
        // hard miss. The fetcher surfaces non-2xx pages as errors, which
        // fall through to the re-fetch below (upstream checks the status
        // explicitly; the outcome is the same miss).
        let page = fetch_page_with(ctx, url, &referer).await;
        if let Ok(html) = page.as_deref()
            && (html.contains("Page not found") || html.contains("Not Found"))
        {
            return Err(ExtractorError::NotFound);
        }
        if let Ok(html) = page.as_deref()
            && let Ok(Some(data_id)) = first_capture(&DATA_ID, html)
            && let Some(stream) = self.extract_via_api(ctx, url, &data_id).await
        {
            return Ok(vec![stream]);
        }

        // Step 3: direct m3u8/mp4 URLs in the page HTML (upstream
        // re-fetches it, which also retries a failed first fetch).
        if let Ok(html) = fetch_page_with(ctx, url, &referer).await
            && let Some(stream) = self.direct_from_page(&html)
        {
            return Ok(vec![stream]);
        }

        Err(ExtractorError::NotFound)
    }
}

impl Megaplay {
    /// Step 2: `getSourcesNew` — the decrypted m3u8 with its tracks.
    ///
    /// Any failure maps to the upstream `gotJson → null` fallthrough, so
    /// the page fallback still runs.
    async fn extract_via_api(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
        data_id: &str,
    ) -> Option<Stream> {
        let api_url = Url::parse(&format!(
            "https://megaplay.buzz/stream/getSourcesNew?id={data_id}"
        ))
        .unwrap_or_else(|e| panic!("the getSourcesNew URL must parse: {e}"));
        let request = FetchRequest::get(api_url)
            .with_header("X-Requested-With", "XMLHttpRequest")
            .with_header("Referer", url.as_str())
            .with_header("Accept", "application/json,text/plain,*/*");
        let data: Value = ctx.fetcher.request(request).await.ok()?.json().ok()?;

        let file = sources_file(&data)?;
        let m3u8 = Url::parse(&file).ok()?;

        let subtitles = subtitle_tracks(&data);
        let has_subtitles = !subtitles.is_empty();

        // Best-effort resolution detection: the playlist advertises
        // `RESOLUTION=WxH` (server-side fetches usually 403).
        let playlist =
            FetchRequest::get(m3u8.clone()).with_header("Referer", "https://megaplay.buzz/");
        let height = ctx
            .fetcher
            .request(playlist)
            .await
            .ok()
            .and_then(|response| first_capture(&RESOLUTION, &response.body).ok().flatten())
            .and_then(|height| height.parse::<u16>().ok());

        let meta = StreamMeta {
            resolution: height,
            subtitles,
            ..StreamMeta::default()
        };
        // Upstream attaches the hotlink Referer only when the API
        // returned subtitle tracks — a verbatim port of the conditional
        // spread over `requestHeaders`.
        let meta = if has_subtitles {
            meta.with_header("Referer", "https://megaplay.buzz/")
        } else {
            meta
        };

        let mut stream = Stream::new(m3u8, Format::Hls)
            .with_label(self.label())
            .with_ttl(TTL);
        stream.meta = meta;
        Some(stream)
    }

    /// Step 3: a direct m3u8 (preferred) or mp4 URL in the page HTML.
    fn direct_from_page(&self, html: &str) -> Option<Stream> {
        let direct = first_capture(&DIRECT_M3U8, html)
            .ok()
            .flatten()
            .or_else(|| first_capture(&DIRECT_MP4, html).ok().flatten())?;
        let url = Url::parse(&direct).ok()?;
        // `parsed.href.includes('.m3u8') ? hls : mp4`.
        let format = if url.as_str().contains(".m3u8") {
            Format::Hls
        } else {
            Format::Mp4
        };
        Some(
            Stream::new(url, format)
                .with_label(self.label())
                .with_ttl(TTL)
                // The fallback always carries the megaplay referer.
                .with_referer("https://megaplay.buzz/"),
        )
    }
}

/// The upstream anime site a megaplay path was embedded from — the
/// `Referer` for the embed page.
///
/// Ports `inferUpstreamReferer`: `/stream/ani/` → `anidoor.me`,
/// `/stream/mal/` → `2dhive.com`, `/stream/s-1/` → `all-wish.me`,
/// anything else (`s-2`/`s-5`, …) → `anikoto.cz`.
fn infer_upstream_referer(url: &Url) -> Url {
    let referer = if url.path().contains("/stream/ani/") {
        "https://anidoor.me/"
    } else if url.path().contains("/stream/mal/") {
        "https://2dhive.com/"
    } else if url.path().contains("/stream/s-1/") {
        "https://all-wish.me/"
    } else {
        "https://anikoto.cz/"
    };
    Url::parse(referer).unwrap_or_else(|e| panic!("the upstream referer must parse: {e}"))
}

/// `data.sources.file`, decrypting `enc` when only the encrypted blob is
/// present — ports the shim's in-place restoration of the legacy shape.
fn sources_file(data: &Value) -> Option<String> {
    match data.get("sources").filter(|sources| !sources.is_null()) {
        Some(sources) => str_field(sources, "file").map(str::to_string),
        None => str_field(data, "enc").and_then(decrypt_enc_file),
    }
}

/// Decrypt the `enc` blob and return its `file` member — ports
/// `decryptMegaplayEnc` from `src/nuvio/megaplay_decrypt.cjs`: AES-256-CBC
/// over urlsafe-base64 with the player bundle's key/IV. Any failure is
/// the upstream's caught-exception `null`.
fn decrypt_enc_file(enc: &str) -> Option<String> {
    let ciphertext = b64url_decode(enc)?;
    let mut key = [0u8; 32];
    key[..AES_KEY_RAW.len()].copy_from_slice(AES_KEY_RAW.as_bytes());
    let iv: [u8; 16] = AES_IV_RAW.as_bytes().try_into().ok()?;
    let decryptor = cbc::Decryptor::<aes::Aes256>::new(
        GenericArray::from_slice(&key),
        GenericArray::from_slice(&iv),
    );
    let mut buffer = ciphertext;
    let plaintext = decryptor.decrypt_padded_mut::<Pkcs7>(&mut buffer).ok()?;
    str_field(&serde_json::from_slice::<Value>(plaintext).ok()?, "file").map(str::to_string)
}

/// Ports `b64urlDecode`: urlsafe→standard alphabet, padding stripped,
/// decoded leniently like `Buffer.from(…, 'base64')`.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let standard = s.replace('-', "+").replace('_', "/");
    let unpadded = standard.trim_end_matches('=');
    STANDARD_NO_PAD.decode(unpadded).ok()
}

/// The API's subtitle tracks, filtered to `captions`/`subtitles` kinds.
fn subtitle_tracks(data: &Value) -> Vec<SubtitleTrack> {
    data.get("tracks")
        .and_then(Value::as_array)
        .map(|tracks| tracks.iter().filter_map(subtitle_track).collect())
        .unwrap_or_default()
}

/// One `tracks` entry → a subtitle track.
///
/// The upstream `id` (label sliced to 8 chars) is cut —
/// [`SubtitleTrack`] has no id field.
fn subtitle_track(track: &Value) -> Option<SubtitleTrack> {
    // `t.file && (t.kind === 'captions' || t.kind === 'subtitles')`.
    if !matches!(str_field(track, "kind"), Some("captions" | "subtitles")) {
        return None;
    }
    let url = Url::parse(str_field(track, "file")?).ok()?;
    // `t.label || 'en'`.
    let language = str_field(track, "label")
        .filter(|label| !label.is_empty())
        .unwrap_or("en");
    Some(SubtitleTrack {
        label: str_field(track, "label").map(str::to_string),
        language: Some(language.to_string()),
        url,
    })
}

/// `value?.key` as a borrowed string.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://megaplay.buzz/stream/ani/20/1/sub")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// A known plaintext/ciphertext pair for the `enc` blob, generated
    /// with `node:crypto` during the port: AES-256-CBC of
    /// `{"file":"https://fetch.nexabloom.top/hls/gGg7J/master.m3u8"}`
    /// under the player-bundle key/IV.
    const ENC_VECTOR: &str =
        "wdeBruh3qqn_i5wUNnyaPcXqidp1UWP84FfPHzGyKXAW0Ph6EguKX6YVOtp-Rzfz-2LTUDqnyQCM2VNkMoAV3g";
    const ENC_FILE: &str = "https://fetch.nexabloom.top/hls/gGg7J/master.m3u8";

    #[test]
    fn decrypts_the_enc_blob() {
        assert_eq!(decrypt_enc_file(ENC_VECTOR).as_deref(), Some(ENC_FILE));
        // Malformed blobs fail closed (upstream returns null).
        assert!(decrypt_enc_file("!!!not-base64").is_none());
        assert!(decrypt_enc_file("").is_none());
    }

    #[test]
    fn matches_the_megaplay_family() {
        let extractor = Megaplay::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/stream/ani/1/1/sub"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("megaplay.buzz"));
        assert!(supports("megaplay-1.buzz"));
        assert!(supports("vidtube.site"));
        assert!(supports("en.megaplay.buzz"));
        assert!(supports("alt.vidtube.site"));
        assert!(!supports("example.com"));
        assert!(!supports("xmegaplay.buzz"));
    }

    #[tokio::test]
    async fn resolves_via_getsourcesnew() {
        // The 2026-09 response shape: `enc` instead of plaintext
        // `sources.file`, plaintext multi-language tracks, one track of
        // an ignored `kind`.
        let api_body = format!(
            r#"{{"tracks":[{{"file":"https://megaplay.buzz/subs/en.vtt","label":"English","kind":"captions"}},{{"file":"https://megaplay.buzz/subs/ja.vtt","label":"Japanese","kind":"captions"}},{{"file":"https://megaplay.buzz/subs/ignored.vtt","label":"Meta","kind":"metadata"}}],"t":3600,"intro":{{}},"outro":{{}},"server":"kari","enc":"{ENC_VECTOR}"}}"#
        );
        let fetcher = ScriptedFetcher::default()
            .page(
                "/stream/ani/20/1/sub",
                r#"<div id="megaplay-player" data-id="12345"></div>"#,
            )
            .page("/stream/getSourcesNew", api_body)
            .page(
                "/hls/gGg7J/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n1080.m3u8\n",
            );
        let ctx = ctx_for(&fetcher, None);

        let streams = Megaplay::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the getSourcesNew path must resolve: {e}"));
        assert_direct_stream(&streams, Format::Hls, ENC_FILE);
        let stream = &streams[0];
        assert_eq!(stream.ttl, TTL);
        assert_eq!(stream.label.as_deref(), Some("Megaplay"));
        assert_eq!(stream.meta.resolution, Some(1080));
        // The hotlink Referer rides along with the subtitle tracks.
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        assert_eq!(stream.meta.subtitles.len(), 2);
        assert_eq!(stream.meta.subtitles[0].label.as_deref(), Some("English"));
        assert_eq!(
            stream.meta.subtitles[0].language.as_deref(),
            Some("English")
        );
        assert_eq!(
            stream.meta.subtitles[0].url.as_str(),
            "https://megaplay.buzz/subs/en.vtt"
        );
        assert_eq!(stream.meta.subtitles[1].label.as_deref(), Some("Japanese"));

        // The wire shape: the anime site referer, then the API headers.
        assert_eq!(
            fetcher
                .sent_header("/stream/ani/20/1/sub", "Referer")
                .as_deref(),
            Some("https://anidoor.me/")
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/getSourcesNew", "X-Requested-With")
                .as_deref(),
            Some("XMLHttpRequest")
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/getSourcesNew", "Referer")
                .as_deref(),
            Some("https://megaplay.buzz/stream/ani/20/1/sub")
        );
        assert_eq!(
            fetcher
                .sent_header("/stream/getSourcesNew", "Accept")
                .as_deref(),
            Some("application/json,text/plain,*/*")
        );
        assert_eq!(
            fetcher
                .sent_header("/hls/gGg7J/master.m3u8", "Referer")
                .as_deref(),
            Some("https://megaplay.buzz/")
        );
    }

    #[tokio::test]
    async fn page_not_found_is_a_miss() {
        let fetcher =
            ScriptedFetcher::default().page("/stream/ani/20/1/sub", "<p>Page not found</p>");
        let ctx = ctx_for(&fetcher, None);

        match Megaplay::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a dead link must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn falls_back_to_a_direct_url() {
        let fetcher = ScriptedFetcher::default().page(
            "/stream/s-1/tok",
            r#"<script>var x = "https://cdn.example.com/direct/file.m3u8?token=1";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://megaplay.buzz/stream/s-1/tok")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = Megaplay::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the direct fallback must resolve: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://cdn.example.com/direct/file.m3u8",
        );
        // The fallback always attaches the megaplay referer.
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://megaplay.buzz/")
        );
        // `/stream/s-1/` embeds come from all-wish.me.
        assert_eq!(
            fetcher.sent_header("/stream/s-1/tok", "Referer").as_deref(),
            Some("https://all-wish.me/")
        );
    }

    #[tokio::test]
    async fn api_failures_fall_back_to_the_page() {
        // `/stream/getSourcesNew` is not scripted — the API fetch fails
        // like upstream's `gotJson → null` fallthrough.
        let fetcher = ScriptedFetcher::default().page(
            "/stream/ani/20/1/sub",
            r#"<div id="megaplay-player" data-id="12345"></div><script>var x = "https://cdn.example.com/file.mp4";</script>"#,
        );
        let ctx = ctx_for(&fetcher, None);

        let streams = Megaplay::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the page fallback must resolve: {e}"));
        assert_direct_stream(&streams, Format::Mp4, "https://cdn.example.com/file.mp4");
    }

    #[tokio::test]
    async fn unresolvable_pages_are_misses() {
        let fetcher =
            ScriptedFetcher::default().page("/stream/ani/20/1/sub", "<p>nothing here</p>");
        let ctx = ctx_for(&fetcher, None);

        match Megaplay::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an empty page must be a NotFound, got {other:?}"),
        }
    }
}
