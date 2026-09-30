//! `VidHawk`: anime HLS with sub/dub audio tracks from `vidhawk.buzz`.
//!
//! Ports `src/extractor/VidHawk.js` — the fallback for raw vidhawk embed
//! URLs (the Itachi source, which emits them, normally resolves its
//! embeds in-source with richer metadata):
//!
//! 1. `GET /api/stream/resolve?anilistId={id}&episode={ep}&variant={sub|dub}&skipMapper=1&parentHost=itachi.tv`
//!    → `{ ticket }` (the default server is used).
//! 2. `GET /api/play?t={ticket}` → `{ tracks: [{id, src}], captions }`.
//! 3. One HLS stream per audio track: `sub` → `multi`/`ja`, `dub` →
//!    `multi`/`en`, each with the matching VTT caption set attached.
//!
//! `edge.vidhawk.buzz` URLs are public-CDN passthroughs: direct `.m3u8`
//! links resolve to themselves, `.vtt` links are subtitle files — not
//! streams — and resolve to nothing. The edge works without a `Referer`,
//! so the streams carry no request headers, exactly like the upstream
//! results.
//!
//! Cut from the upstream: nothing — the whole REST flow ports directly;
//! the explicit Chrome `User-Agent` comes from the fetcher layer instead
//! of each request.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, FetchRequest, ResolveCtx};
use vsources_core::types::{CountryCode, Format, Stream, StreamMeta, SubtitleTrack};

const VIDHAWK_BASE: &str = "https://vidhawk.buzz";

static EMBED_PATH: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"(?i)/embed/ani/(\d+)/(\d+)/(sub|dub)")
        .unwrap_or_else(|e| panic!("valid embed path pattern: {e}"))
});

/// Upstream result lifetime: 30min (tickets may expire).
const TTL: Duration = Duration::from_mins(30);

/// The `VidHawk` extractor.
#[derive(Debug, Default)]
pub struct VidHawk;

impl VidHawk {
    /// A new extractor; stateless — fetches travel through the context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for VidHawk {
    fn id(&self) -> &'static str {
        "vidhawk"
    }

    fn label(&self) -> &'static str {
        "VidHawk"
    }

    fn supports(&self, _ctx: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| host == "vidhawk.buzz" || host == "edge.vidhawk.buzz")
    }

    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        // Direct CDN URLs on the edge host pass through untouched.
        if url.host_str() == Some("edge.vidhawk.buzz") {
            // `isHls = pathname.endsWith('.m3u8') || includes('.m3u8')`.
            if url.path().contains(".m3u8") {
                return Ok(vec![
                    Stream::new(url.clone(), Format::Hls)
                        .with_label(self.label())
                        .with_ttl(TTL),
                ]);
            }
            // A subtitle file is not a stream (upstream returns []).
            if url.path().contains(".vtt") {
                return Err(ExtractorError::NotFound);
            }
        }

        // The embed page: `/embed/ani/{anilistId}/{episode}/{sub|dub}`.
        let captures = EMBED_PATH
            .captures(url.path())
            .map_err(|e| ExtractorError::extraction(self.id(), e.to_string()))?;
        let Some(captures) = captures else {
            return Err(ExtractorError::NotFound);
        };
        let anilist_id = captures
            .get(1)
            .and_then(|group| group.as_str().parse::<u64>().ok())
            .ok_or_else(|| ExtractorError::extraction(self.id(), "invalid anilist id"))?;
        let episode = captures
            .get(2)
            .and_then(|group| group.as_str().parse::<u64>().ok())
            .ok_or_else(|| ExtractorError::extraction(self.id(), "invalid episode number"))?;
        // `(m[3] || 'sub').toLowerCase()`.
        let variant = captures.get(3).map_or_else(
            || "sub".to_string(),
            |group| group.as_str().to_ascii_lowercase(),
        );

        // Steps 1–2: the ticket, then the play data. Any API failure is
        // the upstream `gotJson → null` miss.
        let streams = match self
            .resolve_streams(ctx, anilist_id, episode, &variant)
            .await
        {
            Some(streams) if !streams.is_empty() => streams,
            _ => return Err(ExtractorError::NotFound),
        };
        Ok(streams)
    }
}

impl VidHawk {
    /// Steps 1–2: ticket → play data → one stream per audio track.
    async fn resolve_streams(
        &self,
        ctx: &ResolveCtx<'_>,
        anilist_id: u64,
        episode: u64,
        variant: &str,
    ) -> Option<Vec<Stream>> {
        let resolve_url = Url::parse(&format!(
            "{VIDHAWK_BASE}/api/stream/resolve?anilistId={anilist_id}&episode={episode}&variant={variant}&skipMapper=1&parentHost=itachi.tv"
        ))
        .unwrap_or_else(|e| panic!("the resolve URL must parse: {e}"));
        let resolve_request =
            FetchRequest::get(resolve_url).with_header("Referer", format!("{VIDHAWK_BASE}/"));
        let resolved: Value = ctx
            .fetcher
            .request(resolve_request)
            .await
            .ok()?
            .json()
            .ok()?;
        // `if (!resolveData?.ticket) return []`.
        let ticket = str_field(&resolved, "ticket").filter(|ticket| !ticket.is_empty())?;

        let play_url = Url::parse(&format!("{VIDHAWK_BASE}/api/play?t={}", urlencode(ticket)))
            .unwrap_or_else(|e| panic!("the play URL must parse: {e}"));
        let play_request =
            FetchRequest::get(play_url).with_header("Referer", format!("{VIDHAWK_BASE}/"));
        let play: Value = ctx.fetcher.request(play_request).await.ok()?.json().ok()?;
        let tracks = play.get("tracks").and_then(Value::as_array)?;

        let streams = tracks
            .iter()
            .filter_map(|track| self.track_stream(&play, track))
            .collect();
        Some(streams)
    }

    /// Step 3: one audio track → one HLS stream with its caption set.
    fn track_stream(&self, play: &Value, track: &Value) -> Option<Stream> {
        // `if (!track?.src) continue` and the `new URL` try/catch.
        let src = str_field(track, "src")?;
        let url = Url::parse(src).ok()?;

        // `track.id === 'dub'` picks the dub labels and language set.
        let track_id = str_field(track, "id");
        let is_dub = track_id == Some("dub");
        // `playData.captions?.[track.id] || []`.
        let captions = track_id
            .and_then(|id| play.get("captions").and_then(|captions| captions.get(id)))
            .and_then(Value::as_array);
        let subtitles = captions
            .map(|captions| captions.iter().filter_map(caption_track).collect())
            .unwrap_or_default();

        let meta = StreamMeta {
            languages: if is_dub {
                vec![CountryCode::Multi, CountryCode::En]
            } else {
                vec![CountryCode::Multi, CountryCode::Ja]
            },
            // `audioLabel: track.id === 'dub' ? 'DUB' : 'SUB'`.
            audio: vec![if is_dub { "DUB" } else { "SUB" }.to_string()],
            subtitles,
            ..StreamMeta::default()
        };
        let mut stream = Stream::new(url, Format::Hls)
            .with_label(self.label())
            .with_ttl(TTL);
        stream.meta = meta;
        Some(stream)
    }
}

/// One caption entry → a subtitle track.
///
/// Ports the caption mapping (`lang: c.lang || 'en'`,
/// `label: c.label || 'English'`) and the `new URL` validity filter; the
/// upstream `id` is cut — [`SubtitleTrack`] has no id field.
fn caption_track(caption: &Value) -> Option<SubtitleTrack> {
    let url = Url::parse(str_field(caption, "src")?).ok()?;
    let language = str_field(caption, "lang")
        .filter(|lang| !lang.is_empty())
        .unwrap_or("en");
    let label = str_field(caption, "label")
        .filter(|label| !label.is_empty())
        .unwrap_or("English");
    Some(SubtitleTrack {
        label: Some(label.to_string()),
        language: Some(language.to_string()),
        url,
    })
}

/// `value?.key` as a borrowed string.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `encodeURIComponent` for the ticket query parameter.
fn urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use crate::testing::{ScriptedFetcher, assert_direct_stream, ctx_for};

    use super::*;

    fn url() -> Url {
        Url::parse("https://vidhawk.buzz/embed/ani/20/1/sub")
            .unwrap_or_else(|e| panic!("valid URL: {e}"))
    }

    /// The `/api/stream/resolve` response (the server/defaultAudio
    /// fields the port does not read are kept for shape fidelity).
    const RESOLVE_JSON: &str =
        r#"{"ticket":"TCK-abc123","server":"kari","defaultAudio":"sub","servers":[{"id":"kari"}]}"#;

    /// The `/api/play` response: sub and dub tracks (plus one with an
    /// unparseable src), caption sets with one invalid entry.
    const PLAY_JSON: &str = r#"{
        "tracks": [
            {"id": "sub", "src": "https://edge.vidhawk.buzz/hls/sub.m3u8?t=xyz"},
            {"id": "dub", "src": "https://edge.vidhawk.buzz/hls/dub.m3u8?t=uvw"},
            {"id": "broken", "src": "not-a-url"}
        ],
        "captions": {
            "sub": [
                {"lang": "en", "label": "English", "src": "https://edge.vidhawk.buzz/sub.vtt?t=xyz"},
                {"lang": "fr", "label": "French", "src": "not-a-caption-url"}
            ],
            "dub": []
        },
        "intro": {"start": 0, "end": 100},
        "outro": {"start": 2300, "end": 2400}
    }"#;

    #[test]
    fn matches_the_vidhawk_hosts() {
        let extractor = VidHawk::new();
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let supports = |host: &str| {
            let url = Url::parse(&format!("https://{host}/embed/ani/20/1/sub"))
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
            extractor.supports(&ctx, &url)
        };
        assert!(supports("vidhawk.buzz"));
        assert!(supports("edge.vidhawk.buzz"));
        assert!(!supports("vidhawk.com"));
        assert!(!supports("example.com"));
    }

    #[tokio::test]
    async fn passes_edge_m3u8_urls_through() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://edge.vidhawk.buzz/hls/sub.m3u8?t=xyz")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        let streams = VidHawk::new()
            .extract(&ctx, &url)
            .await
            .unwrap_or_else(|e| panic!("the edge URL must pass through: {e}"));
        assert_direct_stream(
            &streams,
            Format::Hls,
            "https://edge.vidhawk.buzz/hls/sub.m3u8",
        );
        assert_eq!(streams[0].ttl, TTL);
        // Public CDN — no hotlink headers, no upstream fetches.
        assert!(streams[0].meta.request_headers.is_empty());
        assert!(fetcher.requests().is_empty());
    }

    #[tokio::test]
    async fn edge_vtt_urls_are_not_streams() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://edge.vidhawk.buzz/sub.vtt?t=xyz")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match VidHawk::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a subtitle URL must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolves_one_stream_per_audio_track() {
        let fetcher = ScriptedFetcher::default()
            .page("/api/stream/resolve", RESOLVE_JSON)
            .page("/api/play", PLAY_JSON);
        let ctx = ctx_for(&fetcher, None);

        let streams = VidHawk::new()
            .extract(&ctx, &url())
            .await
            .unwrap_or_else(|e| panic!("the embed must resolve: {e}"));
        // The broken track is dropped; sub and dub survive.
        assert_eq!(streams.len(), 2);

        let sub = streams
            .iter()
            .find(|stream| stream.url.as_str().contains("sub.m3u8"))
            .unwrap_or_else(|| panic!("the sub track must resolve"));
        assert_eq!(sub.ttl, TTL);
        assert_eq!(sub.label.as_deref(), Some("VidHawk"));
        assert_eq!(
            sub.meta.languages,
            vec![CountryCode::Multi, CountryCode::Ja]
        );
        assert_eq!(sub.meta.audio, vec!["SUB".to_string()]);
        assert_eq!(sub.meta.subtitles.len(), 1);
        assert_eq!(sub.meta.subtitles[0].label.as_deref(), Some("English"));
        assert_eq!(sub.meta.subtitles[0].language.as_deref(), Some("en"));
        assert_eq!(
            sub.meta.subtitles[0].url.as_str(),
            "https://edge.vidhawk.buzz/sub.vtt?t=xyz"
        );
        // The edge is public — no request headers.
        assert!(sub.meta.request_headers.is_empty());

        let dub = streams
            .iter()
            .find(|stream| stream.url.as_str().contains("dub.m3u8"))
            .unwrap_or_else(|| panic!("the dub track must resolve"));
        assert_eq!(
            dub.meta.languages,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(dub.meta.audio, vec!["DUB".to_string()]);
        // The dub caption set is empty — no subtitles attached.
        assert!(dub.meta.subtitles.is_empty());

        // The wire shape: both API calls under the site referer.
        let requests = fetcher.requests();
        let resolve = requests
            .iter()
            .find(|request| request.url.path() == "/api/stream/resolve")
            .unwrap_or_else(|| panic!("the resolve API must have been called"));
        assert_eq!(
            resolve.url.query().unwrap_or_default(),
            "anilistId=20&episode=1&variant=sub&skipMapper=1&parentHost=itachi.tv"
        );
        assert_eq!(
            resolve.headers.get("Referer").map(String::as_str),
            Some("https://vidhawk.buzz/")
        );
        let play = requests
            .iter()
            .find(|request| request.url.path() == "/api/play")
            .unwrap_or_else(|| panic!("the play API must have been called"));
        assert_eq!(play.url.query().unwrap_or_default(), "t=TCK-abc123");
        assert_eq!(
            play.headers.get("Referer").map(String::as_str),
            Some("https://vidhawk.buzz/")
        );
    }

    #[tokio::test]
    async fn embeds_without_the_ani_path_are_misses() {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        let url = Url::parse("https://vidhawk.buzz/embed/other/20/1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        match VidHawk::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("an unknown embed path must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolves_without_a_ticket_are_misses() {
        let fetcher = ScriptedFetcher::default()
            .page("/api/stream/resolve", r#"{"server":"kari","servers":[]}"#);
        let ctx = ctx_for(&fetcher, None);

        match VidHawk::new().extract(&ctx, &url()).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a ticketless resolve must be a NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sends_the_dub_variant_to_the_resolve_api() {
        let fetcher =
            ScriptedFetcher::default().page("/api/stream/resolve", r#"{"ticket":"TCK-dub"}"#);
        let ctx = ctx_for(&fetcher, None);
        // The case-insensitive path match lowercases the variant.
        let url = Url::parse("https://vidhawk.buzz/embed/ani/20/1/DUB")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));

        // No `/api/play` fixture: the play fetch fails → miss.
        match VidHawk::new().extract(&ctx, &url).await {
            Err(ExtractorError::NotFound) => {}
            other => panic!("a failed play fetch must be a NotFound, got {other:?}"),
        }
        let query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/api/stream/resolve")
            .map(|request| request.url.query().unwrap_or_default().to_string());
        assert_eq!(
            query.as_deref(),
            Some("anilistId=20&episode=1&variant=dub&skipMapper=1&parentHost=itachi.tv")
        );
    }
}
