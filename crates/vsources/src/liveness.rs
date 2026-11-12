//! Conservative, bounded media validation shared by the engine and embedders.
//!
//! Only definitive file/tree failures are dropped. Authentication, network,
//! rate-limit and Cloudflare failures remain inconclusive: the player's IP
//! may succeed. Verdicts are per URL **and playback headers**, never per host.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use moka::Expiry;
use moka::future::Cache;
use tokio::sync::Semaphore;
use url::Url;
use vsources_core::error::FetchError;
use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ProbeResponse};
use vsources_core::types::{Format, Stream};

/// The result of a lightweight media probe, not a guarantee of full playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A playlist chain or direct response ended in recognizable media.
    Alive,
    /// Definitive evidence of missing media or a non-media payload.
    Dead,
    /// Inconclusive, unsupported or intentionally skipped; keep the stream.
    Unknown,
}

/// Limits for an engine's shared media probes.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Whether automatic engine validation is enabled (default true).
    pub enabled: bool,
    /// Maximum probe chains in flight across all resolves (default 6).
    pub concurrency: usize,
    /// Total budget per chain and provider batch, including queues (4 s).
    pub timeout: Duration,
    /// Cache lifetime for recognizable media (5 min).
    pub alive_ttl: Duration,
    /// Cache lifetime for definitive failures (30 s).
    pub dead_ttl: Duration,
    /// Cache lifetime for inconclusive responses (5 s).
    pub unknown_ttl: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            concurrency: 6,
            timeout: Duration::from_secs(4),
            alive_ttl: Duration::from_secs(300),
            dead_ttl: Duration::from_secs(30),
            unknown_ttl: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    url: Url,
    headers: BTreeMap<String, String>,
    format: Format,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedVerdict {
    verdict: Verdict,
    ttl: Duration,
    audio: AudioEvidence,
}

#[derive(Clone, Default)]
struct AudioEvidence {
    languages: Vec<vsources_core::types::CountryCode>,
    english_audio_index: Option<u32>,
}

struct VerdictExpiry;
impl Expiry<Key, CachedVerdict> for VerdictExpiry {
    fn expire_after_create(&self, _: &Key, value: &CachedVerdict, _: Instant) -> Option<Duration> {
        Some(value.ttl)
    }
}

/// A bounded verdict cache with cancellation-safe in-flight coalescing.
///
/// Reuse one instance for a session. No background tasks, shell programs or
/// permanent host bans are needed. Custom fetchers can implement
/// [`Fetcher::probe`] to opt in; otherwise every verdict stays inconclusive.
pub struct StreamProbe {
    config: ProbeConfig,
    cache: Cache<Key, CachedVerdict>,
    permits: Semaphore,
}

impl StreamProbe {
    /// Create a probe cache with at most 4096 entries.
    pub fn new(config: ProbeConfig) -> Self {
        Self {
            permits: Semaphore::new(config.concurrency.max(1)),
            config,
            cache: Cache::builder()
                .max_capacity(4096)
                .expire_after(VerdictExpiry)
                .build(),
        }
    }

    /// Check a stream with its playback headers and a bounded total budget.
    ///
    /// External pages, non-HTTP URLs and known single-use download URLs are
    /// skipped. Unknown verdicts must never be interpreted as dead streams.
    pub async fn check(&self, fetcher: &dyn Fetcher, stream: &Stream) -> Verdict {
        self.inspect(fetcher, stream).await.verdict
    }

    async fn inspect(&self, fetcher: &dyn Fetcher, stream: &Stream) -> CachedVerdict {
        if !self.config.enabled
            || stream.is_external
            || !matches!(stream.url.scheme(), "http" | "https")
            || stream.url.host_str().is_some_and(|host| {
                host == "video-downloads.googleusercontent.com"
                    || host.ends_with(".video-downloads.googleusercontent.com")
            })
        {
            return CachedVerdict {
                verdict: Verdict::Unknown,
                ttl: self.config.unknown_ttl,
                audio: AudioEvidence::default(),
            };
        }
        let key = Key {
            url: stream.url.clone(),
            headers: stream
                .meta
                .request_headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect(),
            format: stream.format,
            ttl: stream.ttl,
        };
        self.cache
            .get_with(key.clone(), async {
                let mut audio = AudioEvidence::default();
                let verdict = tokio::time::timeout(self.config.timeout, async {
                    let Ok(_permit) = self.permits.acquire().await else {
                        return Verdict::Unknown;
                    };
                    probe_tree(fetcher, &key, self.config.timeout, &mut audio).await
                })
                .await
                .unwrap_or(Verdict::Unknown);
                let ttl = match verdict {
                    Verdict::Alive => self.config.alive_ttl,
                    Verdict::Dead => self.config.dead_ttl,
                    Verdict::Unknown => self.config.unknown_ttl,
                }
                .min(stream.ttl);
                tracing::debug!(
                    host = stream.url.host_str(),
                    ?verdict,
                    "media probe completed"
                );
                CachedVerdict {
                    verdict,
                    ttl,
                    audio,
                }
            })
            .await
    }

    pub(crate) async fn filter_progressive(
        &self,
        fetcher: &dyn Fetcher,
        streams: Vec<Stream>,
        audio_language: Option<vsources_core::types::CountryCode>,
        mut publish: impl FnMut(Vec<Stream>),
    ) -> Vec<Stream> {
        let mut streams = streams;
        streams.sort_by_key(|s| std::cmp::Reverse(s.meta.resolution.unwrap_or(0)));
        let mut completed = HashSet::new();
        let mut kept = Vec::new();
        let jobs: Vec<BoxFuture<'_, (usize, Verdict, Stream)>> = streams
            .iter()
            .enumerate()
            .map(|(i, source)| {
                Box::pin(async move {
                    let mut source = source.clone();
                    let compatible = audio_language.is_none_or(|target| {
                        source.meta.audio_selection.as_ref().map_or_else(
                            || {
                                source.meta.languages.iter().all(|language| {
                                    *language == vsources_core::types::CountryCode::Multi
                                }) || source.meta.languages.contains(&target)
                            },
                            |selected| selected.language == target,
                        )
                    });
                    let result = if compatible {
                        self.inspect(fetcher, &source).await
                    } else {
                        CachedVerdict {
                            verdict: Verdict::Unknown,
                            ttl: self.config.unknown_ttl,
                            audio: AudioEvidence::default(),
                        }
                    };
                    if !result.audio.languages.is_empty() {
                        source.meta.languages = result.audio.languages;
                    }
                    if audio_language == Some(vsources_core::types::CountryCode::En)
                        && source.meta.audio_selection.is_none()
                        && let Some(audio_index) = result.audio.english_audio_index
                    {
                        source.meta.audio_selection = Some(vsources_core::types::AudioSelection {
                            language: vsources_core::types::CountryCode::En,
                            audio_index,
                        });
                    }
                    let verdict = result.verdict;
                    (i, verdict, source)
                }) as BoxFuture<'_, (usize, Verdict, Stream)>
            })
            .collect();
        let mut pending = stream::iter(jobs).buffer_unordered(self.config.concurrency.max(1));
        let _ = tokio::time::timeout(self.config.timeout, async {
            while let Some((i, verdict, source)) = pending.next().await {
                completed.insert(i);
                if verdict != Verdict::Dead {
                    publish(vec![source.clone()]);
                    kept.push(source);
                }
            }
        })
        .await;
        drop(pending);
        for (i, source) in streams.into_iter().enumerate() {
            if !completed.contains(&i) {
                publish(vec![source.clone()]);
                kept.push(source);
            }
        }
        kept
    }

    /// Bound the whole batch, so a provider with hundreds of URLs cannot
    /// add hundreds of timeout windows. Unfinished probes remain unknown.
    pub(crate) async fn filter(&self, fetcher: &dyn Fetcher, streams: Vec<Stream>) -> Vec<Stream> {
        let mut completed = BTreeMap::new();
        let jobs: Vec<BoxFuture<'_, (usize, CachedVerdict)>> = streams
            .iter()
            .enumerate()
            .map(|(index, stream)| {
                Box::pin(async move { (index, self.inspect(fetcher, stream).await) })
                    as BoxFuture<'_, _>
            })
            .collect();
        let mut pending = stream::iter(jobs).buffer_unordered(self.config.concurrency.max(1));
        let _ = tokio::time::timeout(self.config.timeout, async {
            while let Some((index, verdict)) = pending.next().await {
                completed.insert(index, verdict);
            }
        })
        .await;
        drop(pending);
        streams
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut stream)| {
                if let Some(result) = completed.remove(&index) {
                    if result.verdict == Verdict::Dead {
                        return None;
                    }
                    if !result.audio.languages.is_empty() {
                        stream.meta.languages = result.audio.languages;
                    }
                }
                Some(stream)
            })
            .collect()
    }
}

fn audio_code(tag: &str) -> Option<vsources_core::types::CountryCode> {
    let tag = tag.trim_matches('"').to_ascii_lowercase();
    let tag = tag.split('-').next().unwrap_or("");
    vsources_core::types::CountryCode::parse(match tag {
        "eng" => "en",
        "spa" => "es",
        "fra" | "fre" => "fr",
        "deu" | "ger" => "de",
        "jpn" => "ja",
        "ita" => "it",
        "por" => "pt",
        "hin" => "hi",
        other => other,
    })
}
fn hls_attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let mut quoted = false;
    let mut start = 0;
    for (index, ch) in line
        .char_indices()
        .chain(std::iter::once((line.len(), ',')))
    {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            if let Some((key, value)) = line[start..index].trim().split_once('=')
                && key.trim() == name
            {
                return Some(value.trim().trim_matches('"'));
            }
            start = index + 1;
        }
    }
    None
}

impl AudioEvidence {
    fn observe(&mut self, response: &ProbeResponse) {
        if !(200..300).contains(&response.status) {
            return;
        }
        let body = String::from_utf8_lossy(&response.body);
        if body
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("#EXTM3U")
        {
            if response.truncated {
                return;
            }
            let variants: Vec<_> = body
                .lines()
                .filter_map(|line| line.trim().strip_prefix("#EXT-X-STREAM-INF:"))
                .collect();
            let groups: HashSet<_> = variants
                .iter()
                .filter_map(|line| hls_attribute(line, "AUDIO"))
                .collect();
            for line in body
                .lines()
                .filter_map(|line| line.trim().strip_prefix("#EXT-X-MEDIA:"))
            {
                if hls_attribute(line, "TYPE") != Some("AUDIO")
                    || (!variants.is_empty()
                        && !hls_attribute(line, "GROUP-ID")
                            .is_some_and(|group| groups.contains(group)))
                {
                    continue;
                }
                if let Some(language) = hls_attribute(line, "LANGUAGE").and_then(audio_code)
                    && !self.languages.contains(&language)
                {
                    self.languages.push(language);
                }
            }
        } else if self.languages.is_empty() {
            if let Some(languages) = vsources_core::audio::mpegts_audio_languages(&response.body) {
                self.english_audio_index = languages
                    .iter()
                    .position(|tag| audio_code(tag) == Some(vsources_core::types::CountryCode::En))
                    .and_then(|index| u32::try_from(index).ok());
                self.languages = languages.iter().filter_map(|tag| audio_code(tag)).collect();
            }
            if let Some(index) = vsources_core::audio::matroska_english_audio_index(&response.body)
            {
                self.english_audio_index = Some(index);
                if !self
                    .languages
                    .contains(&vsources_core::types::CountryCode::En)
                {
                    self.languages.push(vsources_core::types::CountryCode::En);
                }
            }
        }
    }
}

/// A playlist is fetched in full up to 64 KiB; media only needs a prefix.
const PLAYLIST_LIMIT: usize = 64 * 1024;
const MEDIA_LIMIT: usize = 2048;
const MAX_PLAYLISTS: usize = 3;

fn is_playlist_url(url: &Url) -> bool {
    url.path()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("m3u8"))
}

async fn probe_tree(
    fetcher: &dyn Fetcher,
    key: &Key,
    timeout: Duration,
    audio: &mut AudioEvidence,
) -> Verdict {
    let mut current = key.url.clone();
    let mut playlist = key.format == Format::Hls || is_playlist_url(&current);
    let mut seen = HashSet::new();
    let mut offset = 0_u64;
    for _ in 0..=MAX_PLAYLISTS {
        if !seen.insert(current.clone()) {
            return Verdict::Unknown;
        }
        let limit = if playlist {
            PLAYLIST_LIMIT
        } else {
            MEDIA_LIMIT
        };
        let mut request = FetchRequest::get(current.clone()).with_timeout(timeout);
        request.headers.clone_from(&key.headers);
        // Do not inherit an unrelated player's Range header. Playlist bodies
        // need complete lines; media needs enough bytes to recognize archives.
        request.headers.remove("range");
        request
            .headers
            .insert("accept-encoding".into(), "identity".into());
        if !playlist {
            let Some(end) = offset.checked_add(2047) else {
                return Verdict::Unknown;
            };
            request
                .headers
                .insert("range".into(), format!("bytes={offset}-{end}"));
        }
        let response = match fetcher.probe(request, limit).await {
            Ok(Some(response)) => response,
            Err(
                FetchError::NotFound { .. }
                | FetchError::Http {
                    status: 404 | 410, ..
                },
            ) => return Verdict::Dead,
            _ => return Verdict::Unknown,
        };
        audio.observe(&response);
        match classify(&response) {
            Payload::Verdict(verdict) => return verdict,
            Payload::Playlist => {
                let next = match playlist_child(&response) {
                    Ok(Some(next)) => next,
                    Err(()) => return Verdict::Unknown,
                    Ok(None) => {
                        // An ended, complete empty playlist is definitely unusable;
                        // live and truncated playlists can acquire segments later.
                        let body = String::from_utf8_lossy(&response.body);
                        return if !response.truncated && body.contains("#EXT-X-ENDLIST") {
                            Verdict::Dead
                        } else {
                            Verdict::Unknown
                        };
                    }
                };
                current = next.url;
                playlist = next.playlist;
                offset = next.offset;
            }
        }
    }
    Verdict::Unknown // Never declare a tree alive without reaching media.
}

enum Payload {
    Playlist,
    Verdict(Verdict),
}

fn classify(response: &ProbeResponse) -> Payload {
    classify_payload(
        response,
        vsources_core::media_validation::known_error_clip_prefix(&response.body),
    )
}
fn classify_payload(response: &ProbeResponse, known_error_clip: bool) -> Payload {
    let verdict = |v| Payload::Verdict(v);
    let text = String::from_utf8_lossy(&response.body);
    let detection_response = FetchResponse {
        url: response.url.clone(),
        status: response.status,
        headers: response.headers.clone(),
        body: text.to_string(),
    };
    if vsources_cloudflare::detection::detect(&detection_response).is_some() {
        return verdict(Verdict::Unknown);
    }
    match response.status {
        404 | 410 => return verdict(Verdict::Dead),
        200..=299 => {}
        _ => return verdict(Verdict::Unknown),
    }
    if response.body.is_empty() {
        return verdict(Verdict::Unknown);
    }
    let body = response.body.as_slice();
    let head = text.trim_start_matches('\u{feff}').trim_start();
    let ct = response
        .header("content-type")
        .unwrap_or("")
        .to_ascii_lowercase();
    let disposition = response
        .header("content-disposition")
        .unwrap_or("")
        .to_ascii_lowercase();
    if archive(body) || archive_filename(&disposition) {
        return verdict(Verdict::Dead);
    }
    // Body signatures take priority over lying CDN Content-Type headers.
    if head.starts_with("#EXTM3U") {
        return Payload::Playlist;
    }
    if known_error_clip {
        return verdict(Verdict::Dead);
    }
    if media(body) {
        return verdict(Verdict::Alive);
    }
    if image(body) {
        return verdict(Verdict::Dead);
    }
    let lower = head.to_ascii_lowercase();
    if lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || lower.starts_with("<error")
    {
        return verdict(Verdict::Dead);
    }
    // Arbitrary binary data may be encrypted media. NUL alone is not a
    // media signature (archives and thumbnails also contain NUL bytes).
    let textual = std::str::from_utf8(body).is_ok() && !body.contains(&0);
    if ct.contains("zip")
        || ct.contains("x-rar")
        || ct.contains("x-7z")
        || ct.contains("x-tar")
        || ct.contains("gzip")
        || (textual && (ct.contains("text/html") || ct.contains("application/json")))
    {
        return verdict(Verdict::Dead);
    }
    verdict(Verdict::Unknown)
}

fn archive(body: &[u8]) -> bool {
    [
        b"PK\x03\x04".as_slice(),
        b"PK\x05\x06",
        b"PK\x07\x08",
        b"Rar!\x1a\x07",
        b"7z\xbc\xaf\x27\x1c",
        b"\x1f\x8b",
    ]
    .iter()
    .any(|magic| body.starts_with(magic))
        || body.get(257..262) == Some(b"ustar")
}

fn archive_filename(disposition: &str) -> bool {
    disposition.split(';').any(|part| {
        let Some((key, value)) = part.trim().split_once('=') else {
            return false;
        };
        if !matches!(key.trim(), "filename" | "filename*") {
            return false;
        }
        let value = value.trim().trim_matches('"').replace("%2e", ".");
        [".zip", ".rar", ".7z", ".tar", ".gz", ".001"]
            .iter()
            .any(|ext| value.ends_with(ext))
    })
}

fn media(body: &[u8]) -> bool {
    matches!(
        body.get(4..8),
        Some(b"ftyp" | b"styp" | b"moof" | b"moov" | b"mdat")
    ) || body.starts_with(b"\x1a\x45\xdf\xa3")
        || (body.first() == Some(&0x47) && body.get(188) == Some(&0x47))
        || body.starts_with(b"FLV")
}

fn image(body: &[u8]) -> bool {
    body.starts_with(b"\x89PNG\r\n\x1a\n")
        || body.starts_with(b"\xff\xd8\xff")
        || body.starts_with(b"GIF87a")
        || body.starts_with(b"GIF89a")
        || (body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP"))
}

struct Child {
    url: Url,
    playlist: bool,
    offset: u64,
}

fn playlist_child(response: &ProbeResponse) -> Result<Option<Child>, ()> {
    let body = std::str::from_utf8(&response.body)
        .map_err(|_| ())?
        .trim_start_matches('\u{feff}');
    let mut variant = false;
    let mut offset = 0_u64;
    // An incomplete last line may be a truncated URL: never fetch it.
    let complete = if response.truncated {
        body.rsplit_once('\n').ok_or(())?.0
    } else {
        body
    };
    for line in complete.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            variant = true;
        }
        if let Some(range) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            offset = range.split_once('@').ok_or(())?.1.parse().map_err(|_| ())?;
        }
        if !line.is_empty() && !line.starts_with('#') {
            let url = response.url.join(line).map_err(|_| ())?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(());
            }
            return Ok(Some(Child {
                playlist: variant || is_playlist_url(&url),
                url,
                offset,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
