//! Shared Nuvio plumbing: the port of `src/source/nuvioHelpers.js`.
//!
//! Upstream's Nuvio-backed anime providers delegate to `CommonJS` scrapers
//! (the `src/nuvio/*.cjs` family) that answer raw stream objects of the
//! shape `{ url, quality, title, name, size, headers, subtitles }`. The
//! JS wrappers then run those objects through `buildStreamResults` to
//! turn them into source results. This module ports that shared layer
//! so the Rust Nuvio providers can stay thin:
//!
//! - [`NuvioStream`] — the raw scraper stream shape, deserializable from
//!   the JSON the JS scrapers produce (fixture tests feed it directly).
//! - [`build_stream_results`] — raw streams → [`Stream`]s, including the
//!   language/height/size parsing, audio-track normalization, and the
//!   hotlink-header policy.
//! - [`with_retry_on_empty`] / [`with_deadline`] — the bounded
//!   retry-on-empty sweep and per-request deadline wrappers.
//!
//! Cuts versus the JS (shared by every consumer here):
//!
//! - No `/proxy` routing: upstream attached `nuvioReferer` /
//!   `nuvioUserAgent` / `nuvioOrigin` / `nuvioForceHls` meta flags for
//!   the server-side `NuvioExtractor` to route through its media proxy.
//!   There is no server in this SDK, so the flags map onto
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!   (the same headers the proxy sent upstream), and the ambiguous-URL
//!   force-HLS hint rides [`Stream::behavior_hints`] as `nuvioForceHls`.
//! - `meta.title` has no `StreamMeta` field — the rich
//!   `title — streamTitle — filename` display string becomes
//!   [`Stream::label`].
//! - `withRetryOnEmpty`'s console logs are dropped; the timing uses
//!   [`std::time::Instant`] instead of `Date.now()`.

pub mod decrypt;
pub mod flixcloud;
pub mod megaplay;
pub mod speedracelight;
pub mod vidstorm;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::language::find_country_codes;
use vsources_core::types::{CountryCode, Format, Stream, StreamMeta, SubtitleTrack};

/// Hosts that must NOT receive a `Referer` — inverted hotlink gates and
/// datacenter-hostile CDNs (ports `NO_REFERER_HOSTS`). pixeldrain and
/// googleusercontent are direct-play CDNs; `vimeos.zip/net` 403 any
/// Referer-bearing request; `vyrnex.top` can reject
/// datacenter IPs, so the player's residential IP must fetch them.
static NO_REFERER_HOSTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)pixeldrain\.(com|dev)|fastdlserver\.site|googleusercontent\.com|vimeos\.(zip|net)|vyrnex\.top")
        .unwrap_or_else(|e| panic!("valid no-referer host pattern: {e}"))
});

/// `([\d.]+)\s*(GB|MB|TB)` — the `size` field of raw scraper streams.
static SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([\d.]+)\s*(GB|MB|TB)").unwrap_or_else(|e| panic!("valid size pattern: {e}"))
});

/// `(\d{3,4})p` — the quality form `parseHeight` accepts.
static QUALITY_P: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})p").unwrap_or_else(|e| panic!("valid quality pattern: {e}"))
});

/// A last path segment that is 20+ chars with no dot — a hash, not a
/// filename (ports the `extractFilename` hash skip).
static HASH_SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-zA-Z0-9_-]{20,}$").unwrap_or_else(|e| panic!("valid hash pattern: {e}"))
});

/// A `pub-` prefixed R2 bucket id (ports the display-name cleaner).
static R2_BUCKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^pub-[a-f0-9]{20,}").unwrap_or_else(|e| panic!("valid r2 pattern: {e}"))
});

/// Technical playlist indices like `index-s2160p-v1-a1.m3u8`.
static PLAYLIST_INDEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^index-s\d+p-").unwrap_or_else(|e| panic!("valid index pattern: {e}"))
});

/// Generic playlist/stream names (`master.m3u8`, `playlist.mp4`, …).
static GENERIC_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(master|playlist|index|hls|stream|video|play|watch|embed|api|proxy|content|uc|file|download)\.(m3u8|mp4|mkv|ts)$")
        .unwrap_or_else(|e| panic!("valid generic-name pattern: {e}"))
});

/// Bare route segments (`hls`, `p`, `v`, `bulk`, atlantic's
/// `m3u8-proxy`) that are not filenames.
static BARE_SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(hls|playlist|stream|video|play|watch|embed|api|proxy|content|uc|file|download|p|v|e|bulk|m3u8-proxy|cdn-m3u8)$")
        .unwrap_or_else(|e| panic!("valid bare-segment pattern: {e}"))
});

/// Server-script endpoints (`download.aspx`, `api.php`).
static SCRIPT_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.aspx$|\.php$").unwrap_or_else(|e| panic!("valid script pattern: {e}"))
});

/// A hash-ish base name of 30+ chars (`VidEasy`'s moon CDN filenames).
static LONG_BASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap_or_else(|e| panic!("valid base pattern: {e}"))
});

/// One raw subtitle entry in a scraper stream — the union of the field
/// names the Nuvio scrapers emit (`{id, url, lang}`, `{file, label}`,
/// `{url, lang, name}`, …).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct NuvioSubtitle {
    /// Stremio-style track id (`sub.id` / `sub.srclang`).
    #[serde(default)]
    pub id: Option<String>,
    /// The subtitle file URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Track language (`sub.lang`).
    #[serde(default)]
    pub lang: Option<String>,
    /// Display name (`sub.name`).
    #[serde(default)]
    pub name: Option<String>,
    /// ISO language code (`sub.srclang`).
    #[serde(default)]
    pub srclang: Option<String>,
    /// Long-form language (`sub.language`).
    #[serde(default)]
    pub language: Option<String>,
    /// Player label (`sub.label`).
    #[serde(default)]
    pub label: Option<String>,
    /// JW-style file key (`sub.file`).
    #[serde(default)]
    pub file: Option<String>,
}

impl NuvioSubtitle {
    /// The track's file URL — `url` first, then the JW-style `file`.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref().or(self.file.as_deref())
    }

    /// The track's display language — the JS chain prefers `lang`, then
    /// `language`, `srclang`, and `label`.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.lang
            .as_deref()
            .or(self.language.as_deref())
            .or(self.srclang.as_deref())
            .or(self.label.as_deref())
            .or(self.name.as_deref())
    }

    /// The track's id field — `id`, then `srclang`, `language`, `lang`.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id
            .as_deref()
            .or(self.srclang.as_deref())
            .or(self.language.as_deref())
            .or(self.lang.as_deref())
            .or(self.label.as_deref())
    }
}

/// One raw Nuvio scraper stream — the `{ url, quality, title, name,
/// size, headers, subtitles }` shape every `src/nuvio/*.cjs` module
/// returns, plus the extra fields the anime scrapers attach
/// (`behaviorHints.proxyHeaders`, `audioTracks`, `meta.category`,
/// `language`).
#[derive(Debug, Clone, Deserialize)]
pub struct NuvioStream {
    /// The stream URL (http/https only; anything else is dropped).
    pub url: String,
    /// Scraper display name (`AnimeSuge SUB 1080p`, `AnikotoTV | …`).
    #[serde(default)]
    pub name: Option<String>,
    /// Rich display title; feeds the label when `name` is absent.
    #[serde(default)]
    pub title: Option<String>,
    /// Quality label (`1080p`, `4K`, `Auto`).
    #[serde(default)]
    pub quality: Option<String>,
    /// Human file size (`1.5GB`).
    #[serde(default)]
    pub size: Option<String>,
    /// MIME type hint (`application/vnd.apple.mpegurl`, `video/mp4`).
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Audio marker (`sub`/`dub` — `ReAnime`'s `language` field).
    #[serde(default)]
    pub language: Option<String>,
    /// Server name (`ReAnime`'s `source`, `NikaStream`'s raw `server`).
    #[serde(default)]
    pub source: Option<String>,
    /// Per-stream request headers (`Referer`, `User-Agent`, `Origin`).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Subtitle tracks.
    #[serde(default)]
    pub subtitles: Vec<NuvioSubtitle>,
    /// Per-stream audio metadata (string, array, or object array).
    #[serde(rename = "audioTracks", default)]
    pub audio_tracks: Option<Value>,
    /// `hasMultipleAudio` flag.
    #[serde(rename = "hasMultipleAudio", default)]
    pub has_multiple_audio: Option<bool>,
    /// Extra scraper metadata (`meta.category`, `meta.episode`, …).
    pub meta: Option<Value>,
    /// Stremio behavior hints (NikaStream/AnimeZeY carry
    /// `proxyHeaders.request` here).
    #[serde(rename = "behaviorHints", default)]
    pub behavior_hints: Option<Value>,
}

impl NuvioStream {
    /// A stream with just a URL, for typed construction.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            name: None,
            title: None,
            quality: None,
            size: None,
            kind: None,
            language: None,
            source: None,
            headers: BTreeMap::new(),
            subtitles: Vec::new(),
            audio_tracks: None,
            has_multiple_audio: None,
            meta: None,
            behavior_hints: None,
        }
    }

    /// Attach a display name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Attach a display title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Attach a quality label.
    #[must_use]
    pub fn with_quality(mut self, quality: impl Into<String>) -> Self {
        self.quality = Some(quality.into());
        self
    }

    /// Attach a human file size.
    #[must_use]
    pub fn with_size(mut self, size: impl Into<String>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Attach the MIME type hint.
    #[must_use]
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Attach a request header (e.g. `Referer`).
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Attach one subtitle track.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: NuvioSubtitle) -> Self {
        self.subtitles.push(subtitle);
        self
    }

    /// The hotlink `Referer` — `headers` first, then
    /// `behaviorHints.proxyHeaders.request` (AnimeZeY/NikaStream nest
    /// them there).
    #[must_use]
    pub fn referer(&self) -> Option<&str> {
        header_of(&self.headers, "Referer")
            .or_else(|| behavior_proxy_header(self.behavior_hints.as_ref(), "Referer"))
    }

    /// The hotlink `User-Agent` — same two locations.
    #[must_use]
    pub fn user_agent(&self) -> Option<&str> {
        header_of(&self.headers, "User-Agent")
            .or_else(|| behavior_proxy_header(self.behavior_hints.as_ref(), "User-Agent"))
    }

    /// The hotlink `Origin` — same two locations.
    #[must_use]
    pub fn origin(&self) -> Option<&str> {
        header_of(&self.headers, "Origin")
            .or_else(|| behavior_proxy_header(self.behavior_hints.as_ref(), "Origin"))
    }

    /// `meta.category` — `AnimeSuge`'s `sub`/`dub` marker.
    #[must_use]
    pub fn category(&self) -> Option<&str> {
        self.meta
            .as_ref()
            .and_then(|meta| meta.get("category"))
            .and_then(Value::as_str)
    }
}

/// A case-insensitive header lookup on a raw header map.
fn header_of<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// `behaviorHints.proxyHeaders.request.{name}` as a string.
fn behavior_proxy_header<'a>(behavior_hints: Option<&'a Value>, name: &str) -> Option<&'a str> {
    behavior_hints?
        .get("proxyHeaders")?
        .get("request")?
        .get(name)?
        .as_str()
}

/// Parameters for [`build_stream_results`].
pub struct BuildParams<'a> {
    /// The raw scraper streams.
    pub streams: &'a [NuvioStream],
    /// Base display title (`name (year)` or `name S01E02`).
    pub title: &'a str,
    /// The consuming provider's id.
    pub source_id: &'a str,
    /// The consuming provider's display label.
    pub source_label: &'a str,
    /// The provider's default country codes.
    pub country_codes: &'a [CountryCode],
    /// Result lifetime — the JS source's `this.ttl`.
    pub ttl: Duration,
}

/// Convert raw Nuvio scraper streams into [`Stream`]s — the port of
/// `buildStreamResults` (see the module docs for the header-policy
/// mapping).
///
/// Non-http and unparsable URLs are skipped, exactly like the JS
/// `continue`s.
#[must_use]
pub fn build_stream_results(params: &BuildParams<'_>) -> Vec<Stream> {
    let BuildParams {
        streams,
        title,
        source_id,
        source_label,
        country_codes,
        ttl,
    } = params;

    let mut results = Vec::new();
    for stream in *streams {
        if let Some(card) = build_card(stream, title, source_id, source_label, country_codes, *ttl)
        {
            results.push(card);
        }
    }
    results
}

/// One stream's language flags — the audio-track override (the JS's
/// authoritative per-source audio) or the source defaults plus names
/// found in the stream text.
fn stream_languages(
    stream: &NuvioStream,
    stream_title: &str,
    filename: &str,
    country_codes: &[CountryCode],
) -> Vec<CountryCode> {
    let audio_tracks = normalize_audio_tracks(stream.audio_tracks.as_ref());
    if audio_tracks.is_empty() {
        let haystack = format!(
            "{} {} {}",
            stream_title,
            stream.name.as_deref().unwrap_or_default(),
            filename
        );
        let mut codes = country_codes.to_vec();
        codes.extend(find_country_codes(&haystack));
        codes.dedup();
        return codes;
    }
    let mut codes = vec![CountryCode::Multi];
    codes.extend(find_country_codes(&audio_tracks.join(" ")));
    codes.dedup();
    codes
}

/// Convert one raw stream into a [`Stream`] card; `None` skips it
/// (the JS `continue`s on unparsable or non-http URLs).
fn build_card(
    stream: &NuvioStream,
    title: &str,
    source_id: &str,
    source_label: &str,
    country_codes: &[CountryCode],
    ttl: Duration,
) -> Option<Stream> {
    let url = Url::parse(&stream.url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }

    let referer = stream.referer();
    let user_agent = stream.user_agent();
    let origin = stream.origin();
    let host = url.host_str().unwrap_or_default();
    let skip_referer = NO_REFERER_HOSTS.is_match(host).unwrap_or(false);
    let hls = is_hls_url(&url)
        || stream.kind.as_deref().is_some_and(|kind| {
            kind.eq_ignore_ascii_case("hls") || kind.eq_ignore_ascii_case("m3u8")
        });
    let video_file = is_video_file_url(&url)
        || stream.kind.as_deref().is_some_and(|kind| {
            ["mp4", "mkv", "webm"]
                .iter()
                .any(|ext| kind.eq_ignore_ascii_case(ext))
        });

    let filename = extract_filename(&url);
    let display_filename = clean_filename_for_display(&filename);

    // The rich label: `title — streamTitle — displayFilename`.
    let stream_title = stream
        .title
        .clone()
        .or_else(|| stream.quality.clone())
        .unwrap_or_default();
    let mut label_parts = vec![(*title).to_string()];
    if !stream_title.is_empty() {
        label_parts.push(stream_title.clone());
    }
    if !display_filename.is_empty() && display_filename != stream_title {
        label_parts.push(display_filename);
    }
    let label = label_parts.join(" — ");

    // Language flags: explicit audio tracks override the source
    // defaults; otherwise defaults + names found in the stream text.
    let languages = stream_languages(stream, &stream_title, &filename, country_codes);

    let height = parse_height(stream.quality.as_deref())
        .or_else(|| parse_height(stream.title.as_deref()))
        .or_else(|| parse_height(Some(&filename)));

    let mut meta = StreamMeta {
        dubbed: stream.category().map(|category| category == "dub"),
        subbed: stream
            .category()
            .map(|category| category == "sub" || category == "softsub"),
        languages,
        source_id: Some((*source_id).to_string()),
        source_label: Some((*source_label).to_string()),
        resolution: height,
        size: parse_size(stream.size.as_deref()),
        subtitles: stream.subtitles.iter().filter_map(subtitle_track).collect(),
        ..StreamMeta::default()
    };

    // Hotlink headers: the proxy's `nuvioReferer`/`nuvioUserAgent`/
    // `nuvioOrigin` flags become request headers, except on hosts
    // with inverted gates.
    let mut behavior_hints = BTreeMap::new();
    if let Some(referer) = referer.filter(|_| !skip_referer) {
        meta.request_headers
            .insert("Referer".to_string(), referer.to_string());
        if let Some(user_agent) = user_agent {
            meta.request_headers
                .insert("User-Agent".to_string(), user_agent.to_string());
        }
        if let Some(origin) = origin {
            meta.request_headers
                .insert("Origin".to_string(), origin.to_string());
        }
        // Ambiguous URL + Referer → upstream set `nuvioForceHls` so
        // the proxy HEAD-checked the content type; keep the hint for
        // clients that want the same behavior.
        if !hls && !video_file {
            behavior_hints.insert("nuvioForceHls".to_string(), "1".to_string());
        }
    }

    let format = if hls {
        Format::Hls
    } else if video_file {
        Format::Mp4
    } else {
        Format::Unknown
    };

    Some(Stream {
        url,
        format,
        label: Some(label),
        meta,
        ttl,
        is_external: false,
        behavior_hints,
    })
}

/// Map one raw subtitle to a [`SubtitleTrack`] — ports the
/// `subId`/`subLang` fallback chains of `buildStreamResults` (the JS
/// truncates ids to 8 chars).
fn subtitle_track(sub: &NuvioSubtitle) -> Option<SubtitleTrack> {
    let url = sub.url()?;
    let url = Url::parse(url).ok()?;
    let id = sub.id().map_or_else(
        || "en".to_string(),
        |id| id.chars().take(8).collect::<String>(),
    );
    let language = sub.label().unwrap_or("en").to_string();
    Some(SubtitleTrack {
        label: Some(id),
        language: Some(language),
        url,
    })
}

/// `"1080p"` / `"4K"` / `"2160p"` → `1080` / `2160` — ports
/// `parseHeight` (the `\d{3,4}p` form only, so years never match).
#[must_use]
pub fn parse_height(quality: Option<&str>) -> Option<u16> {
    let quality = quality?;
    let lower = quality.to_ascii_lowercase();
    if lower.contains("4k") || lower.contains("2160") {
        return Some(2160);
    }
    QUALITY_P
        .captures_iter(&lower)
        .flatten()
        .filter_map(|captures| captures.get(1))
        .find_map(|group| group.as_str().parse::<u16>().ok())
}

/// Whether the URL is clearly an HLS playlist (`.m3u8` or `/playlist`).
#[must_use]
pub fn is_hls_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains(".m3u8") || path.contains("/m3u8/") || path.contains("/playlist")
}

/// Whether the URL is clearly a video file (`.mp4`, `.mkv`, …).
#[must_use]
pub fn is_video_file_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    [".mp4", ".mkv", ".webm", ".avi", ".mov"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// `"1.5GB"` → bytes — ports `parseSize`.
#[must_use]
// JS `Math.round` over a non-negative parse: the truncating cast is
// the same rounding for every value the regex can produce.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn parse_size(size: Option<&str>) -> Option<u64> {
    let size = size?;
    let captures = SIZE.captures(size).ok()??;
    let number: f64 = captures.get(1)?.as_str().parse().ok()?;
    let unit = captures.get(2)?.as_str().to_ascii_uppercase();
    let bytes = match unit.as_str() {
        "GB" => number * 1024.0 * 1024.0 * 1024.0,
        "MB" => number * 1024.0 * 1024.0,
        "TB" => number * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(bytes.round() as u64)
}

/// The last meaningful path segment — a hash-like last segment is
/// skipped in favor of the one before it (ports `extractFilename`).
#[must_use]
pub fn extract_filename(url: &Url) -> String {
    let parts: Vec<&str> = url.path().split('/').filter(|p| !p.is_empty()).collect();
    let Some(last) = parts.last() else {
        return String::new();
    };
    if !last.contains('.') && parts.len() > 1 && HASH_SEGMENT.is_match(last).unwrap_or(false) {
        return parts[parts.len() - 2].to_string();
    }
    (*last).to_string()
}

/// Clean a filename for display: strip query fragments, hash strings,
/// R2 bucket prefixes, technical playlist indices, generic names, bare
/// route segments, and script endpoints — ports
/// `cleanFilenameForDisplay`.
#[must_use]
pub fn clean_filename_for_display(filename: &str) -> String {
    if filename.is_empty() {
        return String::new();
    }
    let cleaned = filename.split(['?', '#']).next().unwrap_or(filename);
    if cleaned.len() < 3 {
        return String::new();
    }
    if HASH_SEGMENT.is_match(cleaned).unwrap_or(false)
        || R2_BUCKET.is_match(cleaned).unwrap_or(false)
        || PLAYLIST_INDEX.is_match(cleaned).unwrap_or(false)
        || GENERIC_NAME.is_match(cleaned).unwrap_or(false)
        || BARE_SEGMENT.is_match(cleaned).unwrap_or(false)
        || SCRIPT_ENDPOINT.is_match(cleaned).unwrap_or(false)
    {
        return String::new();
    }
    // A base name of 30+ hash-ish chars (VidEasy's moon CDN).
    let base = cleaned
        .trim_end_matches(".m3u8")
        .trim_end_matches(".mp4")
        .trim_end_matches(".mkv")
        .trim_end_matches(".webm")
        .trim_end_matches(".avi")
        .trim_end_matches(".mov");
    if base.chars().count() > 30 && LONG_BASE.is_match(base).unwrap_or(false) {
        return String::new();
    }
    // Long names need at least one readable segment.
    if cleaned.len() > 40 {
        let readable = cleaned
            .split(['.', '_', '-'])
            .filter(|segment| {
                segment.len() >= 2
                    && segment.chars().any(|c| c.is_ascii_alphabetic())
                    && !segment.chars().all(|c| c.is_ascii_hexdigit())
            })
            .count();
        if readable == 0 {
            return String::new();
        }
    }
    cleaned.to_string()
}

/// Run a sweep with the bounded retry-on-empty policy — the port of
/// `withRetryOnEmpty`: an empty (or failed) attempt is retried while at
/// half the total budget remains, so the last attempt always keeps half
/// the budget; a `None` (deadline sentinel) is propagated immediately.
///
/// `f` returns `None` when the caller's deadline already fired (the JS
/// timeout race's `null`), `Some(vec)` otherwise. Errors are the JS
/// throws: they count as empty and retry — map them to an empty sweep
/// at the call site.
pub async fn with_retry_on_empty<T, F, Fut>(
    mut f: F,
    attempts: u32,
    max_total: Duration,
    backoff: Duration,
) -> Option<Vec<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<T>>>,
{
    let start = Instant::now();
    let mut last: Option<Vec<T>> = Some(Vec::new());
    for attempt in 0..attempts {
        if attempt > 0 && start.elapsed() >= max_total {
            break;
        }
        match f().await {
            None => return None,
            Some(result) if !result.is_empty() => return Some(result),
            Some(result) => last = Some(result),
        }
        if attempt + 1 < attempts && start.elapsed() < max_total / 2 {
            tokio::time::sleep(backoff).await;
        } else {
            break;
        }
    }
    last
}

/// Race a future against a deadline — the JS `Promise.race` with a
/// timeout sentinel. `None` means the deadline won.
pub async fn with_deadline<F: Future>(future: F, deadline: Duration) -> Option<F::Output> {
    tokio::time::timeout(deadline, future).await.ok()
}

/// Normalize a raw `audioTracks` value into canonical language names —
/// ports `normalizeAudioTracks`: `"Hindi,English"`, `["Hindi","en"]`,
/// `[{language: "Hindi"}]`, and JSON-encoded strings are all accepted;
/// unknown names are title-cased; deduped, capped at 6.
#[must_use]
pub fn normalize_audio_tracks(raw: Option<&Value>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let list: Vec<String> = match raw {
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Vec::new();
            }
            if text.starts_with('[') {
                match serde_json::from_str::<Vec<Value>>(text) {
                    Ok(items) => items
                        .into_iter()
                        .map(|item| match item {
                            Value::String(name) => name,
                            other => name_of_track(&other).unwrap_or_default(),
                        })
                        .collect(),
                    Err(_) => vec![text.to_string()],
                }
            } else {
                split_language_list(text)
            }
        }
        Value::Array(items) => items.iter().filter_map(name_of_track).collect(),
        _ => Vec::new(),
    };

    let mut out: Vec<String> = Vec::new();
    for name in list {
        let name = name
            .trim()
            .to_lowercase()
            .trim_matches(|c: char| c == '[' || c == ']' || c == '"')
            .to_string();
        if name.is_empty()
            || matches!(
                name.as_str(),
                "null" | "undefined" | "unknown" | "original" | "none" | "default"
            )
        {
            continue;
        }
        let canonical =
            audio_language_alias(&name).map_or_else(|| title_case(&name), str::to_string);
        if !out.contains(&canonical) {
            out.push(canonical);
        }
        if out.len() >= 6 {
            break;
        }
    }
    out
}

/// The `language`/`lang`/`name`/`label`/`title`/`code` string of an
/// object track entry.
fn name_of_track(item: &Value) -> Option<String> {
    match item {
        Value::String(name) => Some(name.clone()),
        Value::Object(_) => [
            "language", "lang", "name", "label", "title", "code", "iso639_1", "iso639",
        ]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .map(str::to_string),
        _ => None,
    }
}

/// Split a comma/plus/slash/semicolon/`and`-separated language list.
fn split_language_list(text: &str) -> Vec<String> {
    const SEPARATORS: [char; 6] = [',', '+', '/', '&', ';', '|'];
    let lower = text.to_lowercase();
    let replaced: String = lower
        .chars()
        .map(|character| {
            if SEPARATORS.contains(&character) {
                '\u{1}'
            } else {
                character
            }
        })
        .collect::<String>()
        .replace(" and ", "\u{1}");
    replaced
        .split('\u{1}')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lowercase alias → canonical display name (ports
/// `AUDIO_LANG_ALIASES`, trimmed to the languages the language table
/// resolves).
#[must_use]
pub fn audio_language_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "english" | "en" | "eng" => "English",
        "hindi" | "hi" | "hin" => "Hindi",
        "japanese" | "ja" | "jpn" | "jp" => "Japanese",
        "korean" | "ko" | "kor" => "Korean",
        "tamil" | "ta" | "tam" => "Tamil",
        "telugu" | "te" | "tel" => "Telugu",
        "malayalam" | "ml" | "mal" => "Malayalam",
        "punjabi" | "pa" | "pan" => "Punjabi",
        "bengali" | "bn" | "ben" => "Bengali",
        "marathi" | "mr" | "mar" => "Marathi",
        "gujarati" | "gu" | "guj" => "Gujarati",
        "kannada" | "kn" | "kan" => "Kannada",
        "spanish" | "es" | "spa" => "Spanish",
        "french" | "fr" | "fra" => "French",
        "german" | "de" | "ger" => "German",
        "italian" | "it" | "ita" => "Italian",
        "portuguese" | "pt" | "por" => "Portuguese",
        "russian" | "ru" | "rus" => "Russian",
        "arabic" | "ar" | "ara" => "Arabic",
        "chinese" | "mandarin" | "zh" | "chi" | "zho" => "Chinese",
        "cantonese" | "yue" => "Cantonese",
        "turkish" | "tr" | "tur" => "Turkish",
        "indonesian" | "id" | "ind" => "Indonesian",
        "thai" | "th" | "tha" => "Thai",
        "vietnamese" | "vi" | "vie" => "Vietnamese",
        "filipino" | "tagalog" | "fil" | "tl" => "Filipino",
        "persian" | "farsi" | "fa" | "fas" => "Persian",
        "hebrew" | "he" | "heb" => "Hebrew",
        "polish" | "pl" | "pol" => "Polish",
        "dutch" | "nl" | "nld" => "Dutch",
        "ukrainian" | "uk" | "ukr" => "Ukrainian",
        "urdu" | "ur" => "Urdu",
        "nepali" | "ne" => "Nepali",
        _ => return None,
    })
}

/// `hindi` → `Hindi` (single words, like the JS word-boundary regex).
fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The dual/multi-audio display label — ports `buildAudioLabel`:
/// 2 tracks → `Dual Audio (A + B)`, 3+ → `Multi Audio (…)`, 1 → the
/// name, 0 + flag → `Dual Audio`.
#[must_use]
pub fn build_audio_label(tracks: &[String], has_multiple_audio: Option<bool>) -> Option<String> {
    if tracks.len() >= 2 {
        let kind = if tracks.len() == 2 { "Dual" } else { "Multi" };
        return Some(format!(
            "{kind} Audio ({})",
            tracks
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(" + ")
        ));
    }
    if tracks.len() == 1 {
        return Some(tracks[0].clone());
    }
    if has_multiple_audio == Some(true) {
        return Some("Dual Audio".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw AnimeZeY-style stream (from the live scraper trace).
    fn animezey_stream() -> NuvioStream {
        NuvioStream::new(
            "https://animezey16082023.animezey16082023.workers.dev/download.aspx?file=abc&mac=1",
        )
        // The animezey wrapper guarantees `title`/`quality` on every
        // stream (falling back to the name and parseQuality).
        .with_title("AnimeZeY | 1080p | Dual-Audio")
        .with_quality("1080p")
        .with_name("AnimeZeY | 1080p | Dual-Audio")
        .with_size("1.21 GB")
        .with_header(
            "User-Agent",
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)",
        )
        .with_header("Referer", "https://1.animezey23112022.workers.dev/")
        .with_subtitle(NuvioSubtitle {
            url: Some("https://sub.example.com/en.vtt".to_string()),
            lang: Some("English".to_string()),
            ..NuvioSubtitle::default()
        })
    }

    #[test]
    fn parses_height_labels() {
        assert_eq!(parse_height(Some("1080p")), Some(1080));
        assert_eq!(parse_height(Some("4K")), Some(2160));
        assert_eq!(parse_height(Some("2160p HDR")), Some(2160));
        assert_eq!(parse_height(Some("720p")), Some(720));
        // A year in a title never matches (the `p`-less form).
        assert_eq!(parse_height(Some("Dune 2021")), None);
        assert_eq!(parse_height(None), None);
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size(Some("1.5GB")), Some(1_610_612_736));
        assert_eq!(parse_size(Some("700 MB")), Some(734_003_200));
        assert_eq!(parse_size(Some("Unknown")), None);
    }

    #[test]
    fn detects_url_kinds() {
        let hls = Url::parse("https://cdn.example/path/index.m3u8?token=1")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        let mp4 = Url::parse("https://cdn.example/movie.mkv")
            .unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert!(is_hls_url(&hls));
        assert!(!is_video_file_url(&hls));
        assert!(is_video_file_url(&mp4));
        assert!(!is_hls_url(&mp4));
    }

    #[test]
    fn extracts_filenames() {
        let plain =
            Url::parse("https://pub.example/Movies4u.Vip.Dune.Part.Two.2024.1080p.WEB-DL.mkv")
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
        assert_eq!(
            extract_filename(&plain),
            "Movies4u.Vip.Dune.Part.Two.2024.1080p.WEB-DL.mkv"
        );
        let hashed =
            Url::parse("https://moon.example/vd/ADGPM2IzbD60Hu_XUAZoxoFPl/index-s2160p-v1-a1.m3u8")
                .unwrap_or_else(|e| panic!("valid URL: {e}"));
        // The hash-like last-but-one segment is skipped by the site's
        // own path shape, so the playlist index stays.
        assert_eq!(extract_filename(&hashed), "index-s2160p-v1-a1.m3u8");
    }

    #[test]
    fn cleans_display_filenames() {
        // Technical playlist index → dropped (quality lives in meta).
        assert_eq!(clean_filename_for_display("index-s2160p-v1-a1.m3u8"), "");
        // R2 bucket prefix → dropped.
        assert_eq!(
            clean_filename_for_display("Pub-35214751cbf1431ba7b6d74f519e61d2_x.m3u8"),
            ""
        );
        // A real release name survives.
        assert_eq!(
            clean_filename_for_display("Dune.Part.Two.2024.1080p.AMZN.WEB-DL.mkv"),
            "Dune.Part.Two.2024.1080p.AMZN.WEB-DL.mkv"
        );
        // Technical proxy-route names (atlantic's CDN) → dropped.
        assert_eq!(clean_filename_for_display("m3u8-proxy"), "");
        // Script endpoints → dropped.
        assert_eq!(clean_filename_for_display("download.aspx"), "");
        // Long hash names → dropped.
        assert_eq!(
            clean_filename_for_display(
                "ma9ylsUHLd1oEUKmveBRDzgXvby4MyCQsteH9ZA1O0g0XZIbx0AtmD.m3u8"
            ),
            ""
        );
    }

    #[test]
    fn normalizes_audio_tracks() {
        let comma = serde_json::json!("Hindi,English");
        assert_eq!(
            normalize_audio_tracks(Some(&comma)),
            vec!["Hindi".to_string(), "English".to_string()]
        );
        let array = serde_json::json!(["ja", "English"]);
        assert_eq!(
            normalize_audio_tracks(Some(&array)),
            vec!["Japanese".to_string(), "English".to_string()]
        );
        let objects = serde_json::json!([{ "language": "Hindi" }, { "lang": "tam" }]);
        assert_eq!(
            normalize_audio_tracks(Some(&objects)),
            vec!["Hindi".to_string(), "Tamil".to_string()]
        );
        let json_string = serde_json::json!("[\"Hindi\",\"English\"]");
        assert_eq!(
            normalize_audio_tracks(Some(&json_string)),
            vec!["Hindi".to_string(), "English".to_string()]
        );
        let and_list = serde_json::json!("Hindi and English");
        assert_eq!(
            normalize_audio_tracks(Some(&and_list)),
            vec!["Hindi".to_string(), "English".to_string()]
        );
        assert!(normalize_audio_tracks(None).is_empty());
    }

    #[test]
    fn builds_audio_labels() {
        let tracks = vec!["Hindi".to_string(), "English".to_string()];
        assert_eq!(
            build_audio_label(&tracks, None),
            Some("Dual Audio (Hindi + English)".to_string())
        );
        let three = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            build_audio_label(&three, None),
            Some("Multi Audio (a + b + c)".to_string())
        );
        assert_eq!(
            build_audio_label(&[], Some(true)),
            Some("Dual Audio".to_string())
        );
        assert_eq!(build_audio_label(&[], Some(false)), None);
    }

    #[test]
    fn builds_stream_results_with_headers_and_languages() {
        let streams = vec![animezey_stream()];
        let params = BuildParams {
            streams: &streams,
            title: "Frieren S01E01",
            source_id: "animezey",
            source_label: "AnimeZeY",
            country_codes: &[CountryCode::Multi, CountryCode::Ja, CountryCode::En],
            ttl: Duration::from_mins(10),
        };
        let results = build_stream_results(&params);
        assert_eq!(results.len(), 1);
        let stream = &results[0];
        // download.aspx is neither playlist nor video-file extension.
        assert_eq!(stream.format, Format::Unknown);
        // The referer host has no inverted gate → request headers ride.
        assert_eq!(
            stream
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://1.animezey23112022.workers.dev/")
        );
        assert!(
            stream
                .meta
                .request_headers
                .get("User-Agent")
                .is_some_and(|ua| ua.contains("iPhone"))
        );
        // Ambiguous URL + referer → the force-HLS hint.
        assert_eq!(
            stream
                .behavior_hints
                .get("nuvioForceHls")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(stream.meta.resolution, Some(1080));
        assert_eq!(stream.meta.size, Some(1_299_227_607));
        assert_eq!(
            stream.label.as_deref(),
            Some("Frieren S01E01 — AnimeZeY | 1080p | Dual-Audio")
        );
        assert_eq!(stream.meta.subtitles.len(), 1);
        assert_eq!(
            stream.meta.subtitles[0].url.as_str(),
            "https://sub.example.com/en.vtt"
        );
    }

    #[test]
    fn skips_referer_on_inverted_gate_hosts() {
        let streams = vec![
            NuvioStream::new("https://vidking.example.vimeos.net/hls/x/master.m3u8")
                .with_header("Referer", "https://example.com/"),
        ];
        let params = BuildParams {
            streams: &streams,
            title: "T",
            source_id: "x",
            source_label: "X",
            country_codes: &[CountryCode::Multi],
            ttl: Duration::from_mins(1),
        };
        let results = build_stream_results(&params);
        assert!(results[0].meta.request_headers.is_empty());
        assert!(results[0].behavior_hints.is_empty());
    }

    #[test]
    fn audio_tracks_override_country_codes() {
        let mut stream = NuvioStream::new("https://cdn.example/movie.mp4");
        stream.audio_tracks = Some(serde_json::json!(["Hindi", "Tamil"]));
        let streams = vec![stream];
        let params = BuildParams {
            streams: &streams,
            title: "T",
            source_id: "x",
            source_label: "X",
            country_codes: &[CountryCode::Multi, CountryCode::En],
            ttl: Duration::from_mins(1),
        };
        let results = build_stream_results(&params);
        assert_eq!(
            results[0].meta.languages,
            vec![CountryCode::Multi, CountryCode::Hi, CountryCode::Ta]
        );
    }

    #[test]
    fn deserializes_js_shaped_streams() {
        let raw = r#"{
            "url": "https://megap.akirax.buzz/hls/abc123/master.m3u8?token=xyz",
            "name": "AnikotoTV | 1080p | Japanese (SUB)",
            "quality": "1080p",
            "type": "application/vnd.apple.mpegurl",
            "subtitles": [
                {"id": "English", "url": "https://megaplay.buzz/sub/1.vtt", "language": "eng"}
            ],
            "headers": {"Referer": "https://megaplay.buzz/", "Origin": "https://megaplay.buzz"}
        }"#;
        let stream: NuvioStream =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("valid JS stream JSON: {e}"));
        assert_eq!(stream.referer(), Some("https://megaplay.buzz/"));
        assert_eq!(stream.origin(), Some("https://megaplay.buzz"));
        assert_eq!(
            stream.kind.as_deref(),
            Some("application/vnd.apple.mpegurl")
        );
        assert_eq!(stream.subtitles[0].id(), Some("English"));
        assert_eq!(stream.subtitles[0].label(), Some("eng"));
    }

    #[tokio::test]
    async fn retries_on_empty_then_succeeds() {
        let mut attempts = 0;
        let result = with_retry_on_empty(
            || {
                attempts += 1;
                async move {
                    if attempts < 2 {
                        Some(Vec::<u8>::new())
                    } else {
                        Some(vec![1])
                    }
                }
            },
            3,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(result, Some(vec![1]));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn propagates_deadline_sentinel() {
        let result: Option<Vec<u8>> = with_retry_on_empty::<u8, _, _>(
            || async { None },
            3,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn deadline_race_returns_none_on_timeout() {
        let slow = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            1
        };
        assert_eq!(with_deadline(slow, Duration::from_millis(1)).await, None);
        let fast = async { 2 };
        assert_eq!(with_deadline(fast, Duration::from_secs(1)).await, Some(2));
    }
    #[test]
    fn explicit_hls_type_and_nexabloom_playback_headers_survive_conversion() {
        let mut stream = NuvioStream::new("https://fetch.nexabloom.top/opaque")
            .with_header("Referer", "https://megaplay.buzz/");
        stream.kind = Some("hls".to_string());
        let card = build_card(
            &stream,
            "Title",
            "source",
            "Source",
            &[],
            Duration::from_mins(5),
        )
        .unwrap_or_else(|| panic!("valid stream"));
        assert_eq!(card.format, Format::Hls);
        assert_eq!(
            card.meta.request_headers.get("Referer").map(String::as_str),
            Some("https://megaplay.buzz/")
        );
    }
}
