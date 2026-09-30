//! `Atlantic`: the `atlantic.st` stream servers (`hls.lol` family).
//!
//! Ports `src/source/Atlantic.js` + its Nuvio scraper
//! `src/nuvio/atlantic.cjs` (the Task 86 protocol):
//!
//! Flow:
//!
//! 1. Resolve the TMDB id, name/year, and best-effort `IMDb` id (the
//!    context media or [`TmdbClient`]).
//! 2. **Helios** (the Artemis replacement): `GET
//!    stream.hls.lol/helios?tmdbId&type[&seasonId&episodeId]` →
//!    `{sources: {Moscow, Novo, Omsk}}` — each `url` either plain or
//!    an `ns_<hex>` "nesterov" payload (AES-256-GCM with the inline
//!    bundle key) decrypting to a payload-worker master. Candidates
//!    pointing back at the SPA origin are decoys and drop. The first
//!    candidate whose master answers `#EXTM3U` wins.
//! 3. **Aphrodite** (curated 4K): the `aphrodite.a.v1` gate — a
//!    session bootstrapped by `POST cdn.hls.lol/content/index` with
//!    `HMAC-SHA256(masterKey, "a|ts|nonce")`, then per-request
//!    `X-A-Sid/Ts/Nonce/Sig` headers signed over the path. A `401` /
//!    `403` / `renew: true` resets the session and retries once; a
//!    dead gate falls back to the unsigned GET. The answer's title
//!    must match the requested one (the unsigned path serves decoy
//!    catalog entries).
//! 4. The winning master is parsed (`#EXT-X-STREAM-INF` variants,
//!    `#EXT-X-MEDIA` audio groups): separated-audio and non-muxed
//!    masters ship **as the master** (players need the audio-group
//!    context); fully muxed masters ship per-variant cards (top four
//!    heights, validated). Flat media playlists validate their first
//!    segment instead. Cards are dropped unless they validate —
//!    upstream ships nothing it has not seen answer.
//! 5. The cards (the server name and quality in the title, the full
//!    browser header set the payload workers require) run through
//!    `build_stream_results`,
//!    sorted 4K-first.
//!
//! Cuts for the library port:
//!
//! - Upstream wrapped the payload-worker URLs in its addon `/proxy`
//!   (injecting `Origin`/`Referer` upstream); there is no server, so
//!   the same headers ride
//!   [`StreamMeta::request_headers`](vsources_core::types::StreamMeta::request_headers)
//!   — what the player must send instead.
//! - The TMDB `original_language` audio flag is cut (the shared
//!   [`TmdbClient`] does not expose it) — the card languages are the
//!   `[multi]` default.
//! - The scraper's own inline subtitle stage was already removed
//!   upstream (Task 55); the resolver-level unified stack is the
//!   parent's domain.
//! - The 30 min gate-session cache: this port bootstraps per resolve
//!   (one extra POST inside the TTL window; the renew/retry flow is
//!   preserved).
//! - The 10.5 s shared deadline wraps each chain; segment probes use
//!   the context fetcher (the JS used a bare streaming `fetch` — the
//!   fetcher contract carries whole text bodies, and the magic-byte
//!   checks survive the lossy decode: `0x47` is `'G'`, the `ftyp`
//!   box magic is ASCII).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use vsources_core::error::SourceError;
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{FetchRequest, ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::{
    BuildParams, NuvioStream, build_stream_results, with_deadline, with_retry_on_empty,
};

/// The SPA origin.
const ORIGIN: &str = "https://atlantic.st";
/// The hotlink `Referer` — the origin with a trailing slash.
const REFERER: &str = "https://atlantic.st/";
/// The Aphrodite CDN.
const CDN: &str = "https://cdn.hls.lol";
/// The Helios API.
const HELIOS: &str = "https://stream.hls.lol";
/// The nesterov AES-256-GCM key (verbatim from the bundle).
const NESTEROV_KEY_HEX: &str = "e4b8a1d6f2c9037b5a8e4d1c6f9b2085a7c3e9f6d1b4a8c2e5f7a0d3b6c9e2f5";
/// The `aphrodite.a.v1` gate seed (from the live gate bundle).
const GATE_SEED_HEX: &str = "452c1208202e241c084e870ad3dffbe033c45f0e398befa3681862c84da6af19";
/// The gate's master-key version prefix.
const GATE_VERSION: &str = "aphrodite.a.v1";
/// The browser UA (upstream `UA`).
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// The Helios source order (the bundle's candidate priority).
const HELIOS_ORDER: [&str; 3] = ["Moscow", "Novo", "Omsk"];
/// Upstream `this.ttl` — 60 s (payloads rotate every ~30-60 s).
const TTL: Duration = Duration::from_secs(60);
/// The shared scraper deadline (upstream `DEADLINE_MS`).
const DEADLINE: Duration = Duration::from_millis(10_500);
/// A master fetch's cap (upstream `MASTER_TIMEOUT_MS`).
const MASTER_TIMEOUT: Duration = Duration::from_millis(6_500);
/// A resolve-stage cap (upstream `RESOLVE_TIMEOUT_MS`).
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(4_500);
/// The empty-retry budget: one retry, only when the first attempt
/// finished inside 8 s (upstream `EMPTY_RETRY_MAX_FIRST_MS`) —
/// expressed as half of [`with_retry_on_empty`]'s total budget.
const RETRY_TOTAL: Duration = Duration::from_secs(16);
/// The delay before the one retry (upstream
/// `EMPTY_RETRY_DELAY_MS`).
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// The `Atlantic` provider.
pub struct Atlantic {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
}

impl Atlantic {
    /// A provider over the shared TMDB client.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>) -> Self {
        Self {
            info: SourceInfo {
                id: "atlantic".to_string(),
                label: "Atlantic".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(ORIGIN).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
        }
    }

    /// One scraper sweep — both servers in parallel (deadline-raced),
    /// the card fan, and the 4K-first sort.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
        tmdb_id: u64,
        name: &str,
        year: Option<u16>,
        // Upstream resolves the `IMDb` id into its scraper but never
        // uses it there (the helios/aphrodite chains take tmdb ids
        // only) — carried for shape parity.
        _imdb_id: Option<String>,
    ) -> Vec<Stream> {
        let is_tv = media.season.is_some();
        let media_type = if is_tv { "tv" } else { "movie" };

        let helios = with_deadline(
            self.helios_master(ctx, tmdb_id, media_type, media.season, media.episode),
            DEADLINE,
        );
        let aphrodite = with_deadline(
            self.aphrodite_master(ctx, tmdb_id, media_type, media.season, media.episode, name),
            DEADLINE,
        );
        let (helios, aphrodite) = futures::future::join(helios, aphrodite).await;

        let mut cards: Vec<NuvioStream> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if let Some(master) = helios.flatten() {
            self.helios_cards(ctx, master, &mut cards, &mut seen).await;
        }
        if let Some(master) = aphrodite.flatten() {
            self.aphrodite_cards(ctx, master, &mut cards, &mut seen)
                .await;
        }

        // 4K first (Stremio renders cards top-down).
        cards.sort_by(|a, b| {
            let rank = |quality: &str| {
                quality
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .unwrap_or(0)
            };
            rank(b.quality.as_deref().unwrap_or_default())
                .cmp(&rank(a.quality.as_deref().unwrap_or_default()))
        });

        let title = title_line(name, year, media);
        let params = BuildParams {
            streams: &cards,
            title: &title,
            source_id: &self.info.id,
            source_label: &self.info.label,
            country_codes: &[CountryCode::Multi],
            ttl: TTL,
        };
        build_stream_results(&params)
    }

    /// The Helios chain — candidates in bundle order, the first
    /// `#EXTM3U` master wins.
    async fn helios_master(
        &self,
        ctx: &ResolveCtx<'_>,
        tmdb_id: u64,
        media_type: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Option<Master> {
        let mut query = format!("tmdbId={tmdb_id}&type={media_type}");
        if media_type == "tv" {
            use std::fmt::Write as _;
            let _ = write!(
                query,
                "&seasonId={}&episodeId={}",
                season.unwrap_or(1),
                episode.unwrap_or(1)
            );
        }
        let url = Url::parse(&format!("{HELIOS}/helios?{query}")).ok()?;
        let request =
            with_headers(FetchRequest::get(url), &common_headers()).with_timeout(RESOLVE_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() || !response.body.starts_with('{') {
            return None;
        }
        let payload: Value = serde_json::from_str(&response.body).ok()?;
        let sources = payload.get("sources")?.as_object()?;

        let mut candidates: Vec<(String, String)> = Vec::new();
        for name in HELIOS_ORDER {
            let Some(raw) = sources
                .get(name)
                .and_then(|entry| entry.get("url"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(decrypted) = nesterov_decrypt(raw) else {
                continue;
            };
            if !decrypted.starts_with("http") {
                continue;
            }
            // A candidate pointing back at the SPA origin is a decoy.
            if let Ok(url) = Url::parse(&decrypted)
                && url
                    .host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case("atlantic.st"))
            {
                continue;
            }
            candidates.push((decrypted, format!("Helios · {name}")));
        }
        for (url, server) in candidates {
            let master = self.fetch_master(ctx, &url).await;
            if master.is_some() {
                return master.map(|parsed| Master {
                    url,
                    server,
                    body: parsed.0,
                    parsed: parsed.1,
                });
            }
        }
        None
    }

    /// The Aphrodite chain — gate-signed resolve, title guard, master
    /// parse.
    async fn aphrodite_master(
        &self,
        ctx: &ResolveCtx<'_>,
        tmdb_id: u64,
        media_type: &str,
        season: Option<u32>,
        episode: Option<u32>,
        expected_title: &str,
    ) -> Option<Master> {
        let path = if media_type == "tv" {
            format!(
                "/content/tv/{tmdb_id}/{}/{}",
                season.unwrap_or(1),
                episode.unwrap_or(1)
            )
        } else {
            format!("/content/movie/{tmdb_id}")
        };
        let resolved = self.gate_get(ctx, &path).await?;
        let title = resolved.title;
        if !title_matches(&title, expected_title) {
            return None;
        }
        let url = resolved
            .hls
            .or(resolved.url.filter(|url| url.starts_with("http")))?;
        if !url.starts_with("http") {
            return None;
        }
        let (body, parsed) = self.fetch_master(ctx, &url).await?;
        Some(Master {
            url,
            server: "Aphrodite".to_string(),
            body,
            parsed,
        })
    }

    /// One master fetch + parse — `None` unless it answers `#EXTM3U`.
    async fn fetch_master(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &str,
    ) -> Option<(String, ParsedMaster)> {
        let url = Url::parse(url).ok()?;
        let request =
            with_headers(FetchRequest::get(url), &common_headers()).with_timeout(MASTER_TIMEOUT);
        let response = ctx.fetcher.request(request).await.ok()?;
        if !response.is_success() || !response.body.starts_with("#EXTM3U") {
            return None;
        }
        Some((response.body.clone(), parse_master(&response.body)))
    }

    /// The Helios card fan — the variant/flat branches of the
    /// upstream artemis path.
    async fn helios_cards(
        &self,
        ctx: &ResolveCtx<'_>,
        master: Master,
        cards: &mut Vec<NuvioStream>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let Master {
            url,
            server,
            body,
            parsed,
        } = master;
        if !parsed.variants.is_empty() {
            let max_height = parsed.variants[0].height;
            let audio_note = audio_note(parsed.audio_tracks.len(), false);
            if parsed.separate_audio || !is_muxed(&parsed) {
                // The master card — players pick quality (and audio)
                // natively. The child is validated resolved against
                // the master.
                let top_child = resolve_child(&url, &parsed.variants[0].uri);
                let ok = with_deadline(validate_playlist_child(ctx, &top_child), DEADLINE)
                    .await
                    .unwrap_or(false);
                if ok {
                    push(
                        cards,
                        seen,
                        &url,
                        quality_label(max_height),
                        format!(
                            "{server} — Auto (up to {}){}",
                            quality_label(max_height),
                            audio_note
                        ),
                    );
                }
            } else {
                // Muxed children — per-variant cards (top four
                // heights, deduped, each validated).
                let mut per_height: Vec<&Variant> = Vec::new();
                for variant in &parsed.variants {
                    match per_height.iter().position(|v| v.height == variant.height) {
                        Some(index) => {
                            if per_height[index].bandwidth < variant.bandwidth {
                                per_height[index] = variant;
                            }
                        }
                        None => per_height.push(variant),
                    }
                }
                per_height.sort_by_key(|variant| std::cmp::Reverse(variant.height));
                for variant in per_height.iter().take(4) {
                    let child = resolve_child(&url, &variant.uri);
                    let ok = with_deadline(validate_playlist_child(ctx, &child), DEADLINE)
                        .await
                        .unwrap_or(false);
                    if ok {
                        push(
                            cards,
                            seen,
                            &child,
                            quality_label(variant.height),
                            format!("{server} — {}", quality_label(variant.height)),
                        );
                    }
                }
            }
        } else if body.contains("#EXTINF") {
            // A flat media playlist — validate the first segment.
            let first_segment = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'));
            let ok = match first_segment.and_then(|segment| {
                Url::parse(&url)
                    .ok()
                    .and_then(|base| base.join(segment).ok())
            }) {
                Some(segment) => with_deadline(probe_segment_magic(ctx, segment), DEADLINE)
                    .await
                    .unwrap_or(false),
                None => false,
            };
            if ok {
                push(
                    cards,
                    seen,
                    &url,
                    "Auto".to_string(),
                    format!("{server} — Auto"),
                );
            }
        }
    }

    /// The Aphrodite card fan — the top-variant and flat branches.
    async fn aphrodite_cards(
        &self,
        ctx: &ResolveCtx<'_>,
        master: Master,
        cards: &mut Vec<NuvioStream>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let Master {
            url,
            server,
            body,
            parsed,
        } = master;
        debug_assert_eq!(server, "Aphrodite");
        if !parsed.variants.is_empty() {
            let audio_note = audio_note(parsed.audio_tracks.len(), true);
            let top_child = resolve_child(&url, &parsed.variants[0].uri);
            let ok = with_deadline(validate_playlist_child(ctx, &top_child), DEADLINE)
                .await
                .unwrap_or(false);
            if ok {
                push(
                    cards,
                    seen,
                    &url,
                    quality_label(parsed.variants[0].height),
                    format!(
                        "Aphrodite — {}{audio_note}",
                        quality_label(parsed.variants[0].height)
                    ),
                );
            }
        } else if body.contains("#EXTINF") {
            let first_segment = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'));
            let ok = match first_segment.and_then(|segment| {
                Url::parse(&url)
                    .ok()
                    .and_then(|base| base.join(segment).ok())
            }) {
                Some(segment) => with_deadline(probe_segment_magic(ctx, segment), DEADLINE)
                    .await
                    .unwrap_or(false),
                None => false,
            };
            if ok {
                push(
                    cards,
                    seen,
                    &url,
                    "Auto".to_string(),
                    "Aphrodite — Auto".to_string(),
                );
            }
        }
    }

    /// The gate-signed GET of a content path — with the session reset,
    /// one retry, and the unsigned fallback. `None` when the answer is
    /// not a `found` content entry.
    async fn gate_get(&self, ctx: &ResolveCtx<'_>, path: &str) -> Option<GateAnswer> {
        for _ in 0..2 {
            // The gate is gone — the unsigned path (decoy-guarded
            // by the title check).
            let Some(session) = bootstrap_session(ctx).await else {
                return unsigned_gate_get(ctx, path).await;
            };
            let url = Url::parse(&format!("{CDN}{path}")).ok()?;
            let headers = gate_sign_headers(&session, path);
            let request = with_owned_headers(
                with_headers(FetchRequest::get(url), &common_headers()),
                &headers,
            )
            .with_timeout(MASTER_TIMEOUT);
            let Ok(response) = ctx.fetcher.request(request).await else {
                return None;
            };
            match response.status {
                401 | 403 => continue, // fresh session, retry once
                _ if !response.is_success() => return None,
                _ => {}
            }
            if !response.body.starts_with('{') {
                return None;
            }
            let Ok(payload) = serde_json::from_str::<GatePayload>(&response.body) else {
                return None;
            };
            if payload.renew == Some(true) {
                continue;
            }
            if payload.found != Some(true) {
                return None;
            }
            return Some(GateAnswer {
                hls: payload.hls.filter(|hls| !hls.is_empty()),
                url: payload.url,
                title: payload.title.unwrap_or_default(),
            });
        }
        None
    }
}

#[async_trait]
impl Source for Atlantic {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    async fn resolve(
        &self,
        ctx: &ResolveCtx<'_>,
        media: &MediaRef,
    ) -> Result<Vec<Stream>, SourceError> {
        let tmdb_id = tmdb_id(ctx, media, &self.tmdb).await?;
        let (name, year) = name_and_year(ctx, media, &self.tmdb, tmdb_id).await?;
        let imdb_id = best_effort_imdb(ctx, media, &self.tmdb, tmdb_id).await;

        let streams = with_retry_on_empty(
            || async {
                Some(
                    self.sweep(ctx, media, tmdb_id, &name, year, imdb_id.clone())
                        .await,
                )
            },
            2,
            RETRY_TOTAL,
            RETRY_DELAY,
        )
        .await
        .unwrap_or_default();
        if streams.is_empty() {
            Err(SourceError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

/// One resolved master.
struct Master {
    /// The master (or flat playlist) URL.
    url: String,
    /// The serving server (`Helios · Moscow`, `Aphrodite`).
    server: String,
    /// The raw body (flat playlists need it).
    body: String,
    /// The parsed variants and audio groups.
    parsed: ParsedMaster,
}

/// The parsed shape of a master playlist.
#[derive(Default)]
struct ParsedMaster {
    /// The `#EXT-X-STREAM-INF` variants (height desc, then bandwidth).
    variants: Vec<Variant>,
    /// The `TYPE=AUDIO` group names.
    audio_tracks: Vec<String>,
    /// Whether an audio group exists (video-only children).
    separate_audio: bool,
}

/// One variant.
struct Variant {
    /// The resolution height (`0` when the line lacks one).
    height: u32,
    /// The `BANDWIDTH` value.
    bandwidth: u64,
    /// The codecs attribute.
    codecs: String,
    /// The variant URI.
    uri: String,
}

/// The gate answer.
struct GateAnswer {
    /// The `hls` field (preferred).
    hls: Option<String>,
    /// The `url` field (legacy).
    url: Option<String>,
    /// The content title (the decoy guard).
    title: String,
}

/// The gate content response.
#[derive(Deserialize)]
struct GatePayload {
    /// The found marker.
    #[serde(default)]
    found: Option<bool>,
    /// The playlist URL (current shape).
    #[serde(default)]
    hls: Option<String>,
    /// The playlist URL (legacy shape).
    #[serde(default)]
    url: Option<String>,
    /// The content title.
    #[serde(default)]
    title: Option<String>,
    /// The session-renew signal.
    #[serde(default)]
    renew: Option<bool>,
}

/// The bootstrapped gate session.
struct GateSession {
    /// The session id.
    sid: String,
    /// The signing key (32 bytes).
    skey: [u8; 32],
}

/// The full browser header set the payload workers require — the
/// upstream `HEADERS` minus the UA (attached per request).
fn common_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("User-Agent", USER_AGENT),
        ("Origin", ORIGIN),
        ("Referer", REFERER),
        ("Accept", "*/*"),
        ("Accept-Language", "en-US,en;q=0.9"),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "cross-site"),
    ]
}

/// Attach a static header list to a request.
fn with_headers(
    mut request: FetchRequest,
    headers: &[(&'static str, &'static str)],
) -> FetchRequest {
    for (name, value) in headers {
        request = request.with_header(*name, *value);
    }
    request
}

/// Attach an owned header list to a request.
fn with_owned_headers(mut request: FetchRequest, headers: &[(String, String)]) -> FetchRequest {
    for (name, value) in headers {
        request = request.with_header(name.as_str(), value.as_str());
    }
    request
}

/// The nesterov decrypt — `ns_<hex>` (iv ‖ ct ‖ tag) → the payload
/// worker URL; plain URLs pass through.
fn nesterov_decrypt(url: &str) -> Option<String> {
    if !url.starts_with("ns_") {
        return Some(url.to_string());
    }
    let blob = hex_to_bytes(&url[3..])?;
    if blob.len() < 29 {
        return None;
    }
    let (iv, rest) = blob.split_at(12);
    let (ciphertext, tag) = rest.split_at(rest.len() - 16);
    let key = hex_to_bytes(NESTEROV_KEY_HEX)?;
    let plain = gcm::decrypt(&key, iv, tag, ciphertext)?;
    String::from_utf8(plain).ok()
}

/// The gate master key — `SHA256("aphrodite.a.v1" ‖ seed)`.
fn gate_master_key() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(GATE_VERSION.as_bytes());
    hasher.update(hex_to_bytes(GATE_SEED_HEX).unwrap_or_default());
    hasher.finalize().into()
}

/// Bootstrap a gate session — the signed `POST /content/index` and the
/// AES-256-GCM session payload.
async fn bootstrap_session(ctx: &ResolveCtx<'_>) -> Option<GateSession> {
    let master_key = gate_master_key();
    let ts = now_secs();
    let nonce = hex_nonce();
    let message = format!("a|{ts}|{nonce}");
    let signature = hex_encode(&hmac_sha256(&master_key, message.as_bytes()));
    let body = serde_json::json!({ "c": "a", "ts": ts, "n": nonce, "s": signature });
    let url = Url::parse(&format!("{CDN}/content/index")).ok()?;
    let request = with_headers(
        FetchRequest::post(url, body.to_string()).with_header("Content-Type", "application/json"),
        &common_headers(),
    )
    .with_timeout(Duration::from_secs(8));
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let payload: Value = serde_json::from_str(&response.body).ok()?;
    let blob = hex_to_bytes(payload.get("d")?.as_str()?)?;
    if blob.len() < 28 {
        return None;
    }
    let (iv, rest) = blob.split_at(12);
    let (ciphertext, tag) = rest.split_at(rest.len() - 16);
    let plain = gcm::decrypt(&master_key, iv, tag, ciphertext)?;
    let session: Value = serde_json::from_slice(&plain).ok()?;
    let sid = session.get("sid")?.as_str()?.to_string();
    let skey = hex_to_bytes(session.get("skey")?.as_str()?)?;
    let skey: [u8; 32] = skey.try_into().ok()?;
    Some(GateSession { sid, skey })
}

/// The signed request headers for a path — `X-A-Sid/Ts/Nonce/Sig`.
fn gate_sign_headers(session: &GateSession, path: &str) -> Vec<(String, String)> {
    let ts = now_secs();
    let nonce = hex_nonce();
    let message = format!("{}|{path}|{ts}|{nonce}", session.sid);
    let signature = hex_encode(&hmac_sha256(&session.skey, message.as_bytes()));
    vec![
        ("X-A-Sid".to_string(), session.sid.clone()),
        ("X-A-Ts".to_string(), ts.to_string()),
        ("X-A-Nonce".to_string(), nonce),
        ("X-A-Sig".to_string(), signature),
    ]
}

/// The unsigned GET fallback for a dead gate.
async fn unsigned_gate_get(ctx: &ResolveCtx<'_>, path: &str) -> Option<GateAnswer> {
    let url = Url::parse(&format!("{CDN}{path}")).ok()?;
    let request =
        with_headers(FetchRequest::get(url), &common_headers()).with_timeout(MASTER_TIMEOUT);
    let response = ctx.fetcher.request(request).await.ok()?;
    if !response.is_success() || !response.body.starts_with('{') {
        return None;
    }
    let payload: GatePayload = serde_json::from_str(&response.body).ok()?;
    if payload.found != Some(true) {
        return None;
    }
    Some(GateAnswer {
        hls: payload.hls.filter(|hls| !hls.is_empty()),
        url: payload.url,
        title: payload.title.unwrap_or_default(),
    })
}

/// Parse a master playlist — variants (height desc), audio groups.
fn parse_master(body: &str) -> ParsedMaster {
    let mut parsed = ParsedMaster::default();
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let mut pending: Option<(Option<u32>, Option<u64>, String)> = None;
    for line in lines {
        if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            if attributes.contains("TYPE=AUDIO")
                && let Some(name) = extract_quoted(attributes, "NAME")
            {
                parsed.audio_tracks.push(name);
            }
            continue;
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let height = attributes
                .split(',')
                .find_map(|pair| pair.strip_prefix("RESOLUTION="))
                .and_then(|resolution| resolution.split('x').nth(1))
                .and_then(|h| h.parse::<u32>().ok());
            let bandwidth = attributes
                .split(',')
                .find_map(|pair| pair.strip_prefix("BANDWIDTH="))
                .and_then(|bw| bw.parse::<u64>().ok());
            let codecs = extract_quoted(attributes, "CODECS").unwrap_or_default();
            pending = Some((height, bandwidth, codecs));
        } else if !line.starts_with('#')
            && !line.is_empty()
            && let Some((height, bandwidth, codecs)) = pending.take()
        {
            parsed.variants.push(Variant {
                height: height.unwrap_or(0),
                bandwidth: bandwidth.unwrap_or(0),
                codecs,
                uri: line.to_string(),
            });
        }
    }
    parsed.separate_audio = !parsed.audio_tracks.is_empty();
    parsed
        .variants
        .sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    parsed
}

/// A `KEY="value"` attribute.
fn extract_quoted(attributes: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=\"");
    let start = attributes.find(&prefix)? + prefix.len();
    let end = attributes[start..].find('"')? + start;
    Some(attributes[start..end].to_string())
}

/// Whether every variant carries its own audio codec (Nova-style
/// muxed children are safe as per-variant cards).
fn is_muxed(parsed: &ParsedMaster) -> bool {
    if parsed.separate_audio || parsed.variants.is_empty() {
        return false;
    }
    parsed.variants.iter().all(|variant| {
        let codecs = variant.codecs.to_ascii_lowercase();
        ["mp4a", "ac-3", "ec-3", "opus", "vorbis"]
            .iter()
            .any(|codec| codecs.contains(codec))
    })
}

/// The quality label of a height.
fn quality_label(height: u32) -> String {
    match height {
        h if h >= 2160 => "2160p".to_string(),
        h if h >= 1080 => "1080p".to_string(),
        h if h >= 720 => "720p".to_string(),
        h if h >= 480 => "480p".to_string(),
        h if h > 0 => format!("{h}p"),
        _ => "Auto".to_string(),
    }
}

/// The audio-track note in a card title.
fn audio_note(count: usize, aphrodite: bool) -> String {
    if aphrodite {
        if count > 1 {
            format!(", {count} audio tracks")
        } else {
            String::new()
        }
    } else if count > 1 {
        format!(", {count} audio tracks (player audio menu)")
    } else if count == 1 {
        ", 1 audio track".to_string()
    } else {
        String::new()
    }
}

/// Resolve a playlist child URI against its master — the masters
/// carry relative variant/segment paths.
fn resolve_child(master: &str, uri: &str) -> String {
    Url::parse(master)
        .ok()
        .and_then(|base| base.join(uri).ok())
        .map_or_else(|| uri.to_string(), Url::into)
}

/// A child/variant playlist must be an m3u8 with at least one
/// playable line.
async fn validate_playlist_child(ctx: &ResolveCtx<'_>, uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    let request =
        with_headers(FetchRequest::get(url), &common_headers()).with_timeout(MASTER_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return false;
    };
    if !response.is_success() || !response.body.starts_with("#EXTM3U") {
        return false;
    }
    response
        .body
        .lines()
        .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
}

/// The first bytes of a media segment — a TS sync byte or an fMP4 box
/// magic (the lossy-decoded body keeps both as ASCII).
async fn probe_segment_magic(ctx: &ResolveCtx<'_>, url: Url) -> bool {
    let request = with_headers(FetchRequest::get(url), &common_headers())
        .with_header("Range", "bytes=0-4095")
        .with_timeout(MASTER_TIMEOUT);
    let Ok(response) = ctx.fetcher.request(request).await else {
        return false;
    };
    if !response.is_success() {
        return false;
    }
    let body = response.body.as_bytes();
    if body.len() < 8 {
        return false;
    }
    // MPEG-TS: 0x47 sync bytes at 0 and 188 ('G' survives the decode).
    if body.first() == Some(&b'G') && body.get(188) == Some(&b'G') {
        return true;
    }
    matches!(&body[4..8], b"ftyp" | b"styp" | b"moov")
}

/// The decoy title guard — token overlap, order-free (60 % of the
/// expected title's tokens present).
fn title_matches(returned: &str, expected: &str) -> bool {
    if returned.is_empty() || expected.is_empty() {
        return true;
    }
    let normalize = |text: &str| {
        text.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<String>>()
    };
    let a = normalize(returned);
    let b = normalize(expected);
    let joined_a = a.join(" ");
    let joined_b = b.join(" ");
    if joined_a == joined_b || joined_a.contains(&joined_b) || joined_b.contains(&joined_a) {
        return true;
    }
    let hits = b.iter().filter(|token| a.contains(token)).count();
    !b.is_empty() && hits * 10 >= b.len() * 6
}

/// One card — the server/quality title, the `Atlantic` name, and the
/// full browser header set.
fn push(
    cards: &mut Vec<NuvioStream>,
    seen: &mut std::collections::HashSet<String>,
    url: &str,
    quality: String,
    title: String,
) {
    if !url.starts_with("http") || !seen.insert(url.to_string()) {
        return;
    }
    let mut stream = NuvioStream::new(url)
        .with_name("Atlantic")
        .with_title(title);
    stream.quality = Some(quality);
    stream.kind = Some("hls".to_string());
    for (name, value) in common_headers() {
        stream = stream.with_header(name, value);
    }
    cards.push(stream);
}

/// The base display title for
/// `build_stream_results` —
/// upstream `name + (season ? ' S01E02' : ' (year)')`.
fn title_line(name: &str, year: Option<u16>, media: &MediaRef) -> String {
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
    media: &MediaRef,
    tmdb: &TmdbClient,
) -> Result<u64, SourceError> {
    match &media.id {
        MediaId::Tmdb(id) => Ok(*id),
        MediaId::Imdb(imdb) => match ctx.media.as_ref().and_then(|media| media.tmdb_id) {
            Some(pre_resolved) => Ok(pre_resolved),
            None => tmdb.tmdb_id_from_imdb(imdb, media.kind).await,
        },
    }
}

/// Name and year, preferring pre-resolved context media — ports
/// `getTmdbNameAndYear`.
async fn name_and_year(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Result<(String, Option<u16>), SourceError> {
    if let Some(resolved) = ctx.media.as_ref().filter(|media| !media.name.is_empty()) {
        return Ok((resolved.name.clone(), resolved.year));
    }
    let name = tmdb.name_and_year(tmdb_id, media.kind, None).await?;
    Ok((name.name, name.year))
}

/// The best-effort `IMDb` id — ports `getImdbId`'s try/catch.
async fn best_effort_imdb(
    ctx: &ResolveCtx<'_>,
    media: &MediaRef,
    tmdb: &TmdbClient,
    tmdb_id: u64,
) -> Option<String> {
    match &media.id {
        MediaId::Imdb(imdb) => Some(imdb.clone()),
        MediaId::Tmdb(_) => {
            if let Some(pre_resolved) = ctx.media.as_ref().and_then(|media| media.imdb_id.clone()) {
                return Some(pre_resolved);
            }
            tmdb.imdb_id_from_tmdb(tmdb_id, media.kind)
                .await
                .ok()
                .flatten()
        }
    }
}

/// Seconds since the epoch.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// A nonce-scrambled 8-byte hex string (the JS used
/// `crypto.randomBytes(8)`; no rand crate ships in the workspace, so
/// the nonce derives from the clock — it only needs to be unique per
/// request).
fn hex_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    // The splitmix64 seed: the low 64 bits of the nanosecond clock —
    // truncation is the seeding, not a loss.
    #[allow(clippy::cast_possible_truncation)]
    let mut state = (nanos as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^= state >> 31;
    let bytes = state.to_be_bytes();
    hex_encode(&bytes)
}

/// HMAC-SHA256 (FIPS 198-1) — the gate's signing primitive.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(message);
    let inner_digest: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(key_block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Hex decode.
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect()
}

/// Hex encode.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// AES-256-GCM decryption over the raw `aes` block cipher — the same
/// minimal SP 800-38D construction the `VidZee` extractor and the
/// vidstorm port carry (the workspace ships no GCM crate).
mod gcm {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::{Aes256, Block};

    /// The GHASH reduction constant `R = E1 ‖ 0^120`.
    const R: u128 = 0xE1 << 120;

    /// Decrypt `ciphertext` under `key`, verifying `auth_tag` (empty
    /// associated data, 96-bit IV).
    pub(super) fn decrypt(
        key: &[u8],
        iv: &[u8],
        auth_tag: &[u8],
        ciphertext: &[u8],
    ) -> Option<Vec<u8>> {
        if key.len() != 32 || iv.len() != 12 || auth_tag.len() != 16 {
            return None;
        }
        let cipher = Aes256::new(GenericArray::from_slice(key));
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        let h = e_k(&cipher, &[0u8; 16]);
        let expected = xor(&e_k(&cipher, &j0), &ghash(h, ciphertext));
        if !constant_time_eq(&expected, auth_tag) {
            return None;
        }
        let mut plaintext = ciphertext.to_vec();
        let mut counter = j0;
        for chunk in plaintext.chunks_mut(16) {
            increment_counter(&mut counter);
            let keystream = e_k(&cipher, &counter);
            for (byte, ks) in chunk.iter_mut().zip(keystream) {
                *byte ^= ks;
            }
        }
        Some(plaintext)
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

    /// Constant-time tag comparison.
    fn constant_time_eq(a: &[u8; 16], b: &[u8]) -> bool {
        if b.len() != 16 {
            return false;
        }
        let mut diff = 0u8;
        for (byte, other) in a.iter().zip(b) {
            diff |= byte ^ other;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::error::FetchError;
    use vsources_core::traits::{FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;

    use super::*;

    // Ground truths generated with Node's `crypto` (aes-256-gcm over
    // the exact upstream key material).

    /// A nesterov token decrypting (with the bundle key, IV
    /// `00112233445566778899aabb`) to the payload-worker master.
    const NESTEROV_TOKEN: &str = "ns_00112233445566778899aabb92513db68e1e723f67637bdde83f667e49c4230ccc3d138c6127bb9ec529a42a965303df93c5e8aab905308b0be105cf8edd59b6a64992974e371f60363663002d10054127c3e244ef8d410c87";
    /// The master URL it decrypts to.
    const NESTEROV_URL: &str = "https://peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8";
    /// The gate session blob — `{sid, skey, exp}` AES-256-GCM'd under
    /// the `aphrodite.a.v1` master key (IV `aabbccddeeff001122334455`).
    const SESSION_BLOB_HEX: &str = "aabbccddeeff001122334455da4e0275aed434377398117fc099961c57e1a1132722d8aaa0d4f0352070bee94f6b9f0f709c6022b2c3e763c5d12ab775c6c0f8e6366b47f44365b9e25ad646f4ff78fe45e2cf82b0b3b512d2e630c59293bac9afc6d38891822ced045fe002809e20af8e714f299bba530961534385209322f84f47ee8e23e7e3198729";
    /// The session the blob decrypts to.
    const SESSION_SID: &str = "sess-1234";
    /// The session's signing key (hex).
    const SESSION_SKEY_HEX: &str =
        "5ee43d0e0f169887b0ee184e4ffbb7ee6d9629c97cf210ad8c2c4ebc73c1d8f5";

    /// A canned response.
    #[derive(Clone)]
    struct Scripted {
        /// HTTP status.
        status: u16,
        /// The body.
        body: String,
    }

    /// A fetcher serving scripted pages by host+path (in order, the
    /// last repeating) and recording every request it sees.
    struct MockFetcher {
        pages: Mutex<HashMap<String, VecDeque<Scripted>>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl MockFetcher {
        /// A fetcher serving nothing yet.
        fn new() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Serve `key` (host + path) with `status`/`body`.
        fn serve(self, key: &str, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.to_string())
                .or_default()
                .push_back(Scripted {
                    status,
                    body: body.into(),
                });
            self
        }

        /// The value of a header sent to `key` (host + path).
        fn sent_header(&self, key: &str, name: &str) -> Option<String> {
            self.requests()
                .iter()
                .filter(|request| request_key(request) == key)
                .find_map(|request| {
                    request
                        .headers
                        .iter()
                        .find(|(header, _)| header.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
        }

        /// The POST bodies sent to `key`, in order.
        fn sent_bodies(&self, key: &str) -> Vec<String> {
            self.requests()
                .iter()
                .filter(|request| request_key(request) == key)
                .map(|request| request.body.clone().unwrap_or_default())
                .collect()
        }

        /// Every request seen, in order.
        fn requests(&self) -> Vec<FetchRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Default for MockFetcher {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The lookup key of a request: host + path.
    fn request_key(request: &FetchRequest) -> String {
        format!(
            "{}{}",
            request.url.host_str().unwrap_or_default(),
            request.url.path()
        )
    }

    #[async_trait]
    impl Fetcher for MockFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            let key = request_key(&request);
            let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
            let response = pages.get_mut(&key).and_then(|queue| {
                // The last scripted response repeats.
                if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                }
            });
            match response {
                Some(scripted) => Ok(FetchResponse {
                    url: request.url,
                    status: scripted.status,
                    headers: BTreeMap::new(),
                    body: scripted.body,
                }),
                None => Err(FetchError::NotFound { url: request.url }),
            }
        }
    }

    /// A provider over the mock.
    fn provider(fetcher: &Arc<MockFetcher>) -> Atlantic {
        Atlantic::new(Arc::new(TmdbClient::new("test-key", fetcher.clone())))
    }

    /// A resolve context over the mock.
    fn ctx_for(fetcher: &Arc<MockFetcher>, media: Option<ResolvedMedia>) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher: fetcher.as_ref(),
            media,
            source_id: None,
            referer: None,
        }
    }

    /// A resolved Dune movie.
    fn dune_media() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(438_631),
            imdb_id: Some("tt1160419".to_string()),
            name: "Dune".to_string(),
            year: Some(2021),
            season: None,
            episode: None,
        }
    }

    /// The dune movie reference.
    fn dune_movie() -> MediaRef {
        MediaRef::movie(MediaId::Tmdb(438_631))
    }

    /// A muxed master (Nova style — every variant carries mp4a).
    fn muxed_master() -> String {
        "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"mp4a.40.2,avc1.640028\"\n\
v1080.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS=\"mp4a.40.2,avc1.64001f\"\n\
v720.m3u8\n"
            .to_string()
    }

    /// A separated-audio master (Orbit style — an audio group,
    /// video-only children).
    fn separated_master() -> String {
        "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio 1\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio 2\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=3840x2160,CODECS=\"avc1.640028\"\n\
v2160.m3u8\n"
            .to_string()
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        assert_eq!(info.id, "atlantic");
        assert_eq!(info.label, "Atlantic");
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
            Some("https://atlantic.st/")
        );
        assert_eq!(info.priority, 0);
    }

    #[test]
    fn nesterov_ground_truth_decrypts() {
        assert_eq!(
            nesterov_decrypt(NESTEROV_TOKEN).as_deref(),
            Some(NESTEROV_URL)
        );
        // Plain URLs pass through; malformed tokens fail closed.
        assert_eq!(
            nesterov_decrypt("https://plain.example/x.m3u8").as_deref(),
            Some("https://plain.example/x.m3u8")
        );
        assert!(nesterov_decrypt("ns_deadbeef").is_none());
    }

    #[tokio::test]
    async fn gate_bootstrap_ground_truth_decrypts() {
        // The blob decrypts under the derived master key and yields
        // the session.
        let fetcher = Arc::new(MockFetcher::new().serve(
            "cdn.hls.lol/content/index",
            200,
            format!("{{\"d\":\"{SESSION_BLOB_HEX}\"}}"),
        ));
        let ctx = ctx_for(&fetcher, None);
        let session = bootstrap_session(&ctx).await;
        assert!(session.is_some());
        let session = session.unwrap_or_else(|| panic!("the session blob must decrypt"));
        assert_eq!(session.sid, SESSION_SID);
        assert_eq!(hex_encode(&session.skey), SESSION_SKEY_HEX);
        // The bootstrap POST carried the HMAC signature and label.
        let body = fetcher.sent_bodies("cdn.hls.lol/content/index");
        assert_eq!(body.len(), 1);
        let payload: Value =
            serde_json::from_str(&body[0]).unwrap_or_else(|e| panic!("valid JSON: {e}"));
        assert_eq!(payload.get("c").and_then(Value::as_str), Some("a"));
        assert!(
            payload
                .get("s")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        );
    }

    #[tokio::test]
    async fn helios_muxed_master_ships_per_variant_cards() -> Result<(), SourceError> {
        let helios_payload = serde_json::json!({
            "sources": { "Moscow": { "url": NESTEROV_TOKEN } }
        });
        let master = muxed_master();
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8",
                    200,
                    master,
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v1080.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v720.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the helios fixture must resolve: {e}"));

        assert_eq!(streams.len(), 2);
        let first = &streams[0];
        // The child variant URL (validated), 4K-first order.
        assert_eq!(
            first.url.as_str(),
            "https://peraspera.nbsycfzrpa4.workers.dev/v1/dune/v1080.m3u8"
        );
        assert_eq!(first.format, Format::Hls);
        assert_eq!(first.meta.resolution, Some(1080));
        assert_eq!(first.meta.source_id.as_deref(), Some("atlantic"));
        assert_eq!(first.meta.source_label.as_deref(), Some("Atlantic"));
        assert_eq!(first.ttl, TTL);
        // The payload workers' header requirements ride the stream.
        assert_eq!(
            first
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://atlantic.st/")
        );
        assert_eq!(
            first.meta.request_headers.get("Origin").map(String::as_str),
            Some("https://atlantic.st")
        );
        // The label: base — server card.
        let label = first.label.as_deref().unwrap_or_default();
        assert!(
            label.starts_with("Dune (2021) — Helios · Moscow — 1080p"),
            "{label}"
        );
        // The helios query carried the browser set.
        assert_eq!(
            fetcher
                .sent_header("stream.hls.lol/helios", "Origin")
                .as_deref(),
            Some("https://atlantic.st")
        );
        assert_eq!(
            fetcher
                .sent_header("stream.hls.lol/helios", "Sec-Fetch-Site")
                .as_deref(),
            Some("cross-site")
        );
        Ok(())
    }

    #[tokio::test]
    async fn helios_separated_audio_ships_the_master_card() {
        let helios_payload = serde_json::json!({
            "sources": { "Novo": { "url": "https://totallyacdn.org/dune/master.m3u8" } }
        });
        let master = separated_master();
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve("totallyacdn.org/dune/master.m3u8", 200, master)
                .serve(
                    "totallyacdn.org/dune/v2160.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the separated fixture must resolve: {e}"));
        // The MASTER ships (players need the audio-group context), not
        // the video-only child.
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://totallyacdn.org/dune/master.m3u8"
        );
        assert_eq!(streams[0].meta.resolution, Some(2160));
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(
            label.contains("Helios · Novo — Auto (up to 2160p), 2 audio tracks"),
            "{label}"
        );
    }

    #[tokio::test]
    async fn spa_decoy_candidates_drop() {
        let helios_payload = serde_json::json!({
            "sources": {
                "Moscow": { "url": "https://atlantic.st/edge-abc/index.m3u8" },
                "Omsk": { "url": NESTEROV_TOKEN }
            }
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8",
                    200,
                    muxed_master(),
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v1080.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v720.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the decoy fixture must resolve: {e}"));
        // Omsk answered; the SPA-shell decoy never fetched.
        assert_eq!(streams.len(), 2);
        assert!(streams.iter().all(|stream| {
            stream
                .label
                .as_deref()
                .is_some_and(|l| l.contains("Helios · Omsk"))
        }));
    }

    #[tokio::test]
    async fn aphrodite_gate_ships_the_curated_master() {
        let content_payload = serde_json::json!({
            "found": true,
            "title": "Dune",
            "hls": "https://totallyacdn.org/content/dune/master.m3u8"
        });
        let master = separated_master();
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "cdn.hls.lol/content/index",
                    200,
                    format!("{{\"d\":\"{SESSION_BLOB_HEX}\"}}"),
                )
                .serve(
                    "cdn.hls.lol/content/movie/438631",
                    200,
                    content_payload.to_string(),
                )
                .serve("totallyacdn.org/content/dune/master.m3u8", 200, master)
                .serve(
                    "totallyacdn.org/content/dune/v2160.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the aphrodite fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://totallyacdn.org/content/dune/master.m3u8"
        );
        assert_eq!(streams[0].meta.resolution, Some(2160));
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(
            label.contains("Aphrodite — 2160p, 2 audio tracks"),
            "{label}"
        );
        // The content GET carried the gate signature headers.
        let key = "cdn.hls.lol/content/movie/438631";
        assert_eq!(
            fetcher.sent_header(key, "X-A-Sid").as_deref(),
            Some(SESSION_SID)
        );
        assert!(
            fetcher
                .sent_header(key, "X-A-Sig")
                .is_some_and(|sig| !sig.is_empty())
        );
    }

    #[tokio::test]
    async fn aphrodite_title_mismatch_drops_the_decoy() {
        let content_payload = serde_json::json!({
            "found": true,
            "title": "Coyote vs. Acme",
            "hls": "https://totallyacdn.org/content/other/master.m3u8"
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve(
                    "cdn.hls.lol/content/index",
                    200,
                    format!("{{\"d\":\"{SESSION_BLOB_HEX}\"}}"),
                )
                .serve(
                    "cdn.hls.lol/content/movie/438631",
                    200,
                    content_payload.to_string(),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn dead_gate_falls_back_to_the_unsigned_path() {
        // The bootstrap 403s; the unsigned GET still answers (the
        // title guard holds), and the master resolves.
        let content_payload = serde_json::json!({
            "found": true,
            "title": "Dune",
            "hls": "https://totallyacdn.org/content/dune/master.m3u8"
        });
        let master = separated_master();
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("cdn.hls.lol/content/index", 403, "{}")
                .serve(
                    "cdn.hls.lol/content/movie/438631",
                    200,
                    content_payload.to_string(),
                )
                .serve("totallyacdn.org/content/dune/master.m3u8", 200, master)
                .serve(
                    "totallyacdn.org/content/dune/v2160.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the unsigned fallback must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        // No gate headers on the unsigned path.
        assert!(
            fetcher
                .sent_header("cdn.hls.lol/content/movie/438631", "X-A-Sid")
                .is_none()
        );
    }

    #[tokio::test]
    async fn unvalidated_children_ship_nothing() {
        // The master parses but the child playlist is HTML — the card
        // must drop (nothing ships unvalidated).
        let helios_payload = serde_json::json!({
            "sources": { "Moscow": { "url": NESTEROV_TOKEN } }
        });
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8",
                    200,
                    muxed_master(),
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v1080.m3u8",
                    200,
                    "<html>loading…</html>",
                )
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/v720.m3u8",
                    404,
                    "",
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn flat_media_playlist_validates_the_first_segment() {
        let helios_payload = serde_json::json!({
            "sources": { "Moscow": { "url": NESTEROV_TOKEN } }
        });
        // A flat playlist: no #EXT-X-STREAM-INF, one #EXTINF segment.
        let flat = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg-00001.ts\n";
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8",
                    200,
                    flat,
                )
                // The segment answers TS sync bytes ('G' at 0 and 188).
                .serve(
                    "peraspera.nbsycfzrpa4.workers.dev/v1/dune/seg-00001.ts",
                    200,
                    format!("G{}G", "x".repeat(187)),
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let streams = provider(&fetcher)
            .resolve(&ctx, &dune_movie())
            .await
            .unwrap_or_else(|e| panic!("the flat fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].url.as_str(),
            "https://peraspera.nbsycfzrpa4.workers.dev/v1/dune/master.m3u8"
        );
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(label.contains("Helios · Moscow — Auto"), "{label}");
    }

    #[tokio::test]
    async fn helios_miss_is_not_found() {
        // The helios answer is empty and the gate 404s.
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, r#"{"sources":{}}"#)
                .serve("cdn.hls.lol/content/index", 404, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher).resolve(&ctx, &dune_movie()).await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }

    #[tokio::test]
    async fn series_queries_the_season_episode_ids() {
        let helios_payload = serde_json::json!({
            "sources": { "Moscow": { "url": "https://totallyacdn.org/tv/master.m3u8" } }
        });
        let master = muxed_master();
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("stream.hls.lol/helios", 200, helios_payload.to_string())
                .serve("totallyacdn.org/tv/master.m3u8", 200, master)
                .serve(
                    "totallyacdn.org/tv/v1080.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                )
                .serve(
                    "totallyacdn.org/tv/v720.m3u8",
                    200,
                    "#EXTM3U\n#EXTINF:4,\nseg1.ts\n",
                ),
        );
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref(),
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(2),
                episode: Some(3),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 2, 3);
        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 2);
        let query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.host_str() == Some("stream.hls.lol"))
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(query.contains("tmdbId=1396"), "{query}");
        assert!(query.contains("type=tv"), "{query}");
        assert!(query.contains("seasonId=2"), "{query}");
        assert!(query.contains("episodeId=3"), "{query}");
        // The series label carries S02E03.
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(label.contains("Breaking Bad S02E03"), "{label}");
    }

    #[test]
    fn title_guard_overlaps_tokens() {
        assert!(title_matches("Dune", "Dune"));
        assert!(title_matches("Dune: Part Two", "Dune"));
        assert!(title_matches("Dune (2021)", "Dune 2021"));
        assert!(!title_matches("Coyote vs. Acme", "Dune"));
        // Nothing to compare — don't block.
        assert!(title_matches("", "Dune"));
    }

    #[test]
    fn master_parsing_reads_variants_and_audio() {
        let parsed = parse_master(&separated_master());
        assert_eq!(parsed.variants.len(), 1);
        assert_eq!(parsed.variants[0].height, 2160);
        assert_eq!(parsed.audio_tracks, vec!["Audio 1", "Audio 2"]);
        assert!(parsed.separate_audio);
        assert!(!is_muxed(&parsed));

        let muxed = parse_master(&muxed_master());
        assert_eq!(muxed.variants.len(), 2);
        assert_eq!(muxed.variants[0].height, 1080);
        assert!(is_muxed(&muxed));
        assert_eq!(quality_label(2160), "2160p");
        assert_eq!(quality_label(0), "Auto");
    }
}
