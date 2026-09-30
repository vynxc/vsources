//! `VideasyTo`: the `speedracelight` fan through `player.videasy.to`.
//!
//! Ports `src/source/VideasyTo.js` + its Nuvio scraper
//! `src/nuvio/videasyto.cjs` — which does not scrape on its own but
//! delegates the whole sweep to the videasy scraper
//! (`src/nuvio/videasy.cjs`: the shared seed, the ten-server fan, the
//! base64+XOR-encrypted `{sources, subtitles}` payloads) and then
//! renames and normalizes the cards for the `.to` player.
//!
//! Flow:
//!
//! 1. Resolve the TMDB id, name/year, and best-effort `IMDb` id (the
//!    context media or [`TmdbClient`]).
//! 2. A seed from `api.speedracelight.com/seed` (the shared
//!    [`SeedStore`] — 25 s cache, in-flight coalescing, 120 s
//!    down-mark) and the ten videasy servers in parallel via
//!    `fetch_provider`:
//!    `Hydrogen` (`cdn`), `Titanium` (`tejo`), `Oxygen` (`neon2`),
//!    `Lithium` (`downloader2`), `Krypton` (`ym`), `Carbon`
//!    (`mb-flix`), `Aluminium` (`lamovie`), `Nitrogen` (`m4uhd`),
//!    `Neon` (`superflix`), `Helium` (`1movies`), with the vidking
//!    `Origin`/`Referer`. A 401 (stale seed) invalidates and retries
//!    once, per server.
//! 3. The wrapper's card naming: `Videasy | {quality} | {server}`
//!    (its `PROVIDER_NAME` template — upstream's `s.name || …`
//!    fallback; the port takes the rebranding template as the name
//!    since it is the `.to`-specific layer) plus the scraper's emoji
//!    card title and the vidking hotlink headers.
//! 4. The `VideasyTo.js` enrichment: the quality string is parsed
//!    into a height (`4k`/`2160` → 2160, explicit `1440`/`1080`/
//!    `720`/`480`/`360`, then the first 3–4-digit run; `_is4k` →
//!    2160; default 1080) and normalized to `{height}p` for
//!    `build_stream_results`.
//!    Duplicate URLs across servers collapse.
//! 5. Empty sweeps retry (up to two more, 2 s apart) — the
//!    speedracelight backend fails stochastically per sweep.
//!
//! Cuts for the library port:
//!
//! - The scraper's own TMDB details fetch (title/year/runtime for the
//!   card label) is cut: name/year come from the context/client, and
//!   the runtime line carries the scraper's 90-minute fallback.
//! - The obfuscated scraper's fetch-shim sweep timeouts (8 s first,
//!   12 s on retries) ride the shared client's per-request cap.
//! - The JS raced the whole resolution at 32 s; this port keeps the
//!   loop bounded by the same 32 s [`with_deadline`] wrap.
//! - The JS header comment describes a Playwright-based 9-provider
//!   flow (`Yoru`/`Cypher`/…); the deployed scraper has none — the
//!   delegation above is the live behavior.
//! - `meta.title` has no `StreamMeta` field — the emoji card title
//!   rides [`Stream::label`] via `build_stream_results`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;
use vsources_core::error::{FetchError, SourceError};
use vsources_core::tmdb::TmdbClient;
use vsources_core::traits::{ResolveCtx, Source};
use vsources_core::types::{CountryCode, MediaId, MediaRef, MediaType, SourceInfo, Stream};

use crate::nuvio::speedracelight::{Provider, ProviderQuery, SeedStore, fetch_provider};
use crate::nuvio::{BuildParams, NuvioStream, NuvioSubtitle, build_stream_results, with_deadline};

/// The player the source labels (upstream `this.baseUrl`).
const BASE_URL: &str = "https://player.videasy.to";
/// Upstream `this.ttl` — 10 min.
const TTL: Duration = Duration::from_mins(10);
/// The 32 s `Promise.race` cap around the whole resolution.
const DEADLINE: Duration = Duration::from_secs(32);
/// Empty-sweep retries after the first (upstream `EMPTY_RETRY_MAX`).
const EMPTY_RETRIES: u32 = 2;
/// The delay between empty sweeps (upstream `EMPTY_RETRY_DELAY_MS`).
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// The scraper's runtime fallback when TMDB details lack one.
const DEFAULT_RUNTIME_MIN: u32 = 90;

/// The ten videasy servers the delegated scraper sweeps (verbatim
/// order) — display name plus the `speedracelight` endpoint.
const SERVERS: [(&str, &str); 10] = [
    ("Hydrogen", "cdn/sources-with-title"),
    ("Titanium", "tejo/sources-with-title"),
    ("Oxygen", "neon2/sources-with-title"),
    ("Lithium", "downloader2/sources-with-title"),
    ("Krypton", "ym/sources-with-title"),
    ("Carbon", "mb-flix/sources-with-title"),
    ("Aluminium", "lamovie/sources-with-title"),
    ("Nitrogen", "m4uhd/sources-with-title"),
    ("Neon", "superflix/sources-with-title"),
    ("Helium", "1movies/sources-with-title"),
];

/// The `VideasyTo` provider.
pub struct VideasyTo {
    /// The descriptor served by [`Source::info`].
    info: SourceInfo,
    /// Shared TMDB identity resolution.
    tmdb: Arc<TmdbClient>,
    /// The shared speedracelight seed store (upstream's process-wide
    /// `srlSeed` singleton — one store across the speedracelight
    /// providers so their seeds do not invalidate each other).
    seeds: Arc<SeedStore>,
}

impl VideasyTo {
    /// A provider over the shared TMDB client and seed store.
    #[must_use]
    pub fn new(tmdb: Arc<TmdbClient>, seeds: Arc<SeedStore>) -> Self {
        Self {
            info: SourceInfo {
                id: "videasyto".to_string(),
                label: "Videasy.to".to_string(),
                content_types: vec![MediaType::Movie, MediaType::Series],
                country_codes: vec![CountryCode::Multi, CountryCode::En],
                base_url: Url::parse(BASE_URL).ok(),
                priority: 0,
                domain_key: None,
            },
            tmdb,
            seeds,
        }
    }

    /// One sweep over all ten servers — per-server failures contribute
    /// nothing (the obfuscated scraper's per-server catch), and a 401
    /// invalidates the seed and retries once per server.
    async fn sweep(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        year: Option<u16>,
        media: &MediaRef,
        tmdb_id: u64,
        imdb_id: Option<String>,
    ) -> Vec<Stream> {
        let query = ProviderQuery {
            title: name.to_string(),
            year,
            media_type: media_type(media).to_string(),
            tmdb_id,
            imdb_id,
            season_id: media.season.unwrap_or(1),
            episode_id: media.episode.unwrap_or(1),
        };

        let fetches = SERVERS.iter().map(|(server, endpoint)| {
            let provider = Provider {
                name: server,
                endpoint,
            };
            let seeds = Arc::clone(&self.seeds);
            let query = query.clone();
            async move {
                let json = match fetch_provider(ctx, &seeds, provider, &query).await {
                    Ok(json) => json,
                    // A 401 is a stale seed: invalidate and retry once.
                    Err(FetchError::Http { status: 401, .. }) => {
                        seeds.invalidate(query.tmdb_id);
                        fetch_provider(ctx, &seeds, provider, &query)
                            .await
                            .ok()
                            .flatten()
                    }
                    Err(_) => None,
                };
                (*server, json)
            }
        });
        let answers = futures::future::join_all(fetches).await;

        let mut seen = HashSet::new();
        let mut raw: Vec<NuvioStream> = Vec::new();
        for (server, json) in answers {
            let Some(json) = json else {
                continue;
            };
            let Some(sources) = json.get("sources").and_then(Value::as_array) else {
                continue;
            };
            for source in sources {
                let Some(url) = source.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if !url.starts_with("http") || !seen.insert(url.to_string()) {
                    continue;
                }
                raw.push(card(source, &json, url, name, year, media, server));
            }
        }

        let params = BuildParams {
            streams: &raw,
            title: &title_line(name, year, media),
            source_id: &self.info.id,
            source_label: &self.info.label,
            country_codes: &self.info.country_codes,
            ttl: TTL,
        };
        build_stream_results(&params)
    }

    /// The empty-sweep retry ladder — the wrapper's loop: a
    /// stochastically-empty sweep does not mean the title has none, so
    /// retry after the short pause. Bounded by [`DEADLINE`].
    async fn sweep_with_retries(
        &self,
        ctx: &ResolveCtx<'_>,
        name: &str,
        year: Option<u16>,
        media: &MediaRef,
        tmdb_id: u64,
        imdb_id: Option<String>,
    ) -> Vec<Stream> {
        let mut streams = self
            .sweep(ctx, name, year, media, tmdb_id, imdb_id.clone())
            .await;
        for _ in 0..EMPTY_RETRIES {
            if !streams.is_empty() {
                break;
            }
            tokio::time::sleep(RETRY_DELAY).await;
            streams = self
                .sweep(ctx, name, year, media, tmdb_id, imdb_id.clone())
                .await;
        }
        streams
    }
}

#[async_trait]
impl Source for VideasyTo {
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

        let streams = with_deadline(
            self.sweep_with_retries(ctx, &name, year, media, tmdb_id, imdb_id),
            DEADLINE,
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

/// `movie` / `tv` — the query's `mediaType`.
fn media_type(media: &MediaRef) -> &'static str {
    if media.season.is_some() {
        "tv"
    } else {
        "movie"
    }
}

/// One card from a decrypted source — the wrapper's
/// `Videasy | {quality} | {server}` name, the scraper's emoji title,
/// the vidking hotlink headers, and the `VideasyTo.js` quality
/// normalization (`{height}p`).
fn card(
    source: &Value,
    payload: &Value,
    url: &str,
    name: &str,
    year: Option<u16>,
    media: &MediaRef,
    server: &str,
) -> NuvioStream {
    let quality = source
        .get("quality")
        .and_then(Value::as_str)
        .filter(|quality| !quality.is_empty())
        .unwrap_or("1080p")
        .to_string();
    let badge = if quality.contains("2160") || quality.to_ascii_lowercase().contains("4k") {
        "🌟"
    } else if quality.contains("1080") {
        "🔥"
    } else {
        "⚡"
    };
    let format = if url.contains(".m3u8") {
        "M3U8"
    } else if url.contains(".mp4") {
        "MP4"
    } else {
        "MKV"
    };
    let provider_label = provider_label_of(url);
    let title = format!(
        "🎬 {media_line}\n{badge} {quality} | 🌍 Original Audio | 🎧 AAC\n🎞️ {format} | ⏱️ {DEFAULT_RUNTIME_MIN} min\n💧 {server} | 🔗 Provider: {provider_label}",
        media_line = media_line(name, year, media),
    );

    // The JS enrichment: parse the height, default to 1080, and
    // normalize the quality field to `{height}p`.
    let height = parse_height(&quality)
        .or_else(|| is_4k(source).then_some(2160))
        .unwrap_or(1080);

    let mut stream = NuvioStream::new(url)
        .with_name(format!("Videasy | {quality} | {server}"))
        .with_title(title)
        .with_quality(format!("{height}p"))
        .with_header("Referer", "https://www.vidking.net/")
        .with_header("Origin", "https://www.vidking.net")
        .with_header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36");
    if let Some(subtitles) = payload.get("subtitles").and_then(Value::as_array) {
        for subtitle in subtitles {
            let Some(sub_url) = subtitle.get("url").and_then(Value::as_str) else {
                continue;
            };
            let label = subtitle
                .get("label")
                .or_else(|| subtitle.get("lang"))
                .and_then(Value::as_str);
            stream = stream.with_subtitle(NuvioSubtitle {
                url: Some(sub_url.to_string()),
                label: label.map(str::to_string),
                ..NuvioSubtitle::default()
            });
        }
    }
    stream
}

/// Whether a decrypted source is 4K — the scraper's `_is4k` flag.
fn is_4k(source: &Value) -> bool {
    source
        .get("_is4k")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `VideasyTo.js`'s `parseHeight` — the explicit ladder first, then
/// the first 3–4-digit run.
fn parse_height(quality: &str) -> Option<u16> {
    let quality = quality.to_ascii_lowercase();
    if quality.contains("4k") || quality.contains("2160") {
        return Some(2160);
    }
    for (needle, height) in [
        ("1440", 1440),
        ("1080", 1080),
        ("720", 720),
        ("480", 480),
        ("360", 360),
    ] {
        if quality.contains(needle) {
            return Some(height);
        }
    }
    // The `/\d{3,4}/` run — the first 3- or 4-digit group.
    let mut run = String::new();
    for character in quality.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_digit() {
            run.push(character);
        } else if (3..=4).contains(&run.len()) {
            if let Ok(height) = run.parse::<u16>() {
                return Some(height);
            }
            run.clear();
        } else {
            run.clear();
        }
    }
    None
}

/// `Name - (2021)` for movies, `Name S2E3 - (2008)` for series — the
/// scraper's two label shapes.
fn media_line(name: &str, year: Option<u16>, media: &MediaRef) -> String {
    match (media.season, media.episode, year) {
        (Some(season), Some(episode), Some(year)) => {
            format!("{name} S{season}E{episode} - ({year})")
        }
        (Some(season), Some(episode), None) => format!("{name} S{season}E{episode}"),
        (_, _, Some(year)) => format!("{name} - ({year})"),
        (_, _, None) => name.to_string(),
    }
}

/// The provider label of a source URL — the host's first domain
/// label (`moon.peakstorm.top` → `moon`).
fn provider_label_of(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    host.split('.')
        .next()
        .filter(|label| !label.is_empty())
        .unwrap_or("cdn")
        .to_string()
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
            None => soften(tmdb.tmdb_id_from_imdb(imdb, media.kind).await),
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
    let name = soften(tmdb.name_and_year(tmdb_id, media.kind, None).await)?;
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use vsources_core::traits::{FetchRequest, FetchResponse, Fetcher, ResolvedMedia};
    use vsources_core::types::Format;

    use super::*;

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

    // -- the fixture-side cipher inverse --------------------------------
    //
    // The same keystream as `decrypt_payload`, used to encrypt test
    // payloads byte-for-byte (verified against the real JS cipher via
    // the module's ground-truth payloads).

    /// The murmur-style finalizer.
    fn finalize(mut value: u32) -> u32 {
        value ^= value >> 16;
        value = value.wrapping_mul(2_246_822_507);
        value ^= value >> 13;
        value = value.wrapping_mul(3_266_489_909);
        value ^= value >> 16;
        value
    }

    /// The FNV-1a + finalizer hash.
    fn fnv_mixed(text: &str) -> u32 {
        let mut hash: u32 = 2_166_136_261;
        for character in text.chars() {
            hash = (hash ^ u32::from(character)).wrapping_mul(16_777_619);
        }
        finalize(hash)
    }

    /// Rotate-left with the `o &= 31` guard.
    fn rotl(value: u32, shift: u32) -> u32 {
        let shift = shift & 31;
        if shift == 0 {
            return value;
        }
        value.rotate_left(shift)
    }

    /// The per-seed state — with the sparse-array slot tracking the
    /// real JS relies on.
    struct FixtureState {
        table: Vec<u32>,
        initialized: Vec<bool>,
        acc: u32,
    }

    /// The state init.
    fn new_state(seed: &str, media_id: u32) -> FixtureState {
        const STATE_LEN: u32 = 61;
        let rounds = 8u32;
        let ms = 2_654_435_769u32;
        let mut table = vec![0u32; STATE_LEN as usize];
        let mut initialized = vec![false; STATE_LEN as usize];
        let mut index = finalize(fnv_mixed(seed) ^ finalize(media_id ^ ms));
        for round in 0..rounds {
            let slot = index % STATE_LEN;
            index = rotl(index.wrapping_add(ms), 7 + (round & 0x7));
            table[slot as usize] = index ^ finalize(index);
            initialized[slot as usize] = true;
            index = finalize(index.wrapping_add(slot));
        }
        FixtureState {
            table,
            initialized,
            acc: finalize(index ^ 0xA5A5_A5A5),
        }
    }

    /// The keystream.
    fn keystream(seed: &str, media_id: u32, length: usize) -> Vec<u8> {
        let mut state = new_state(seed, media_id);
        let mut out = Vec::with_capacity(length);
        let mut counter = 0u32;
        while out.len() < length {
            let slot = state.acc % 61;
            let index = slot as usize;
            let d = 2_654_435_769u32.wrapping_mul(counter.wrapping_add(1));
            let mixed = if state.initialized[index] {
                state.acc | (state.table[index] ^ d)
            } else {
                state.acc ^ d
            };
            let rotated = rotl(mixed.wrapping_add(state.acc), slot & 0x1F)
                ^ rotl(state.acc, slot.wrapping_mul(7) & 0x1F);
            let value = finalize(rotated.wrapping_add(2_654_435_769));
            state.table[index] = value;
            state.initialized[index] = true;
            state.acc = value;
            counter = counter.wrapping_add(1);
            for shift in [0, 8, 16, 24] {
                if out.len() < length {
                    out.push(((value >> shift) & 0xff) as u8);
                }
            }
        }
        out
    }

    /// Encrypt a JSON payload for `seed`/`tmdb` — the test-side
    /// inverse of `decrypt_payload`.
    fn encrypt_fixture(json: &str, seed: &str, tmdb: u64) -> String {
        let mut body = vec![109, 118, 109, 49];
        body.extend_from_slice(json.as_bytes());
        let key = keystream(seed, u32::try_from(tmdb).unwrap_or(0), body.len());
        let encrypted: Vec<u8> = body.iter().zip(key.iter()).map(|(b, k)| b ^ k).collect();
        base64_encode_urlsafe(&encrypted)
    }

    /// URL-safe base64 without padding.
    fn base64_encode_urlsafe(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            let chars = [
                ALPHABET[(triple >> 18 & 0x3f) as usize],
                ALPHABET[(triple >> 12 & 0x3f) as usize],
                ALPHABET[(triple >> 6 & 0x3f) as usize],
                ALPHABET[(triple & 0x3f) as usize],
            ];
            match chunk.len() {
                1 => out.push_str(std::str::from_utf8(&chars[..2]).unwrap_or("==")),
                2 => out.push_str(std::str::from_utf8(&chars[..3]).unwrap_or("=")),
                _ => out.push_str(std::str::from_utf8(&chars).unwrap_or("")),
            }
        }
        out
    }

    // -- fixtures ------------------------------------------------------------

    /// The fixture seed and media id every payload encrypts under.
    const SEED: &str = "fixture-seed-02";
    const TMDB: u64 = 693_134;

    /// A provider over the mock and a fresh seed store.
    fn provider(fetcher: &Arc<MockFetcher>) -> VideasyTo {
        VideasyTo::new(
            Arc::new(TmdbClient::new("test-key", fetcher.clone())),
            Arc::new(SeedStore::new()),
        )
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

    /// A resolved Dune: Part Two movie.
    fn dune_media() -> ResolvedMedia {
        ResolvedMedia {
            tmdb_id: Some(TMDB),
            imdb_id: Some("tt15239678".to_string()),
            name: "Dune: Part Two".to_string(),
            year: Some(2024),
            season: None,
            episode: None,
        }
    }

    /// The mock seed page.
    fn seed_page() -> String {
        format!(r#"{{"seed":"{SEED}"}}"#)
    }

    /// The ground-truth payload from the shared module's tests,
    /// re-encrypted for this fixture's seed — proving the fixture
    /// cipher matches the real one.
    #[test]
    fn fixture_cipher_matches_the_shared_ground_truth() {
        use crate::nuvio::speedracelight::decrypt_payload;
        let ground_seed = "a1b2c3d4e5f6a7b8";
        let ground_tmdb = 123_456u64;
        let json = r#"{"sources":[{"url":"https://cdn.example.com/yoru/master.m3u8","quality":"1080p"}],"subtitles":[{"url":"https://subs.example.com/en.vtt","label":"English"}]}"#;
        let encrypted = encrypt_fixture(json, ground_seed, ground_tmdb);
        assert_eq!(
            decrypt_payload(&encrypted, ground_seed, ground_tmdb).as_deref(),
            Some(json)
        );
    }

    #[test]
    fn parse_height_maps_the_js_ladder() {
        assert_eq!(parse_height("4K"), Some(2160));
        assert_eq!(parse_height("2160p"), Some(2160));
        assert_eq!(parse_height("1440p UHD"), Some(1440));
        assert_eq!(parse_height("1080p"), Some(1080));
        assert_eq!(parse_height("HD 720"), Some(720));
        assert_eq!(parse_height("480p"), Some(480));
        assert_eq!(parse_height("360"), Some(360));
        // The `/\d{3,4}/` run matches "256".
        assert_eq!(parse_height("x256"), Some(256));
        assert_eq!(parse_height(""), None);
    }

    #[test]
    fn info_describes_the_source() {
        let mock = Arc::new(MockFetcher::new());
        let source = provider(&mock);
        let info = source.info();
        assert_eq!(info.id, "videasyto");
        assert_eq!(info.label, "Videasy.to");
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
            Some("https://player.videasy.to/")
        );
        assert_eq!(info.priority, 0);
    }

    #[tokio::test]
    async fn resolves_streams_with_normalized_quality() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "sources": [
                { "url": "https://moon.peakstorm.top/vd/x1/index-s4k-v1-a1.m3u8", "quality": "4K" },
                { "url": "https://sun.peakstorm.top/vd/x1/index-s1080p-v1-a1.m3u8", "quality": "1080p" },
                { "url": "https://sun.peakstorm.top/vd/x1/index-odd-v1-a1.m3u8", "quality": "" },
                { "url": "not-a-url", "quality": "480p" }
            ],
            "subtitles": [ { "url": "https://subs.example.com/en.vtt", "label": "English" } ]
        });
        let encrypted = encrypt_fixture(&payload.to_string(), SEED, TMDB);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api.speedracelight.com/seed", 200, seed_page())
                .serve(
                    "api.speedracelight.com/cdn/sources-with-title",
                    200,
                    encrypted,
                )
                .serve("api.speedracelight.com/tejo/sources-with-title", 404, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await
            .unwrap_or_else(|e| panic!("the movie fixture must resolve: {e}"));

        // The non-http source is dropped; the three survivors carry
        // normalized heights (4K → 2160, 1080p, empty → 1080 default).
        assert_eq!(streams.len(), 3);
        let first = &streams[0];
        assert_eq!(
            first.url.as_str(),
            "https://moon.peakstorm.top/vd/x1/index-s4k-v1-a1.m3u8"
        );
        assert_eq!(first.format, Format::Hls);
        assert_eq!(first.meta.resolution, Some(2160));
        assert_eq!(streams[1].meta.resolution, Some(1080));
        assert_eq!(streams[2].meta.resolution, Some(1080));
        assert_eq!(first.meta.source_id.as_deref(), Some("videasyto"));
        assert_eq!(first.meta.source_label.as_deref(), Some("Videasy.to"));
        assert_eq!(first.ttl, TTL);
        // The wrapper's rebranded name is the label feed.
        let label = first.label.as_deref().unwrap_or_default();
        assert!(
            label.starts_with("Dune: Part Two (2024) — 🎬 Dune: Part Two - (2024)"),
            "{label}"
        );
        assert!(label.contains("🌟 4K"), "{label}");
        assert!(label.contains("💧 Hydrogen | 🔗 Provider: moon"), "{label}");
        // The vidking hotlink headers the scraper attached.
        assert_eq!(
            first
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://www.vidking.net/")
        );
        assert_eq!(
            first.meta.request_headers.get("Origin").map(String::as_str),
            Some("https://www.vidking.net")
        );
        // The subtitle rode along.
        assert_eq!(first.meta.subtitles.len(), 1);
        assert_eq!(
            first.meta.subtitles[0].url.as_str(),
            "https://subs.example.com/en.vtt"
        );

        // The query carried the meta the scraper sends.
        let query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/cdn/sources-with-title")
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(query.contains("tmdbId=693134"), "{query}");
        assert!(query.contains("mediaType=movie"), "{query}");
        assert!(query.contains("title=Dune"), "{query}");
        assert!(query.contains("imdbId=tt15239678"), "{query}");
        assert!(query.contains("enc=2"), "{query}");
        Ok(())
    }

    #[tokio::test]
    async fn series_queries_the_season_episode_ids() -> Result<(), SourceError> {
        let payload = serde_json::json!({
            "sources": [ { "url": "https://moon.peakstorm.top/vd/bb/index-s720p-v1-a1.m3u8", "quality": "720p" } ]
        });
        let encrypted = encrypt_fixture(&payload.to_string(), SEED, 1396);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api.speedracelight.com/seed", 200, seed_page())
                .serve(
                    "api.speedracelight.com/cdn/sources-with-title",
                    200,
                    encrypted,
                )
                .serve("api.speedracelight.com/tejo/sources-with-title", 404, ""),
        );
        let ctx = ResolveCtx {
            fetcher: fetcher.as_ref(),
            media: Some(ResolvedMedia {
                tmdb_id: Some(1396),
                imdb_id: None,
                name: "Breaking Bad".to_string(),
                year: Some(2008),
                season: Some(1),
                episode: Some(1),
            }),
            source_id: None,
            referer: None,
        };
        let media = MediaRef::series(MediaId::Tmdb(1396), 1, 1);

        let streams = provider(&fetcher)
            .resolve(&ctx, &media)
            .await
            .unwrap_or_else(|e| panic!("the series fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        let query = fetcher
            .requests()
            .iter()
            .find(|request| request.url.path() == "/cdn/sources-with-title")
            .map(|request| request.url.query().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(query.contains("mediaType=tv"), "{query}");
        assert!(query.contains("seasonId=1"), "{query}");
        assert!(query.contains("episodeId=1"), "{query}");
        // The series label carries S1E1 in the card title.
        let label = streams[0].label.as_deref().unwrap_or_default();
        assert!(label.contains("S1E1 - (2008)"), "{label}");
        assert!(label.contains("S01E01"), "{label}");
        Ok(())
    }

    #[tokio::test]
    async fn empty_sweeps_retry_until_a_server_delivers() {
        // The first sweep: every server 404s. The retry: Hydrogen
        // delivers. The seed is cached, so no extra /seed hits.
        let payload = serde_json::json!({
            "sources": [ { "url": "https://moon.peakstorm.top/vd/l/index-s1080p-v1-a1.m3u8", "quality": "1080p" } ]
        });
        let encrypted = encrypt_fixture(&payload.to_string(), SEED, TMDB);
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api.speedracelight.com/seed", 200, seed_page())
                .serve("api.speedracelight.com/cdn/sources-with-title", 404, "")
                .serve("api.speedracelight.com/cdn/sources-with-title", 404, "")
                .serve(
                    "api.speedracelight.com/cdn/sources-with-title",
                    200,
                    encrypted,
                ),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));

        let streams = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await
            .unwrap_or_else(|e| panic!("the retry fixture must resolve: {e}"));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].meta.resolution, Some(1080));
    }

    #[tokio::test]
    async fn all_servers_down_is_not_found() {
        let fetcher = Arc::new(
            MockFetcher::new()
                .serve("api.speedracelight.com/seed", 200, seed_page())
                .serve("api.speedracelight.com/cdn/sources-with-title", 404, "")
                .serve("api.speedracelight.com/tejo/sources-with-title", 404, ""),
        );
        let ctx = ctx_for(&fetcher, Some(dune_media()));
        let result = provider(&fetcher)
            .resolve(&ctx, &MediaRef::movie(MediaId::Tmdb(TMDB)))
            .await;
        assert!(matches!(result, Err(SourceError::NotFound)));
    }
}
