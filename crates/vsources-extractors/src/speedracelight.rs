//! The speedracelight API client — the port of
//! `src/utils/speedracelight.js` + the shared seed store of
//! `src/utils/srlSeed.cjs`.
//!
//! vidking.net's backend (`api.speedracelight.com`) answers with
//! base64+XOR-encrypted JSON:
//!
//! 1. [`fetch_seed`] — `GET /seed?mediaId={tmdbId}` → `{ seed }`
//!    (rotated per request; cached 25 s with in-flight coalescing and a
//!    120 s upstream-down fast-fail);
//! 2. [`fetch_provider`] — `GET /{endpoint}?…&seed=…` → the decrypted
//!    `{ sources: [{url, quality, …}], subtitles }` payload;
//! 3. [`fetch_all_providers`] — both providers in parallel, with the
//!    seed-invalidating 401 retry.
//!
//! All requests must carry `Origin: https://www.vidking.net` and
//! `Referer: https://www.vidking.net/`, or the API 403s.
//!
//! The keystream (`xf`/`Rf`/`Cf` in the bundle) is a 61-entry
//! xorshift-style stream; the two dead-code branches of the original
//! (`If`/`bf` test `n·(n+1) & 1`, which is always even → always the
//! same path) are collapsed here.
//!
//! Cuts versus the JS: the module-level singleton store becomes an
//! explicit [`SeedStore`] the caller owns; the seed-fetch in-flight
//! coalescing serializes per media id (upstream shared a promise —
//! same observable behavior for concurrent callers).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use vsources_core::error::FetchError;
use vsources_core::traits::{FetchRequest, ResolveCtx};

/// The API root.
pub const SPEEDRACELIGHT_API_BASE: &str = "https://api.speedracelight.com";
/// The upstream player the API gates its CORS on.
const VIDKING_ORIGIN: &str = "https://www.vidking.net";
/// The hotlink Referer.
const VIDKING_REFERER: &str = "https://www.vidking.net/";

/// The `"mvm1"` magic prefix every payload starts with.
const MAGIC: [u8; 4] = [109, 118, 109, 49];
/// The golden-ratio multiplier `ms`.
const MS: u32 = 2_654_435_769;
/// The state array size `Js`.
const STATE_LEN: u32 = 61;
/// The setup rounds `Sf`.
const ROUNDS: u32 = 8;
/// The final-state twist constant.
const FINAL_XOR: u32 = 2_779_096_485;

/// One provider entry of the bundle's registry (`Vr`) — trimmed to the
/// two reliable backends, exactly like the JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// The display name (`Yoru`, `Omen`).
    pub name: &'static str,
    /// The API endpoint under [`SPEEDRACELIGHT_API_BASE`].
    pub endpoint: &'static str,
}

/// The provider registry — Yoru (direct MP4/HLS) and Omen
/// (vimeos.net HLS), the two the JS kept after dropping the flaky
/// five.
pub const PROVIDERS: [Provider; 2] = [
    Provider {
        name: "Yoru",
        endpoint: "cdn/sources-with-title",
    },
    Provider {
        name: "Omen",
        endpoint: "lamovie/sources-with-title",
    },
];

/// The murmur-style finalizer `ci(l)`: xor-shift, two multiplies, two
/// more xor-shifts, all in u32.
fn finalize(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(2_246_822_507);
    value ^= value >> 13;
    value = value.wrapping_mul(3_266_489_909);
    value ^= value >> 16;
    value
}

/// Rotate-left `ps(l, o)` with the `o &= 31` guard.
fn rotl(value: u32, shift: u32) -> u32 {
    let shift = shift & 31;
    if shift == 0 {
        return value;
    }
    value.rotate_left(shift)
}

/// The FNV-1a + finalizer hash `vf(l)` (char-code arithmetic, like the
/// JS `charCodeAt`).
fn fnv_mixed(text: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for character in text.chars() {
        hash = (hash ^ u32::from(character)).wrapping_mul(16_777_619);
    }
    finalize(hash)
}

/// The per-seed state `Rf(l, o)`: a 61-slot table plus an accumulator.
struct StreamState {
    /// The `S` table.
    table: Vec<u32>,
    /// Which slots were ever assigned — the JS table is a SPARSE
    /// `new Array(61)` (only the 8 setup rounds write it), and `Cf`
    /// masks with `0 - +(r in e)`: a hole contributes neither its
    /// (undefined → 0) table value NOR the `a & b` half of `Nf`.
    initialized: Vec<bool>,
    /// The `acc`.
    acc: u32,
}

impl StreamState {
    /// Build the state — the only live branch of `Rf` (the `If`
    /// branch is unreachable: `n·(n+1)` is always even).
    fn new(seed: &str, media_id: u32) -> Self {
        let mut table = vec![0u32; STATE_LEN as usize];
        let mut initialized = vec![false; STATE_LEN as usize];
        let mut index = finalize(fnv_mixed(seed) ^ finalize(media_id ^ MS));
        for round in 0..ROUNDS {
            // `bf(r)` is always true — the else branch never runs.
            let slot = index % STATE_LEN;
            index = rotl(index.wrapping_add(MS), 7 + (round & 7));
            table[slot as usize] = index ^ finalize(index);
            initialized[slot as usize] = true;
            index = finalize(index.wrapping_add(slot));
        }
        Self {
            table,
            initialized,
            acc: finalize(index ^ FINAL_XOR),
        }
    }

    /// Advance the state — `Cf(l, o)`. The `Nf` combine is
    /// `(a ^ b) | (a & b & n)`: for an assigned slot `n` is `-1` (all
    /// ones) so it folds to `a | b`, while a hole has `n = 0` and a
    /// table value of `undefined >>> 0 = 0`, leaving `a ^ d`.
    /// Writing the slot at the end makes it assigned for later calls.
    fn next(&mut self, counter: u32) -> u32 {
        let slot = self.acc % STATE_LEN;
        let index = slot as usize;
        let d = MS.wrapping_mul(counter.wrapping_add(1));
        let mixed = if self.initialized[index] {
            self.acc | (self.table[index] ^ d)
        } else {
            self.acc ^ d
        };
        let rotated = rotl(mixed.wrapping_add(self.acc), slot & 31)
            ^ rotl(self.acc, slot.wrapping_mul(7) & 31);
        let value = finalize(rotated.wrapping_add(MS));
        self.table[index] = value;
        self.initialized[index] = true;
        self.acc = value;
        value
    }
}

/// The keystream `xf(l, o, e)` — 4 little-endian bytes per state
/// advance.
fn keystream(seed: &str, media_id: u32, length: usize) -> Vec<u8> {
    let mut state = StreamState::new(seed, media_id);
    let mut out = Vec::with_capacity(length);
    let mut counter = 0u32;
    while out.len() < length {
        let word = state.next(counter);
        counter = counter.wrapping_add(1);
        for shift in [0, 8, 16, 24] {
            if out.len() < length {
                out.push(((word >> shift) & 0xff) as u8);
            }
        }
    }
    out
}

/// Decrypt a response body (urlsafe-base64 + XOR) → the JSON text —
/// ports `decryptPayload`. `None` when the base64, the `"mvm1"` magic,
/// or the UTF-8 decode fails.
#[must_use]
pub fn decrypt_payload(payload: &str, seed: &str, tmdb_id: u64) -> Option<String> {
    let standard = payload.replace('-', "+").replace('_', "/");
    let padded = format!("{standard}{}", "=".repeat((4 - standard.len() % 4) % 4));
    let body = STANDARD.decode(padded).ok()?;
    if body.len() < MAGIC.len() {
        return None;
    }
    let media_id = u32::try_from(tmdb_id).ok()?;
    let key = keystream(seed, media_id, body.len());
    let plain: Vec<u8> = body
        .iter()
        .zip(key.iter())
        .map(|(byte, key_byte)| byte ^ key_byte)
        .collect();
    if plain[..MAGIC.len()] != MAGIC {
        return None;
    }
    String::from_utf8(plain[MAGIC.len()..].to_vec()).ok()
}

/// One cached seed with its expiry.
struct SeedEntry {
    /// The seed value.
    seed: String,
    /// When the entry was stored.
    stored_at: Instant,
}

/// The shared seed store — the port of `srlSeed.cjs`: a 25 s TTL cache
/// (the server's TTL is ~30 s), per-media-id fetch serialization (the
/// JS's in-flight promise registry), and a 120 s upstream-down mark
/// for confirmed edge 5xx answers.
pub struct SeedStore {
    /// Cached seeds, keyed by TMDB id.
    seeds: Mutex<HashMap<u64, SeedEntry>>,
    /// In-flight guards per media id — concurrent callers coalesce.
    in_flight: Mutex<HashMap<u64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// When the upstream-down mark expires.
    down_until: Mutex<Option<Instant>>,
}

impl Default for SeedStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SeedStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            seeds: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
            down_until: Mutex::new(None),
        }
    }

    /// The seed cache TTL (25 s — server TTL is ~30 s).
    const SEED_TTL: Duration = Duration::from_secs(25);
    /// The upstream-down mark TTL.
    const DOWN_TTL: Duration = Duration::from_secs(120);

    /// Whether a confirmed edge 5xx has the API marked down.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.down_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|until| Instant::now() < until)
    }

    /// Mark the API down (definitive ≥ 500 on a fresh `/seed` probe).
    pub fn mark_down(&self) {
        *self
            .down_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Instant::now() + Self::DOWN_TTL);
    }

    /// Drop a cached seed (after a 401 — the bundle's `Lf()`
    /// invalidation).
    pub fn invalidate(&self, tmdb_id: u64) {
        self.seeds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&tmdb_id);
    }

    /// The cached seed, when fresh.
    fn cached(&self, tmdb_id: u64) -> Option<String> {
        let mut seeds = self
            .seeds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match seeds.get(&tmdb_id) {
            Some(entry) if entry.stored_at.elapsed() < Self::SEED_TTL => Some(entry.seed.clone()),
            Some(_) => {
                seeds.remove(&tmdb_id);
                None
            }
            None => None,
        }
    }

    /// Store a fresh seed.
    fn store(&self, tmdb_id: u64, seed: &str) {
        self.seeds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                tmdb_id,
                SeedEntry {
                    seed: seed.to_string(),
                    stored_at: Instant::now(),
                },
            );
    }

    /// The per-id in-flight guard — concurrent callers for the same
    /// media id serialize here (the JS coalesced via a shared promise).
    fn in_flight_guard(&self, tmdb_id: u64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(tmdb_id)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// The `/seed` response shape.
#[derive(Deserialize)]
struct SeedResponse {
    /// The rotated seed.
    seed: String,
}

/// Fetch (or reuse) the seed for a media id — ports `fetchSeed`: the
/// fast-fail on a down mark, the 25 s cache, per-id coalescing, and
/// up to 3 attempts honoring 429 `Retry-After` (capped at 10 s, 2 s
/// default). A definitive ≥ 500 marks the API down for 120 s.
pub async fn fetch_seed(
    ctx: &ResolveCtx<'_>,
    store: &SeedStore,
    tmdb_id: u64,
) -> Result<String, FetchError> {
    if store.is_down() {
        return Err(FetchError::Transport {
            url: Url::parse(&format!("{SPEEDRACELIGHT_API_BASE}/seed"))
                .unwrap_or_else(|e| panic!("valid seed URL: {e}")),
            message: "speedracelight down (cached edge 5xx) — fast-fail".to_string(),
        });
    }
    if let Some(seed) = store.cached(tmdb_id) {
        return Ok(seed);
    }

    // Coalesce concurrent fetches for the same media id.
    let guard = store.in_flight_guard(tmdb_id);
    let _release = guard.lock().await;
    if let Some(seed) = store.cached(tmdb_id) {
        return Ok(seed);
    }

    let url = Url::parse(&format!(
        "{SPEEDRACELIGHT_API_BASE}/seed?mediaId={}",
        urlencode(&tmdb_id.to_string())
    ))
    .unwrap_or_else(|e| panic!("valid seed URL: {e}"));

    let mut last_error: Option<FetchError> = None;
    for attempt in 0..3 {
        let request = FetchRequest::get(url.clone())
            .with_header("Origin", VIDKING_ORIGIN)
            .with_header("Referer", VIDKING_REFERER)
            .with_header("Accept", "application/json")
            .with_timeout(Duration::from_secs(10));
        match ctx.fetcher.request(request).await {
            Ok(response) if response.is_success() => {
                let payload: SeedResponse =
                    response.json().map_err(|error| FetchError::Transport {
                        url: url.clone(),
                        message: format!("seed response missing seed field: {error}"),
                    })?;
                if payload.seed.is_empty() {
                    return Err(FetchError::Transport {
                        url,
                        message: "seed response missing seed field".to_string(),
                    });
                }
                store.store(tmdb_id, &payload.seed);
                return Ok(payload.seed);
            }
            Ok(response) => {
                // A definitive edge 5xx proves the API is down for
                // everyone — mark it (timeouts and 4xx never do) and
                // stop paying the remaining attempts.
                if response.status >= 500 {
                    store.mark_down();
                    break;
                }
                last_error = Some(FetchError::Http {
                    url: url.clone(),
                    status: response.status,
                });
            }
            Err(error @ FetchError::RateLimited { .. }) => {
                let wait = match &error {
                    FetchError::RateLimited { retry_after_ms, .. } => retry_after_ms
                        .filter(|ms| *ms > 0)
                        .map_or(Duration::from_secs(2), |ms| {
                            Duration::from_millis(ms.min(10_000))
                        }),
                    _ => Duration::from_secs(2),
                };
                last_error = Some(error);
                if attempt < 2 {
                    tokio::time::sleep(wait).await;
                }
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| FetchError::Transport {
        url,
        message: "seed fetch failed".to_string(),
    }))
}

/// The metadata a provider query needs.
#[derive(Debug, Clone)]
pub struct ProviderQuery {
    /// The media title.
    pub title: String,
    /// The release year.
    pub year: Option<u16>,
    /// The media type (`movie` / `tv`).
    pub media_type: String,
    /// The TMDB id.
    pub tmdb_id: u64,
    /// The `IMDb` id, when known.
    pub imdb_id: Option<String>,
    /// The season id (1-based).
    pub season_id: u32,
    /// The episode id (1-based).
    pub episode_id: u32,
}

/// The `Retry-After`-aware GET of one provider's payload — ports
/// `fetchProvider`. Returns `Ok(None)` on non-401 HTTP failures and
/// malformed JSON; a 401 (or a failed decrypt, the JS's synthetic 401)
/// returns `Err(Http{401})` so the caller can invalidate and retry.
pub async fn fetch_provider(
    ctx: &ResolveCtx<'_>,
    store: &SeedStore,
    provider: Provider,
    query: &ProviderQuery,
) -> Result<Option<Value>, FetchError> {
    let seed = fetch_seed(ctx, store, query.tmdb_id).await?;

    let target = format!(
        "{}/{}?title={}&mediaType={}&year={}&episodeId={}&seasonId={}&tmdbId={}&imdbId={}&enc=2&seed={}&_t={}",
        SPEEDRACELIGHT_API_BASE,
        provider.endpoint,
        urlencode(&query.title),
        urlencode(&query.media_type),
        query.year.map(|y| y.to_string()).unwrap_or_default(),
        query.episode_id,
        query.season_id,
        query.tmdb_id,
        query.imdb_id.as_deref().unwrap_or_default(),
        urlencode(&seed),
        now_millis(),
    );
    let url = Url::parse(&target).map_err(|_| FetchError::Transport {
        url: Url::parse(SPEEDRACELIGHT_API_BASE).unwrap_or_else(|e| panic!("valid base: {e}")),
        message: format!("invalid provider URL for {}", provider.name),
    })?;
    let request = FetchRequest::get(url.clone())
        .with_header("Origin", VIDKING_ORIGIN)
        .with_header("Referer", VIDKING_REFERER)
        .with_header("Cache-Control", "no-cache, no-store, must-revalidate")
        .with_header("Pragma", "no-cache")
        .with_header("Expires", "0")
        .with_timeout(Duration::from_secs(15));
    let response = ctx.fetcher.request(request).await?;
    if !response.is_success() {
        if response.status == 401 {
            return Err(FetchError::Http {
                url: url.clone(),
                status: 401,
            });
        }
        return Ok(None);
    }

    // A failed decrypt means a stale seed — the JS's synthetic 401.
    let Some(decrypted) = decrypt_payload(&response.body, &seed, query.tmdb_id) else {
        return Err(FetchError::Http {
            url: url.clone(),
            status: 401,
        });
    };
    let Ok(json) = serde_json::from_str::<Value>(&decrypted) else {
        return Ok(None);
    };
    if json.get("sources").and_then(Value::as_array).is_some() {
        Ok(Some(json))
    } else {
        Ok(None)
    }
}

/// Fetch all providers in parallel — ports `fetchAllProviders`: each
/// provider's 401 invalidates the seed and retries once; failures
/// answer `None` (the JS `{provider, json: null}`).
pub async fn fetch_all_providers(
    ctx: &ResolveCtx<'_>,
    store: &SeedStore,
    query: &ProviderQuery,
) -> Vec<(Provider, Option<Value>)> {
    let fetches = PROVIDERS.iter().map(|provider| {
        let provider = *provider;
        async move {
            let first = fetch_provider(ctx, store, provider, query).await;
            let json = match first {
                Ok(json) => json,
                Err(FetchError::Http { status: 401, .. }) => {
                    store.invalidate(query.tmdb_id);
                    fetch_provider(ctx, store, provider, query)
                        .await
                        .ok()
                        .flatten()
                }
                Err(_) => None,
            };
            (provider, json)
        }
    });
    futures::future::join_all(fetches).await
}

/// `Date.now()` — the cache-busting `_t` parameter.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// Component-level percent encoding for query values.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use vsources_core::traits::{FetchResponse, Fetcher, ResolveCtx};

    use super::*;

    /// A fetcher answering canned bodies keyed by path.
    struct ScriptedFetcher {
        /// URL path → (status, body).
        pages: Mutex<HashMap<String, (u16, String)>>,
        /// The requests seen so far.
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl ScriptedFetcher {
        /// A fetcher serving `path` with `status`/`body`.
        fn page(self, path: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.into(), (status, body.into()));
            self
        }

        /// How many requests hit `path`.
        fn hits(&self, path: &str) -> usize {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|request| request.url.path() == path)
                .count()
        }
    }

    impl Default for ScriptedFetcher {
        fn default() -> Self {
            Self {
                pages: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Fetcher for ScriptedFetcher {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            let path = request.url.path().to_string();
            let (status, body) = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&path)
                .cloned()
                .unwrap_or((404, String::new()));
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: std::collections::BTreeMap::new(),
                body,
            })
        }
    }

    /// A resolve context over the scripted fetcher.
    fn ctx_for(fetcher: &ScriptedFetcher) -> ResolveCtx<'_> {
        ResolveCtx {
            fetcher,
            media: None,
            source_id: None,
            referer: None,
        }
    }

    /// The ground-truth payload: encrypting the JSON with seed
    /// `a1b2c3d4e5f6a7b8` / TMDB 123456 via the same keystream (Node
    /// reproduction of the JS).
    const GROUND_TRUTH_PAYLOAD: &str = "dfS4GATTVMdIPIt14QZYTraUVVwxDX_-BD_Vx8QsdCkZo0M7i3nsqCa6IL5M7qFN0EPpGRiD69vMbws-4I5m6Rl8eeRysj30vEp6ukKnljea0adax-w9CXxFcuFgFpligneO6dT_AtmPeMsVpcmiPEYIStUqC-muhjObHMzlCVpJahcxBCgW7QESSv5Ykr3V07PhypGm2uyF-RK5Ct2QcA";
    const GROUND_TRUTH_SEED: &str = "a1b2c3d4e5f6a7b8";
    const GROUND_TRUTH_TMDB: u64 = 123_456;
    const GROUND_TRUTH_JSON: &str = "{\"sources\":[{\"url\":\"https://cdn.example.com/yoru/master.m3u8\",\"quality\":\"1080p\"}],\"subtitles\":[{\"url\":\"https://subs.example.com/en.vtt\",\"label\":\"English\"}]}";

    #[test]
    fn decrypts_the_ground_truth_payload() {
        assert_eq!(
            decrypt_payload(GROUND_TRUTH_PAYLOAD, GROUND_TRUTH_SEED, GROUND_TRUTH_TMDB).as_deref(),
            Some(GROUND_TRUTH_JSON)
        );
    }

    #[test]
    fn fails_closed_on_bad_payloads() {
        assert!(decrypt_payload("", GROUND_TRUTH_SEED, GROUND_TRUTH_TMDB).is_none());
        assert!(decrypt_payload("!!!", GROUND_TRUTH_SEED, GROUND_TRUTH_TMDB).is_none());
        // Wrong seed → the magic check fails.
        assert!(decrypt_payload(GROUND_TRUTH_PAYLOAD, "wrong-seed", GROUND_TRUTH_TMDB).is_none());
        // Wrong media id → same.
        assert!(decrypt_payload(GROUND_TRUTH_PAYLOAD, GROUND_TRUTH_SEED, 999).is_none());
    }

    #[tokio::test]
    async fn fetches_and_caches_the_seed() -> Result<(), FetchError> {
        let fetcher = ScriptedFetcher::default().page("/seed", 200, r#"{"seed":"s3ed"}"#);
        let ctx = ctx_for(&fetcher);
        let store = SeedStore::new();
        let seed = fetch_seed(&ctx, &store, 42).await?;
        assert_eq!(seed, "s3ed");
        // A second fetch is served from the cache — no second request.
        let again = fetch_seed(&ctx, &store, 42).await?;
        assert_eq!(again, "s3ed");
        assert_eq!(fetcher.hits("/seed"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn marks_down_on_edge_5xx() {
        let fetcher = ScriptedFetcher::default().page("/seed", 502, "");
        let ctx = ctx_for(&fetcher);
        let store = SeedStore::new();
        assert!(fetch_seed(&ctx, &store, 7).await.is_err());
        assert!(store.is_down());
        // The down mark fast-fails without another request.
        let before = fetcher.hits("/seed");
        assert!(fetch_seed(&ctx, &store, 7).await.is_err());
        assert_eq!(fetcher.hits("/seed"), before);
    }

    #[tokio::test]
    async fn fetches_a_provider_payload() {
        let seed_page = format!(r#"{{"seed":"{GROUND_TRUTH_SEED}"}}"#);
        let fetcher = ScriptedFetcher::default()
            .page("/seed", 200, seed_page)
            .page("/cdn/sources-with-title", 200, GROUND_TRUTH_PAYLOAD)
            .page("/lamovie/sources-with-title", 404, "");
        let ctx = ctx_for(&fetcher);
        let store = SeedStore::new();
        let query = ProviderQuery {
            title: "Dune".to_string(),
            year: Some(2021),
            media_type: "movie".to_string(),
            tmdb_id: GROUND_TRUTH_TMDB,
            imdb_id: None,
            season_id: 1,
            episode_id: 1,
        };
        let results = fetch_all_providers(&ctx, &store, &query).await;
        assert_eq!(results.len(), 2);
        let yoru = results
            .iter()
            .find(|(provider, _)| provider.name == "Yoru")
            .and_then(|(_, json)| json.as_ref());
        assert!(yoru.is_some_and(|json| {
            json.get("sources")
                .and_then(Value::as_array)
                .is_some_and(|sources| !sources.is_empty())
        }));
        // Omen's 404 → a null payload, not an error.
        let omen = results
            .iter()
            .find(|(provider, _)| provider.name == "Omen")
            .and_then(|(_, json)| json.as_ref());
        assert!(omen.is_none());
    }

    #[tokio::test]
    async fn retries_once_on_401() {
        let seed_page = format!(r#"{{"seed":"{GROUND_TRUTH_SEED}"}}"#);
        let fetcher = ScriptedFetcher::default()
            .page("/seed", 200, seed_page)
            .page("/cdn/sources-with-title", 401, "");
        let ctx = ctx_for(&fetcher);
        let store = SeedStore::new();
        let query = ProviderQuery {
            title: "Dune".to_string(),
            year: Some(2021),
            media_type: "movie".to_string(),
            tmdb_id: GROUND_TRUTH_TMDB,
            imdb_id: None,
            season_id: 1,
            episode_id: 1,
        };
        // `fetchProvider` propagates the 401 so the caller can react.
        let result = fetch_provider(&ctx, &store, PROVIDERS[0], &query).await;
        assert!(matches!(result, Err(FetchError::Http { status: 401, .. })));
        assert_eq!(fetcher.hits("/seed"), 1);

        // `fetchAllProviders` owns the recovery: it invalidates the
        // seed and retries once (a fresh `/seed` fetch) before giving
        // up with a null payload.
        let results = fetch_all_providers(&ctx, &store, &query).await;
        let yoru = results
            .iter()
            .find(|(provider, _)| provider.name == "Yoru")
            .and_then(|(_, json)| json.as_ref());
        assert!(yoru.is_none());
        // The initial fetch plus the post-invalidation re-fetch.
        assert!(fetcher.hits("/seed") >= 2);
    }

    #[test]
    fn urlencodes_query_components() {
        assert_eq!(urlencode("Dune: Part Two"), "Dune%3A%20Part%20Two");
        assert_eq!(urlencode("plain"), "plain");
    }
}
