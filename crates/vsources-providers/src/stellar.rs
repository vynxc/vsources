//! `Stellar`: stellar.gdn's proof-of-work resolve chain.
//!
//! Ports `src/source/Stellar.js` + `src/nuvio/stellar.cjs` (movies/TV/
//! anime with direct HLS up to 4K). The backend gates every request
//! behind a per-call proof-of-work:
//!
//! 1. `GET https://api.stellar.gdn/api/challenge` →
//!    `{challenge, difficulty}`.
//! 2. Solve the `PoW`: find the nonce whose
//!    `SHA-256(challenge + nonce)` hex starts with `difficulty` zeros
//!    (≤ 5 000 000 tries).
//! 3. AES-256-GCM-encrypt the request payload with the key
//!    `SHA-256(STELLAR_GDN_KEY + today's UTC date)` →
//!    `{q: ct, s: iv, t: tag, d: date}`.
//! 4. `POST /api/resolve` (and `/api/subtitles`, with its own fresh
//!    `PoW`) → `{url, source, availableSources, subtitles}` — a direct
//!    HLS master on `cdn.reallyfast.ch` (Orbit) / Nova's worker.
//! 5. Probe each master playlist (the CDN workers hotlink-gate on
//!    `Origin`/`Referer: stellar.gdn`): variant resolutions and
//!    `#EXT-X-MEDIA:TYPE=AUDIO` tracks. Title-level subtitles come
//!    from `/api/subtitles` (the resolve response usually ships an
//!    empty array even when 60–200 multi-language VTTs exist).
//! 6. One card per source (the default plus every
//!    `availableSources` entry), each re-resolved through its own `PoW`;
//!    the wrapper enriches with `[Stellar {server}] {height}p
//!    WEB-DL {codec} {audio}` titles and the hotlink headers, then
//!    [`build_stream_results`] maps them (download cards get their
//!    BluRay/Remux release metadata back in a post-pass, like the JS's
//!    `r.meta` overrides). The whole chain retries once on empty
//!    inside a 34.5 s race (upstream Task 38/59).
//!
//! Cuts for the library port:
//!
//! - The TMDB anime probe is cut (the `TmdbClient` precedent) —
//!   `isAnime` is always false, so the audio label is always
//!   `English` and the base codes `[multi, en]`.
//! - The AES key secret ships as a constant (upstream keeps it in the
//!   site-secrets registry, env-overridable there);
//!   [`Stellar::with_key`] overrides it for embedders.
//! - The workspace ships no RNG crate — the GCM IV is derived from
//!   `SHA-256(challenge + nonce + clock)`, unique per request, which
//!   is GCM's requirement.
//! - `meta.title` becomes [`Stream::label`]; `serverName`/
//!   `audioLabel`/`isMultiAudio` have no `StreamMeta` fields (the
//!   label and language flags carry them). The JS's iframe/
//!   `notWebVideo` filter never fired (the scraper always emits HLS
//!   mime types) and is not ported. No `/proxy`: the hotlink
//!   Origin/Referer ride request headers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{
    BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline,
    with_retry_on_empty,
};

/// The provider id, upstream `this.id`.
const ID: &str = "stellar";
/// The display label, upstream `this.label`.
const LABEL: &str = "Stellar";
/// The site origin, upstream `this.baseUrl`/`STELLAR_GDN`.
const STELLAR_GDN: &str = "https://stellar.gdn";
/// The resolve backend, upstream `BACKEND_URL`.
const BACKEND_URL: &str = "https://api.stellar.gdn";
/// Upstream `this.ttl`.
const TTL: Duration = Duration::from_mins(10);
/// One challenge/subtitles/resolve call (upstream: 10 s/10 s/15 s —
/// collapsed to the resolve value).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The resolve POST timeout (upstream: 15 s).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);
/// The `PoW` nonce ceiling (upstream: 5 000 000).
const POW_LIMIT: u64 = 5_000_000;
/// The retry-on-empty budget (upstream `maxTotalMs: 34000`).
const RETRY_TOTAL: Duration = Duration::from_secs(34);
/// The retry backoff (the upstream default).
const RETRY_BACKOFF: Duration = Duration::from_millis(400);
/// The outer race (the JS `34500` cap).
const SWEEP_DEADLINE: Duration = Duration::from_millis(34_500);
/// The upstream browser UA.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The AES key secret from the site's JS bundle (rotated upstream on
/// 2026-09; override with [`Stellar::with_key`]).
const STELLAR_GDN_KEY: &str = "iwTL6oi-9LLc3M4a1jcQV6jciugKj1_z6dYhdSbbtlg:";

/// `RESOLUTION=(\d+)x(\d+)` — a variant's dimensions.
static RESOLUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"RESOLUTION=(\d+)x(\d+)")
        .unwrap_or_else(|e| panic!("valid resolution pattern: {e}"))
});

/// `NAME="([^"]+)"` — an audio track's name.
static MEDIA_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"NAME="([^"]+)""#).unwrap_or_else(|e| panic!("valid name pattern: {e}"))
});

/// `URI="([^"]+)"` — an audio track's URI.
static MEDIA_URI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"URI="([^"]+)""#).unwrap_or_else(|e| panic!("valid uri pattern: {e}"))
});

/// `(\d{3,4})p?` — the wrapper's `parseHeight` (the `p` is optional).
static HEIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{3,4})p?").unwrap_or_else(|e| panic!("valid height pattern: {e}"))
});

/// `([\d.]+)\s*(GB|MB)` — a file size inside a title.
static SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\d.]+)\s*(GB|MB)").unwrap_or_else(|e| panic!("valid size pattern: {e}"))
});

/// The `Stellar` provider.
pub struct Stellar {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The AES key secret.
    key_secret: String,
}

impl Stellar {
    /// A provider over the shared TMDB client with the bundled key
    /// secret.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self::with_key(tmdb, STELLAR_GDN_KEY)
    }

    /// A provider with an explicit key secret (the upstream
    /// site-secrets env override).
    #[must_use]
    pub fn with_key(tmdb: Arc<TmdbClient>, key_secret: impl Into<String>) -> Self {
        Self {
            info: SourceInfo {
                id: ID.to_string(),
                label: LABEL.to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(STELLAR_GDN).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            key_secret: key_secret.into(),
        }
    }
}

#[async_trait]
impl Source for Stellar {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, &self.tmdb, media).await?;
        let (name, year) = name_and_year(ctx, &self.tmdb, media, tmdb_id).await?;
        let title = display_title(&name, year, media);

        // The bounded retry-on-empty (transient 429 windows) inside the
        // outer race — errors behave like empty sweeps, like the JS.
        let scrape = with_retry_on_empty(
            || async { Some(self.scrape(ctx, media, tmdb_id, &name).await) },
            2,
            RETRY_TOTAL,
            RETRY_BACKOFF,
        );
        let streams = with_deadline(scrape, SWEEP_DEADLINE)
            .await
            .flatten()
            .unwrap_or_default();
        if streams.is_empty() {
            return Err(SourceError::NotFound);
        }

        // The wrapper's enrichment: scraped cards become the final
        // Nuvio streams, with the download metadata kept aside for the
        // post-pass.
        let enriched: Vec<(NuvioStream, Option<(String, String, Option<u64>)>)> =
            streams.iter().map(enrich).collect();
        let nuvio_streams: Vec<NuvioStream> =
            enriched.iter().map(|(stream, _)| stream.clone()).collect();
        let country_codes = vec![CountryCode::Multi, CountryCode::En];
        let built = build_stream_results(&BuildParams {
            streams: &nuvio_streams,
            title: &title,
            source_id: ID,
            source_label: LABEL,
            country_codes: &country_codes,
            ttl: TTL,
        });
        // The wrapper's post-pass: download cards get their release
        // metadata back (the JS `r.meta` overrides).
        Ok(built
            .into_iter()
            .zip(enriched)
            .map(|(mut stream, (_, download))| {
                if let Some((source_type, codec, size)) = download {
                    stream.meta.quality = Some(source_type);
                    stream.meta.codec = Some(codec);
                    stream.meta.size = size;
                }
                stream
            })
            .collect())
    }
}

impl Stellar {
    /// The scraper chain — the port of `stellar.cjs getStreams` (the
    /// default source plus every `availableSources` entry, each
    /// re-resolved through its own `PoW`, sorted 4K-first). Any error
    /// answers empty (the JS catch).
    async fn scrape(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        name: &str,
    ) -> Vec<NuvioStream> {
        let media_type = if media.season.is_some() {
            "tv"
        } else {
            "movie"
        };
        let Ok(result) = self
            .resolve_stream_url(ctx, media_type, tmdb_id, media.season, media.episode, None)
            .await
        else {
            return Vec::new();
        };
        if result.url.is_empty() {
            return Vec::new();
        }

        let probe = probe_master_playlist(ctx, &result.url).await;
        let quality = probe
            .as_ref()
            .map_or_else(|| "1080p".to_string(), |probe| probe.quality.clone());
        // Subtitles: the resolve response's array when populated, the
        // separate endpoint otherwise (it always needs a fresh PoW).
        let mut subtitles = result.subtitles.clone();
        if subtitles.is_empty() {
            subtitles = self
                .fetch_subtitles(ctx, media_type, tmdb_id, media.season, media.episode)
                .await;
        }

        let mut streams = vec![scraped_stream(
            name,
            result.source.as_deref().unwrap_or("Default"),
            &result.url,
            &quality,
            probe.as_ref(),
            &subtitles,
        )];
        for source in &result.available_sources {
            if Some(source) == result.source.as_ref() {
                continue;
            }
            let Ok(alt) = self
                .resolve_stream_url(
                    ctx,
                    media_type,
                    tmdb_id,
                    media.season,
                    media.episode,
                    Some(source),
                )
                .await
            else {
                continue;
            };
            if alt.url.is_empty() {
                continue;
            }
            let alt_probe = probe_master_playlist(ctx, &alt.url).await;
            let alt_quality = alt_probe
                .as_ref()
                .map_or_else(|| "1080p".to_string(), |probe| probe.quality.clone());
            let alt_subs = if alt.subtitles.is_empty() {
                subtitles.clone()
            } else {
                alt.subtitles
            };
            streams.push(scraped_stream(
                name,
                source,
                &alt.url,
                &alt_quality,
                alt_probe.as_ref(),
                &alt_subs,
            ));
        }

        // Sort by quality (4K first).
        streams.sort_by_key(|stream| quality_order(stream.quality.as_deref().unwrap_or("")));
        streams
    }

    /// The challenge → `PoW` → encrypt → POST resolve chain — the port
    /// of `resolveStreamUrl`.
    async fn resolve_stream_url(
        &self,
        ctx: &ResolveCtx<'_>,
        media_type: &str,
        tmdb_id: u64,
        season: Option<u32>,
        episode: Option<u32>,
        source: Option<&str>,
    ) -> Result<ResolveOutcome, StellarError> {
        let challenge = self.fetch_challenge(ctx).await?;
        let nonce =
            solve_pow(&challenge.challenge, challenge.difficulty).ok_or(StellarError::Pow)?;

        let mut payload = serde_json::json!({
            "mediaType": media_type,
            "id": tmdb_id,
            "challenge": challenge.challenge,
            "nonce": nonce,
        });
        if let Some(season) = season {
            payload["season"] = Value::from(season);
        }
        if let Some(episode) = episode {
            payload["episode"] = Value::from(episode);
        }
        if let Some(source) = source {
            payload["source"] = Value::from(source);
        }
        let cipher = encrypt_payload(
            &self.key_secret,
            &payload.to_string(),
            &challenge.challenge,
            &nonce,
        );

        let url =
            Url::parse(&format!("{BACKEND_URL}/api/resolve")).map_err(|_| StellarError::Http)?;
        let request = FetchRequest::post(
            url,
            serde_json::to_string(&cipher).map_err(|_| StellarError::Http)?,
        )
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", UA)
        .with_header("Origin", STELLAR_GDN)
        .with_header("Referer", format!("{STELLAR_GDN}/"))
        .with_timeout(RESOLVE_TIMEOUT);
        let response = ctx.fetcher.request(request).await?;
        if !response.is_success() {
            return Err(StellarError::Http);
        }
        let outcome: ResolveOutcome = response.json().map_err(|_| StellarError::Http)?;
        Ok(outcome)
    }

    /// One challenge fetch.
    async fn fetch_challenge(&self, ctx: &ResolveCtx<'_>) -> Result<Challenge, StellarError> {
        let url =
            Url::parse(&format!("{BACKEND_URL}/api/challenge")).map_err(|_| StellarError::Http)?;
        let request = FetchRequest::get(url)
            .with_header("User-Agent", UA)
            .with_header("Origin", STELLAR_GDN)
            .with_header("Referer", format!("{STELLAR_GDN}/"))
            .with_header("Accept", "application/json")
            .with_timeout(REQUEST_TIMEOUT);
        let response = ctx.fetcher.request(request).await?;
        if !response.is_success() {
            return Err(StellarError::Http);
        }
        response.json().map_err(|_| StellarError::Http)
    }

    /// The subtitles endpoint (its own fresh `PoW`) — the port of
    /// `fetchSubtitles`; failures answer empty.
    async fn fetch_subtitles(
        &self,
        ctx: &ResolveCtx<'_>,
        media_type: &str,
        tmdb_id: u64,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Vec<ApiSubtitle> {
        let Ok(challenge) = self.fetch_challenge(ctx).await else {
            return Vec::new();
        };
        let Some(nonce) = solve_pow(&challenge.challenge, challenge.difficulty) else {
            return Vec::new();
        };
        let mut payload = serde_json::json!({
            "mediaType": media_type,
            "id": tmdb_id,
            "challenge": challenge.challenge,
            "nonce": nonce,
        });
        if let Some(season) = season {
            payload["season"] = Value::from(season);
        }
        if let Some(episode) = episode {
            payload["episode"] = Value::from(episode);
        }
        let Ok(url) = Url::parse(&format!("{BACKEND_URL}/api/subtitles")) else {
            return Vec::new();
        };
        let request = FetchRequest::post(url, payload.to_string())
            .with_header("Content-Type", "application/json")
            .with_header("User-Agent", UA)
            .with_header("Origin", STELLAR_GDN)
            .with_header("Referer", format!("{STELLAR_GDN}/"))
            .with_timeout(RESOLVE_TIMEOUT);
        let Ok(response) = ctx.fetcher.request(request).await else {
            return Vec::new();
        };
        if !response.is_success() {
            return Vec::new();
        }
        let Ok(payload) = serde_json::from_str::<SubtitlesResponse>(&response.body) else {
            return Vec::new();
        };
        payload.subtitles
    }
}

/// The challenge response.
#[derive(Deserialize)]
struct Challenge {
    /// The challenge string.
    challenge: String,
    /// The required hex-zero prefix length.
    difficulty: usize,
}

/// The resolve response.
#[derive(Deserialize)]
struct ResolveOutcome {
    /// The direct stream URL.
    #[serde(default)]
    url: String,
    /// The winning source name.
    #[serde(default)]
    source: Option<String>,
    /// The other sources to try.
    #[serde(rename = "availableSources", default)]
    available_sources: Vec<String>,
    /// Subtitles on the resolve response (usually empty).
    #[serde(default)]
    subtitles: Vec<ApiSubtitle>,
}

/// One title-level subtitle.
#[derive(Debug, Clone, Deserialize)]
struct ApiSubtitle {
    /// The language code.
    #[serde(default)]
    language: Option<String>,
    /// The display label.
    #[serde(default)]
    label: Option<String>,
    /// The VTT URL.
    #[serde(default)]
    url: Option<String>,
}

impl ApiSubtitle {
    /// The `{id, url, lang}` track the scraper attaches.
    fn track(&self) -> Option<NuvioSubtitle> {
        let url = self.url.clone()?;
        let id = self.language.clone().unwrap_or_else(|| "en".to_string());
        let lang = self
            .label
            .clone()
            .or_else(|| self.language.clone())
            .unwrap_or_else(|| "English".to_string());
        Some(NuvioSubtitle {
            id: Some(id),
            url: Some(url),
            lang: Some(lang),
            ..NuvioSubtitle::default()
        })
    }
}

/// The subtitles endpoint envelope.
#[derive(Deserialize)]
struct SubtitlesResponse {
    /// The subtitle list.
    #[serde(default)]
    subtitles: Vec<ApiSubtitle>,
}

/// The master-playlist probe — the port of `probeMasterPlaylist`
/// (variant resolutions + audio tracks; the CDN hotlink gate means the
/// probe carries `Origin`/`Referer: stellar.gdn`).
#[derive(Debug, Clone)]
struct Probe {
    /// The best variant's quality (`2160p`/`1080p`/`720p`/`SD`).
    quality: String,
    /// The `#EXT-X-MEDIA:TYPE=AUDIO` tracks (`{name, url, default}`).
    audio_tracks: Vec<Value>,
}

/// The encrypted request envelope `{q, s, t, d}`.
#[derive(serde::Serialize)]
struct StellarCipher {
    /// The base64 ciphertext.
    q: String,
    /// The base64 IV.
    s: String,
    /// The base64 auth tag.
    t: String,
    /// The key-derivation date.
    d: String,
}

/// Why the resolve chain failed.
#[derive(Debug)]
enum StellarError {
    /// A `PoW` that did not converge.
    Pow,
    /// An HTTP/JSON failure.
    Http,
}

impl From<FetchError> for StellarError {
    fn from(_: FetchError) -> Self {
        Self::Http
    }
}

/// Solve the `PoW` — `SHA-256(challenge + nonce)` hex starting with
/// `difficulty` zeros, capped at [`POW_LIMIT`] tries.
fn solve_pow(challenge: &str, difficulty: usize) -> Option<String> {
    let target = "0".repeat(difficulty);
    for nonce in 0..=POW_LIMIT {
        let digest = Sha256::digest(format!("{challenge}{nonce}").as_bytes());
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(hex, "{byte:02x}");
        }
        if hex.starts_with(&target) {
            return Some(nonce.to_string());
        }
    }
    None
}

/// The AES-256-GCM key — `SHA-256(secret + date)`.
fn derive_key(secret: &str, date: &str) -> [u8; 32] {
    let digest: [u8; 32] = Sha256::digest(format!("{secret}{date}").as_bytes()).into();
    digest
}

/// Encrypt the payload with a per-request IV — the port of
/// `encryptPayload` (the IV is derived from the challenge material and
/// the clock because the workspace ships no RNG crate).
fn encrypt_payload(secret: &str, payload: &str, challenge: &str, nonce: &str) -> StellarCipher {
    let date = utc_today();
    let iv_source = Sha256::digest(format!("{challenge}{nonce}{}", nanos()).as_bytes());
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&iv_source[..12]);
    encrypt_with_iv(secret, payload, &date, &iv)
}

/// Encrypt with an explicit IV and date — the ground-truth-testable
/// core of [`encrypt_payload`].
fn encrypt_with_iv(secret: &str, payload: &str, date: &str, iv: &[u8; 12]) -> StellarCipher {
    let key = derive_key(secret, date);
    let (ciphertext, tag) = gcm::encrypt(&key, iv, payload.as_bytes())
        .unwrap_or_else(|| panic!("the GCM encryption of a small payload cannot fail"));
    StellarCipher {
        q: STANDARD.encode(&ciphertext),
        s: STANDARD.encode(iv),
        t: STANDARD.encode(tag),
        d: date.to_string(),
    }
}

/// The monotonic clock — IV uniqueness material.
fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

/// Today's UTC date as `YYYY-MM-DD` (the JS
/// `new Date().toISOString().slice(0, 10)`).
fn utc_today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or_default();
    let days = i64::try_from(days).unwrap_or_default();
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch → the civil date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Probe a master playlist — variant resolutions and audio tracks.
async fn probe_master_playlist(ctx: &ResolveCtx<'_>, url: &str) -> Option<Probe> {
    let url = Url::parse(url).ok()?;
    let request = FetchRequest::get(url)
        .with_header("User-Agent", UA)
        .with_header("Origin", STELLAR_GDN)
        .with_header("Referer", format!("{STELLAR_GDN}/"))
        .with_timeout(REQUEST_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }

    let mut variants: Vec<(u32, u32, u64)> = Vec::new();
    let mut audio_tracks: Vec<Value> = Vec::new();
    for line in response.body.lines() {
        if line.starts_with("#EXT-X-STREAM-INF") {
            if let Some((width, height)) =
                RESOLUTION
                    .captures(line)
                    .ok()
                    .flatten()
                    .and_then(|captures| {
                        Some((
                            captures.get(1)?.as_str().parse().ok()?,
                            captures.get(2)?.as_str().parse().ok()?,
                        ))
                    })
            {
                variants.push((width, height, 0));
            }
        } else if line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") {
            let name = MEDIA_NAME
                .captures(line)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map(|group| group.as_str().to_string());
            let uri = MEDIA_URI
                .captures(line)
                .ok()
                .flatten()
                .and_then(|captures| captures.get(1))
                .map(|group| group.as_str().to_string());
            if let (Some(name), Some(uri)) = (name, uri) {
                audio_tracks.push(serde_json::json!({
                    "name": name,
                    "url": uri,
                    "default": line.contains("DEFAULT=YES"),
                }));
            }
        }
    }
    if variants.is_empty() {
        return None;
    }
    // The best variant by resolution (the JS sorts by bandwidth; the
    // fixtures carry none).
    let best = variants
        .iter()
        .max_by_key(|(width, height, _)| width.max(height))?;
    let max_dimension = best.0.max(best.1);
    let quality = if max_dimension >= 3840 {
        "2160p"
    } else if max_dimension >= 1920 {
        "1080p"
    } else if max_dimension >= 1280 {
        "720p"
    } else {
        "SD"
    };
    Some(Probe {
        quality: quality.to_string(),
        audio_tracks,
    })
}

/// Build the scraper-shaped stream for one source — the port of the
/// `.cjs` `buildStream` (the name/title/quality/subtitles/audioTracks
/// the wrapper re-enriches).
#[allow(clippy::too_many_arguments)]
fn scraped_stream(
    title: &str,
    server: &str,
    url: &str,
    quality: &str,
    probe: Option<&Probe>,
    subtitles: &[ApiSubtitle],
) -> NuvioStream {
    let is_4k = quality == "2160p";
    let res_str = probe
        .map(|probe| format!(" {}", probe.quality))
        .unwrap_or_default();
    let mut stream = NuvioStream::new(url.to_string())
        .with_quality(quality.to_string())
        .with_name(format!(
            "Stellar - {server}{}",
            if is_4k { " 4K" } else { "" }
        ))
        .with_title(format!(
            "{title} [Stellar {server}{res_str}{}]",
            if is_4k { " 4K" } else { "" }
        ))
        .with_kind("application/vnd.apple.mpegurl");
    for subtitle in subtitles {
        if let Some(track) = subtitle.track() {
            stream = stream.with_subtitle(track);
        }
    }
    if let Some(probe) = probe.filter(|probe| !probe.audio_tracks.is_empty()) {
        stream.audio_tracks = Some(Value::Array(probe.audio_tracks.clone()));
    }
    stream
}

/// The wrapper's enrichment of one scraped stream — the port of
/// `Stellar.js`'s mapping (server name, height, codec/sourceType/hdr
/// from the label text, download detection, hotlink headers). Returns
/// the stream plus the download-card metadata for the post-pass.
fn enrich(scraped: &NuvioStream) -> (NuvioStream, Option<(String, String, Option<u64>)>) {
    let server = scraped
        .name
        .as_deref()
        .unwrap_or_default()
        .trim_start_matches("Stellar - ")
        .trim()
        .to_string();
    let quality = scraped.quality.as_deref().unwrap_or_default();
    let height = stellar_height(quality).unwrap_or(1080);
    let label_text = format!(
        "{} {}",
        scraped.name.as_deref().unwrap_or_default(),
        scraped.title.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    let is_download = label_text.contains("dl ");

    let mut codec = "x264".to_string();
    let mut source_type = "WebDL".to_string();
    if label_text.contains("h265") || label_text.contains("hevc") || label_text.contains("x265") {
        codec = "HEVC".to_string();
    } else if label_text.contains("remux") {
        codec = "AVC".to_string();
    }
    if label_text.contains("bluray") || label_text.contains("remux") || label_text.contains("bdrip")
    {
        source_type = if label_text.contains("remux") {
            "BluRay Remux".to_string()
        } else {
            "BluRay".to_string()
        };
    }
    let hdr = if label_text.contains("dolby vision") || label_text.contains(" dv ") {
        " DolbyVision"
    } else if label_text.contains("hdr10+") {
        " HDR10+"
    } else if label_text.contains("hdr") {
        " HDR"
    } else {
        ""
    };

    let file_size = SIZE
        .captures(scraped.title.as_deref().unwrap_or_default())
        .ok()
        .flatten()
        .and_then(|captures| {
            let value: f64 = captures.get(1)?.as_str().parse().ok()?;
            let unit = captures.get(2)?.as_str().to_ascii_uppercase();
            Some(match unit.as_str() {
                "GB" => size_bytes(value * 1024.0 * 1024.0 * 1024.0),
                _ => size_bytes(value * 1024.0 * 1024.0),
            })
        });

    // The audio label is always English (the anime probe is cut).
    let audio_label = "English";
    let stream_type = if is_download {
        source_type.as_str()
    } else {
        "WEB-DL"
    };
    let mut stream = NuvioStream::new(scraped.url.clone())
        .with_quality(if quality.is_empty() {
            format!("{height}p")
        } else {
            quality.to_string()
        })
        .with_name(format!("Stellar - {server}"))
        .with_title(format!(
            "[Stellar {server}] {height}p {stream_type} {codec}{hdr} {audio_label}"
        ));
    // The hotlink gate: every stream the scraper emits carries the
    // HLS mime type, so the headers apply to downloads too.
    stream = stream
        .with_header("Referer", format!("{STELLAR_GDN}/"))
        .with_header("Origin", STELLAR_GDN);
    stream.subtitles.clone_from(&scraped.subtitles);
    stream.audio_tracks.clone_from(&scraped.audio_tracks);

    let download = is_download.then_some((source_type, codec, file_size));
    (stream, download)
}

/// Round a parsed file size to bytes, saturating — a malformed label
/// cannot fabricate a size (the JS `Math.round`).
///
/// The float casts are the point: the label is untrusted text, so the
/// clamp bounds every degenerate value.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn size_bytes(value: f64) -> u64 {
    if value <= 0.0 {
        return 0;
    }
    value.round().min(u64::MAX as f64).max(0.0) as u64
}

/// The wrapper's height parse: `4k`/`2160` → 2160, `1440`, then the
/// optional-`p` 3-4 digit form.
fn stellar_height(quality: &str) -> Option<u16> {
    let lower = quality.to_ascii_lowercase();
    if lower.contains("4k") || lower.contains("2160") {
        return Some(2160);
    }
    if lower.contains("1440") {
        return Some(1440);
    }
    HEIGHT
        .captures(&lower)
        .ok()
        .flatten()
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
}

/// The 4K-first sort order.
fn quality_order(quality: &str) -> u8 {
    match quality {
        "2160p" => 0,
        "1080p" => 1,
        "720p" => 2,
        "480p" => 3,
        "SD" => 4,
        _ => 9,
    }
}

/// `name + (season ? ` S01E02` : ` (${year})`)` — the display title.
fn display_title(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    if media.season.is_some() {
        format!("{} {}", name, media.format_season_and_episode())
    } else {
        year.map_or_else(|| format!("{name} ()"), |year| format!("{name} ({year})"))
    }
}

/// The TMDB id: IMDb-keyed references resolve through the pre-resolved
/// context media or `/find` — ports `getTmdbId`.
async fn tmdb_id(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear`.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    tmdb: &TmdbClient,
    media: &MediaRef,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
    Ok((name.name, name.year))
}

/// Map miss-shaped failures onto the not-found answer.
fn soften<T>(error: Result<T, SourceError>) -> Result<T, SourceError> {
    match error {
        Err(
            SourceError::NotFound
            | SourceError::Fetch(FetchError::NotFound { .. } | FetchError::Http { status: 404, .. }),
        ) => Err(SourceError::NotFound),
        other => other,
    }
}

/// AES-256-GCM encryption over the raw `aes` block cipher — the same
/// minimal SP 800-38D construction the workspace's other GCM users
/// (the Vidzee extractor, `nuvio::vidstorm`) implement privately.
mod gcm {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::{Aes256, Block};

    /// The GHASH reduction constant `R = E1 ‖ 0^120`.
    const R: u128 = 0xE1 << 120;

    /// Encrypt `plaintext` under `key` with a 96-bit `iv`, returning
    /// the ciphertext and the 16-byte auth tag.
    pub(super) fn encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Option<(Vec<u8>, [u8; 16])> {
        if key.len() != 32 || iv.len() != 12 {
            return None;
        }
        let cipher = Aes256::new(GenericArray::from_slice(key));

        // J0 = IV ‖ 0^31 ‖ 1 (the 96-bit-IV case).
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;

        let mut ciphertext = plaintext.to_vec();
        let mut counter = j0;
        for chunk in ciphertext.chunks_mut(16) {
            increment_counter(&mut counter);
            let keystream = e_k(&cipher, &counter);
            for (byte, ks) in chunk.iter_mut().zip(keystream) {
                *byte ^= ks;
            }
        }
        // The tag authenticates the ciphertext (GHASH runs over it,
        // never the plaintext).
        let h = e_k(&cipher, &[0u8; 16]);
        let tag = xor(&e_k(&cipher, &j0), &ghash(h, &ciphertext));
        Some((ciphertext, tag))
    }

    /// `E_K(block)` — one AES-256 block encryption.
    fn e_k(cipher: &Aes256, bytes: &[u8; 16]) -> [u8; 16] {
        let mut block = Block::clone_from_slice(bytes);
        cipher.encrypt_block(&mut block);
        let mut out = [0u8; 16];
        out.copy_from_slice(&block);
        out
    }

    /// GHASH with empty associated data.
    fn ghash(h: [u8; 16], data: &[u8]) -> [u8; 16] {
        let h = u128::from_be_bytes(h);
        let mut y: u128 = 0;
        let mut block = [0u8; 16];
        for chunk in data.chunks(16) {
            block.fill(0);
            block[..chunk.len()].copy_from_slice(chunk);
            y = gmul(y ^ u128::from_be_bytes(block), h);
        }
        block.fill(0);
        block[8..].copy_from_slice(&u64::try_from(data.len() * 8).unwrap_or(0).to_be_bytes());
        gmul(y ^ u128::from_be_bytes(block), h).to_be_bytes()
    }

    /// Multiplication in GF(2^128) with the GCM polynomial.
    fn gmul(x: u128, y: u128) -> u128 {
        let mut z: u128 = 0;
        let mut v = x;
        for bit in (0..128).rev() {
            if (y >> bit) & 1 == 1 {
                z ^= v;
            }
            let lsb = v & 1;
            v >>= 1;
            if lsb == 1 {
                v ^= R;
            }
        }
        z
    }

    /// `inc32`: increment the big-endian last 32 bits.
    fn increment_counter(counter: &mut [u8; 16]) {
        let value = u32::from_be_bytes([counter[12], counter[13], counter[14], counter[15]]);
        let incremented = value.wrapping_add(1);
        counter[12..].copy_from_slice(&incremented.to_be_bytes());
    }

    /// XOR two 16-byte blocks.
    fn xor(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (index, byte) in a.iter().enumerate() {
            out[index] = byte ^ b[index];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher};
    use vsources_core::types::{Format, MediaId};

    use super::*;

    /// A fetcher serving canned bodies keyed by URL path (or
    /// `path?query`) in call order — the last body repeats — recording
    /// every request. Query-bearing lookups fall back to the bare path,
    /// so TMDB requests (`?api_key=…`) can be scripted by path alone.
    #[derive(Default)]
    struct ScriptedFetcher {
        pages: Mutex<HashMap<String, Vec<(u16, String)>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// Serve `key` with `status`/`body`; earlier registrations pop
        /// first.
        fn page(self, key: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.into())
                .or_default()
                .push((status, body.into()));
            self
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = match request.url.query() {
                Some(query) => format!("{}?{query}", request.url.path()),
                None => request.url.path().to_string(),
            };
            let entry = {
                let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
                pages.get_mut(&key).map(|bodies| {
                    if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0].clone()
                    }
                })
            };
            let entry = match entry {
                Some(entry) => Some(entry),
                None => self
                    .pages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_mut(request.url.path())
                    .map(|bodies| {
                        if bodies.len() > 1 {
                            bodies.remove(0)
                        } else {
                            bodies[0].clone()
                        }
                    }),
            };
            let Some((status, body)) = entry else {
                return Err(FetchError::NotFound { url: request.url });
            };
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body,
            })
        }
    }

    /// The provider over a TMDB client sharing the scripted fetcher.
    fn provider(mock: &Arc<ScriptedFetcher>) -> Stellar {
        Stellar::new(Arc::new(TmdbClient::new("test-key", mock.clone())))
    }

    /// A context over the scripted fetcher.
    fn ctx_for(mock: &Arc<ScriptedFetcher>) -> ResolveCtx<'_> {
        let fetcher: &dyn Fetcher = mock.as_ref();
        ResolveCtx {
            fetcher,
            media: None,
            source_id: None,
            referer: None,
        }
    }

    /// The fixture media (Inception, TMDB 27205).
    const TMDB_ID: u64 = 27205;

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let info = provider.info();
        assert_eq!(info.id, "stellar");
        assert_eq!(info.label, "Stellar");
        assert_eq!(
            info.content_types,
            vec![MediaType::Movie, MediaType::Series]
        );
        assert_eq!(
            info.country_codes,
            vec![CountryCode::Multi, CountryCode::En]
        );
        assert_eq!(
            info.base_url.as_ref().map(Url::as_str),
            Some("https://stellar.gdn/")
        );
        assert_eq!(info.priority, 0);
        assert_eq!(info.domain_key, None);
    }

    #[test]
    fn solves_the_proof_of_work() {
        let nonce = solve_pow("abc", 1).unwrap_or_else(|| panic!("a difficulty-1 PoW converges"));
        let digest = Sha256::digest(format!("abc{nonce}").as_bytes());
        let hex = hex_of(&digest);
        assert!(hex.starts_with('0'));
        // A difficulty-2 PoW also converges quickly.
        let nonce = solve_pow("abc", 2).unwrap_or_else(|| panic!("a difficulty-2 PoW converges"));
        let digest = Sha256::digest(format!("abc{nonce}").as_bytes());
        let hex = hex_of(&digest);
        assert!(hex.starts_with("00"));
    }

    /// Ground truth: encrypting the payload with the bundled key
    /// secret, date `2026-09-25`, and IV `00112233445566778899aabb`
    /// produces these `q`/`s`/`t` values (Node's `aes-256-gcm`).
    #[test]
    fn encrypts_the_ground_truth_payload() {
        let payload =
            r#"{"mediaType":"movie","id":27205,"challenge":"fix-challenge-token","nonce":"17"}"#;
        let iv: [u8; 12] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
        ];
        let cipher = encrypt_with_iv(STELLAR_GDN_KEY, payload, "2026-09-25", &iv);
        assert_eq!(
            cipher.q,
            "RBdrjucUi3UZR0of0u8zC+rkyEI4wGxe8qYCq99dp+EEWGFT8LADeeVH/EaAhHFNgeaXZQPPMmJnnhiiNm5nV/5AXSDkTxxqJQq7pRkmDA=="
        );
        assert_eq!(cipher.s, "ABEiM0RVZneImaq7");
        assert_eq!(cipher.t, "MY+Cryh6hmwi6bYtXC7Q8Q==");
        assert_eq!(cipher.d, "2026-09-25");
    }

    #[test]
    fn derives_the_utc_date() {
        let today = utc_today();
        assert_eq!(today.len(), 10, "YYYY-MM-DD");
        assert!(
            today
                .chars()
                .enumerate()
                .all(|(index, character)| if index == 4 || index == 7 {
                    character == '-'
                } else {
                    character.is_ascii_digit()
                })
        );
        // The civil-date conversion anchors: the epoch and a known
        // leap day.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(21_742), (2029, 7, 12));
    }

    /// Lowercase hex of a digest — the shared shape of the `PoW` tests.
    fn hex_of(digest: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Every source contributes a card; the walkthrough is one linear
    /// script over the scripted fetcher.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolves_streams_from_every_source() -> Result<(), SourceError> {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/movie/{TMDB_ID}"),
                    200,
                    r#"{"title":"Inception","release_date":"2010-07-16"}"#,
                )
                // The challenge endpoint (repeated per PoW).
                .page("/api/challenge", 200, r#"{"challenge":"pow-challenge","difficulty":1}"#)
                // The default source (Orbit).
                .page(
                    "/api/resolve",
                    200,
                    r#"{"url":"https://cdn.reallyfast.ch/movie/master.m3u8","source":"Orbit","availableSources":["Orbit","Nova","DL Remux"],"subtitles":[]}"#,
                )
                // Orbit's master playlist (variants + audio tracks).
                .page(
                    "/movie/master.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nv1080.m3u8\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio 1\",DEFAULT=YES,URI=\"a1.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio 2\",DEFAULT=NO,URI=\"a2.m3u8\"\n",
                )
                // The separate subtitles endpoint.
                .page(
                    "/api/subtitles",
                    200,
                    r#"{"subtitles":[{"language":"en","label":"English","url":"https://cache.vdrk.site/en.vtt"}]}"#,
                )
                // Nova (an alternate source).
                .page(
                    "/api/resolve",
                    200,
                    r#"{"url":"https://h.themepark.workers.dev/hls/nova.m3u8","source":"Nova","availableSources":[],"subtitles":[]}"#,
                )
                .page(
                    "/hls/nova.m3u8",
                    200,
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=15000000,RESOLUTION=3840x2160\nv2160.m3u8\n",
                )
                // The download source (a Remux MKV).
                .page(
                    "/api/resolve",
                    200,
                    r#"{"url":"https://cdn.reallyfast.ch/files/movie-remux.mkv","source":"DL Remux","availableSources":[],"subtitles":[]}"#,
                ),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let streams = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await?;

        // Orbit, Nova, and the download card (probes of the download
        // URL fail — an MKV is not an m3u8 — so it ships unprobed).
        assert_eq!(streams.len(), 3);
        // 4K first (Nova's 2160p probe).
        assert_eq!(streams[0].meta.resolution, Some(2160));
        assert!(streams[0].label.as_deref().is_some_and(|label| {
            // The scraper appends `4K` to 2160p server names and the
            // wrapper keeps it.
            label.contains("Inception (2010) — [Stellar Nova 4K] 2160p WEB-DL x264 English")
        }));
        // The Orbit card carries the hotlink headers and the shared
        // subtitle.
        let orbit = streams
            .iter()
            .find(|stream| stream.url.as_str() == "https://cdn.reallyfast.ch/movie/master.m3u8")
            .unwrap_or_else(|| panic!("the Orbit card exists"));
        assert_eq!(orbit.format, Format::Hls);
        assert_eq!(
            orbit
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://stellar.gdn/")
        );
        assert_eq!(
            orbit.meta.request_headers.get("Origin").map(String::as_str),
            Some("https://stellar.gdn")
        );
        assert_eq!(orbit.meta.subtitles.len(), 1);
        assert_eq!(
            orbit.meta.subtitles[0].url.as_str(),
            "https://cache.vdrk.site/en.vtt"
        );
        // The download card gets its release metadata back.
        let download = streams
            .iter()
            .find(|stream| {
                std::path::Path::new(stream.url.path())
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv"))
            })
            .unwrap_or_else(|| panic!("the download card exists"));
        assert_eq!(download.format, Format::Mp4);
        assert_eq!(download.meta.quality.as_deref(), Some("BluRay Remux"));
        assert_eq!(download.meta.codec.as_deref(), Some("AVC"));
        assert_eq!(download.meta.resolution, Some(1080));
        // The resolve chain POSTed three encrypted payloads (default +
        // two alternates) with the `{q,s,t,d}` shape.
        let posts: Vec<FetchRequest> = mock
            .requests()
            .into_iter()
            .filter(|request| request.url.path() == "/api/resolve")
            .collect();
        assert_eq!(posts.len(), 3);
        for post in posts {
            let body: Value = serde_json::from_str(post.body.as_deref().unwrap_or_default())
                .unwrap_or_else(|error| panic!("the resolve POST body is JSON: {error}"));
            for key in ["q", "s", "t", "d"] {
                assert!(
                    body.get(key).is_some_and(Value::is_string),
                    "the {key} field"
                );
            }
            assert_eq!(
                body.get("d").and_then(Value::as_str),
                Some(utc_today().as_str())
            );
        }
        assert!(
            streams
                .iter()
                .all(|stream| stream.meta.source_id.as_deref() == Some("stellar"))
        );
        assert_eq!(streams[0].ttl, TTL);
        Ok(())
    }

    #[tokio::test]
    async fn a_failing_resolve_retries_then_answers_not_found() {
        let mock = Arc::new(
            ScriptedFetcher::default()
                .page(
                    format!("/3/movie/{TMDB_ID}"),
                    200,
                    r#"{"title":"Inception","release_date":"2010-07-16"}"#,
                )
                .page(
                    "/api/challenge",
                    200,
                    r#"{"challenge":"pow-challenge","difficulty":1}"#,
                )
                .page("/api/resolve", 503, "{}"),
        );
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB_ID)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
        // The initial attempt plus the retry-on-empty.
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|request| request.url.path() == "/api/resolve")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_tmdb_miss_is_not_found() {
        let mock = Arc::new(ScriptedFetcher::default());
        let provider = provider(&mock);
        let ctx = ctx_for(&mock);

        let result = provider
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(404)))
            .await;

        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[test]
    fn parses_heights_and_quality_order() {
        assert_eq!(stellar_height("2160p"), Some(2160));
        assert_eq!(stellar_height("4K"), Some(2160));
        assert_eq!(stellar_height("1440p"), Some(1440));
        assert_eq!(stellar_height("1080p"), Some(1080));
        assert_eq!(stellar_height("SD"), None);
        assert_eq!(quality_order("2160p"), 0);
        assert_eq!(quality_order("SD"), 4);
    }
}
